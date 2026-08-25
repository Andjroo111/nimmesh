# LoRa gateway: bridging NIMmesh onto an existing Meshtastic mesh

The design for issue #6. [`TRANSPORTS.md`](TRANSPORTS.md) established that this is the
right next transport and why (kilometres instead of room scale, ~$35 per radio, a
community mesh already outside the window). This doc pins down the protocol decisions
that document deliberately left open: what exactly crosses the radio, how dedup and
store-and-forward survive the hop, and what the gateway software looks like.

No hardware has been driven yet. Everything below is computed from the wire formats in
this repo and the published Meshtastic specs cited in `TRANSPORTS.md`; the numbers that
matter are proposed as CI assertions so the implementation cannot silently drift. And
the standing honesty note applies unchanged: no two phones have exchanged a byte over
*any* radio yet (issue #3 is the BLE field test) — this design queues behind it.

## The core decision: what crosses the radio

The naive answer — put the whole nimmesh packet on the LoRa frame — does not fit.
A nimmesh BLE frame is PKCS#7-padded to **256 bytes** minimum (PROTOCOL.md), and one
Meshtastic packet carries **237 − 16 = 221 usable bytes** (the repo's
`transport_mtu_tests` constants). So the bridge must unwrap.

Strip the padding and the mesh header, and what must survive is:

1. **The payload** — the Nimiq TLV envelope, verbatim. This is the money-carrying part
   and it is already transport-independent.
2. **The relay identity.** nimmesh dedup and store-and-forward key every packet by
   `(msgType, senderID, timestamp)` (`engine::relay_key`). If the bridge drops these,
   the same transaction re-entering BLE on the far side gets a *new* identity: dedup
   breaks, the G7 gossip-sync offers it to peers that already hold it, and two bridge
   gateways hearing the same LoRa frame would inject two "different" packets.
3. **The TTL** — the remaining hop budget, so a transaction cannot ping-pong between
   bridged mesh islands forever.

### The bridge frame

```text
version(1)=1 | type(1) | ttl(1) | timestamp(8, ms) | senderID(8) | payload(…)
```

19 bytes of prelude — exactly the original packet's header fields minus the two the
bridge recomputes (`flags`, `payloadLength`) — then the untouched TLV envelope. On the
far side the gateway reconstructs a canonical nimmesh packet from these fields, and by
construction its `relay_key` is **identical** to the original. Dedup, the recent-packet
store, and gossip-sync then work across the hop with zero new machinery:

- two bridge gateways hearing the same frame inject byte-identical packets → the second
  one dies in `relay_seen`, exactly like a duplicate BLE flood;
- a phone that was offline during the flood still gets the tx from its gateway's G7
  store-and-forward, because the reconstructed packet is remembered like any other;
- the TTL keeps decrementing across the bridge, so hop budget is global, not per-island.

Meshtastic's own packet id / hop machinery is *not* reused for nimmesh dedup — the
radio is transport, not protocol. Its job ends at delivering the frame.

### Size budget (computed from the TLV encoder, to become CI assertions)

| envelope | TLV bytes | + prelude 19 | fits 221? |
| --- | --- | --- | --- |
| required only (txWire 139 + networkId) | 144 | 163 | yes, 58 spare |
| full basic (+ validUntil, txId, wantReceipt) | 187 | 206 | yes, 15 spare |
| memo form (txWire ~205 + networkId) | ~210 | ~229 | **no** |

Two consequences, stated rather than discovered in a field:

- **A basic transfer bridges with every optional TLV intact.** No feature loss.
- **Memo transactions do not bridge in v1.** They stay BLE-side; the gateway drops them
  from the bridge queue and (locally) logs why. If memo bridging is ever wanted, the
  `encMemo` TLV is the one to strip — the memo is end-to-end encrypted convenience, not
  money — but that is a later, explicit decision.

Proposed additions to `transport_mtu_tests`: `a_full_basic_envelope_plus_bridge_prelude_fits_one_meshtastic_packet`
(the 206 ≤ 221 row, from real encoders, not constants) and a companion asserting the
memo form does **not** fit, so the exclusion is documented by a red bar the day the
sizes change.

Small non-money packets (`0x31` receipts, `0x32` head beacons, `0x33`/`0x34` balance
queries/responses) all fit trivially and bridge the same way. Fragmented payloads
(`0x36` history, the coming `0x37` balance proof) are **not bridged in v1**: each
fragment would fit, but reassembly across a lossy multi-hop LoRa path with no ACK is a
different reliability problem, and nothing money-critical rides fragments.

## Portnum

Meshtastic reserves portnums **256–511 for private applications**, with `PRIVATE_APP =
256` as the conventional default (the portnum doc cited in `TRANSPORTS.md`). The bridge
uses **256**, as one named constant, with a config override. Rationale: any other value
in the range is equally unregistered, so picking an exotic number buys no collision
protection — actual disambiguation comes from the first prelude byte (`version = 1`),
which lets the gateway drop foreign `PRIVATE_APP` traffic in one comparison, plus the
strict TLV decode behind it (hostile-input discipline already in the codec). If the
project ever matters enough to register a public portnum upstream, that is a one-line
change.

Frames are sent as **broadcast** on the mesh's primary channel, not direct messages:
the far gateway's position is unknown by design, Meshtastic broadcast needs no ACK
(delivery confirmation is nimmesh's own `0x31` receipt coming back the same way), and
broadcast is what the flood model expects.

## Topology and roles

```
phone (offline signer)
   │ BLE (existing transport)
gateway host + radio A          ── LoRa, portnum 256 ──   community mesh relays …
   (bridge: BLE ⇄ LoRa)                                        │
                                              radio B + gateway host (internet)
                                                 │ existing G8 RPC path
                                              Nimiq testnet
```

- **The bridge is a role of the existing gateway node**, not a new binary: a
  `MeshGateway` host that additionally owns a radio. The mac-node is the natural first
  host (it already runs headless), via the official `meshtastic` Rust crate over
  serial/TCP — `nimmesh-core` is already Rust.
- **A phone can be the bridge host too** — Meshtastic's BLE client API is public and
  the Android app already runs the BLE central role — but that is phase 2: it needs the
  phone to hold two BLE links (mesh peers + radio) and adds nothing to the protocol
  design.
- The radio is **transport only**. Keys never leave the phone; the bridge carries
  already-signed, broadcast-safe bytes, the same non-money-path contract as every relay
  (GOAL.md core value #1).

## Validity-window hygiene at the bridge

The bridge applies the same guard the engine already applies (`beacon::is_expired`)
before spending airtime: a transaction whose validity window has closed against the
gateway's freshest head is dropped, not bridged. LoRa airtime is the scarcest resource
in the whole system (~1.35 s per frame at LongFast, per `TRANSPORTS.md`) — a dead tx on
the radio is strictly worse than a dead tx on BLE.

Rate limiting mirrors the beacon scheduler pattern: token-bucket per packet type,
beacons at most one per bridge per few minutes (the mesh does not need a 10 s cadence
at kilometre scale), transactions effectively unlimited (26/hour even under an EU duty
cycle is far above any realistic offline payment volume).

## Etiquette on someone else's mesh

Unchanged from `TRANSPORTS.md` and worth restating as design constraints, not vibes:
match the community's published node settings (Client role, hop limit 3, MQTT off,
telemetry at 1 hour), say hello in their channel before the traffic shows up, and honor
that a payment bridge is a guest on a messaging network. The bridge's rate limits above
are sized so nimmesh traffic is invisible next to normal chat.

## Build order (revised from TRANSPORTS.md step 3)

1. **Bridge frame codec in `nimmesh-core`** — encode/decode + the two new MTU
   assertions. Pure Rust, no hardware, testable now.
2. **Bridge loop in the mac-node** — `meshtastic` crate, serial first; BLE-side is the
   existing engine. Behind a config flag, off by default.
3. **Bench test with two radios, no mesh** — tx signed on phone A → BLE → radio A →
   radio B → gateway → testnet. The LoRa twin of issue #3.
4. **The community mesh** — after the hello, the same test across town.

Steps 3–4 are where every claim above stops being computed and starts being measured.
