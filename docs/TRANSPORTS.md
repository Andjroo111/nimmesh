# Transports

Where nimmesh can go beyond Bluetooth, and what each move costs.

Researched 2026-08-21. Every number here is either computed from a published formula
or cited to a source. Nothing is estimated by feel.

## The fact that governs everything

A signed Nimiq transaction is small enough to fit through almost every constrained
radio on earth **in one piece**.

From [`docs/PROTOCOL.md`](PROTOCOL.md), our own `txWire` payload:

| Form | Size |
| --- | --- |
| Basic transfer | **~139 B** |
| With a memo | **~205 B** |

Now put that against the maximum single-unit payload of every cheap transport worth
having:

| Transport | Max payload | 139 B fits | 205 B fits |
| --- | --- | --- | --- |
| Meshtastic packet | 237 B | yes, 98 B spare | yes, 32 B spare |
| Binary SMS (GSM 03.38) | 140 B | yes, **1 B spare** | no |
| Bluetooth LE (what we ship) | 256 B block | yes | yes |
| LoRa raw (SF11/250k) | 255 B | yes | yes |

Nothing needs chunking. Nothing needs reassembly. Nothing has a partial-delivery
failure mode. That is not a small detail, it is the entire reason this expansion is
cheap for us and expensive for everyone else.

### Why Bitcoin cannot do this

The two prior-art projects that put Bitcoin on Meshtastic, [btcmesh][btcmesh] and
[MeshtasticBitcoinCore_Bridge][btcbridge], both **chunk** the transaction across
multiple packets and reassemble at the far end. Two reasons compound:

1. A typical Bitcoin transaction is larger than a Nimiq one to begin with.
2. Both send the transaction as a **hexadecimal string**, which doubles the byte
   count before it ever reaches the radio.

We avoid both. Meshtastic's `PRIVATE_APP` port carries raw bytes, so 139 binary bytes
stay 139 binary bytes. One packet, delivered or not delivered, no half states.

This is the sharpest technical claim nimmesh has, and it is worth leading with when
talking to the Nimiq team.

## Candidates

Ranked by what they cost us against what they buy us.

### 1. Meshtastic over LoRa (recommended)

The obvious move, and the one issue #17 was already reaching for.

- **Range.** Kilometres instead of the 30-ish metres BLE gives us. Range is dominated
  by antenna and elevation rather than the radio.
- **Cost.** A Heltec V3 is [$20-30][devices]. The newer V4 pushes 28 dBm against the
  V3's 20 dBm, roughly 6x the radiated power, for a few dollars more.
- **Licence.** None. 915 MHz in the US is unlicensed ISM. No ham ticket, no callsign.
- **Community.** The largest LoRa mesh community there is, 100+ supported hardware
  variants, and thousands of nodes already deployed and visible on public maps.
- **Integration.** There is an official [`meshtastic` Rust crate][rustcrate] with
  serial and TCP transports on tokio. `nimmesh-core` is already Rust. `PRIVATE_APP`
  (portnum 256) is usable immediately without rebuilding any protobuf files.

**Airtime.** Computed from the Semtech AN1200.13 time-on-air formula for a 155 B
frame (139 B transaction plus 16 B Meshtastic header) at the LongFast preset
(SF11, BW 250 kHz, CR 4/5):

```
Tsym     = 2^11 / 250000        = 8.192 ms
preamble = (8 + 4.25) * 8.192   = 100.4 ms
payload  = 153 symbols * 8.192  = 1253.4 ms
total                           ≈ 1.35 s on air
```

Computed, not measured on hardware. Under an EU 1% duty cycle that is about 26
transactions per hour per node, which is far more than an offline payment mesh needs.
The US has no 1% cap, so the ceiling is higher again.

**Why it matters strategically:** every existing Meshtastic node is a potential nimmesh
relay after a firmware-free software change on the gateway side. We would not be
building a network, we would be joining one that already exists.

### 2. Binary SMS

Already scoped as issue #24, and the numbers are almost comic.

A single SMS carries [exactly 140 bytes][sms] of user data under 8-bit encoding. A
basic Nimiq transaction is 139 bytes. It fits with **one byte to spare**, no
concatenation, no 6-byte User Data Header eating into the budget.

This covers the case where there is cell signal but no data, which is enormously common
in rural areas, on prepaid plans, and during network congestion after a disaster. It
needs no hardware at all.

Caveat that must be stated plainly: the memo form at ~205 B does **not** fit. SMS is a
basic-transfer-only transport unless we accept splitting.

### 3. MeshCore

Same hardware as Meshtastic, different firmware, [60 seconds to reflash][hexaspot].

The routing model is better for large public networks. Meshtastic makes every
node a relay; MeshCore splits Companions from Repeaters and routes toward infrastructure
instead of rebroadcasting to everyone. One Austin community reported roughly 5 s across
a nine-hop route against 10-20 s for Meshtastic on comparable topology.

**Recommendation: support it, but second.** The community is much smaller and it
launched in early 2025. Because the hardware is identical, backing Meshtastic first
costs us nothing if we add MeshCore later. PotatoMesh, the dashboard in the screenshot,
already supports both, which is a useful signal about where the ecosystem is heading.

### 4. Reticulum

The most architecturally interesting option and the worst near-term fit.

Reticulum is a cryptography-first Layer 3 stack. A destination is a 16-byte hash derived
from an Ed25519 public key, and there are no IP or MAC addresses at all. One flat
address space spans LoRa, TCP, serial, and packet radio at once. It is philosophically closer to what
nimmesh is than Meshtastic is.

But it is a bigger conceptual lift, the community is far smaller, and it needs RNode
hardware. **Park it.** Revisit if we ever want one address space across every medium.

### 5. Nostr relays

Already scoped as issue #25, and cheap. Not a mesh transport, an *egress* transport:
publish the signed blob to relays, and any watcher broadcasts it. Zero hardware, global
reach, complements LoRa rather than competing with it. Our Noise layer already
references NIP-17 gift-wrap, so the vocabulary is familiar.

### 6. Ruled out

- **APRS and ham radio.** Needs a licence, so it fails the "easy" bar. Also, most
  amateur bands forbid encrypted content and arguably commercial traffic, which is a
  bad fit for payments.
- **Satellite.** Real but expensive, and the wrong shape for a community mesh.
- **Wi-Fi Direct / Aware.** Higher bandwidth than BLE but shorter range than LoRa, so it
  adds complexity without adding reach.

## There is already a mesh in the metro

This is the finding that changes the shape of the work.

[the local community mesh][kcmesh] is an existing community Meshtastic network with **60+ active
nodes**. Coverage runs across downtown, central and the east side, thins out north, south and
southwest, and follows a chain of outlier nodes along the interstate. They
run a Discord, recruit hosts for a backbone initiative on the metro edges, and do weekly
drone lifts to reach silent nodes. A separate [a MeshCore group][meshtastic] group exists too.

So the honest framing is not "build a LoRa mesh". It is **join the one already outside the
window**. One $30 radio puts a nimmesh gateway on a city-wide network that other people
already maintain, power, and extend.

Their published node settings, worth matching so we behave like a good citizen rather than
a chatty stranger:

| Setting | Value |
| --- | --- |
| MQTT | disabled |
| Role | Client, **not** Router & Client |
| Hop limit | 3 |
| Position / telemetry broadcast | 1 hour |

Their own recommended starting hardware runs from a ~$35 DIY build to a ~$70 LILYGO T-Echo,
with solar nodes at $100-200 for anyone mounting outside.

**One caution before we show up.** This is a community network built for off-grid messaging,
not a payment rail we get to annex. Riding it with a custom `PRIVATE_APP` port is polite and
invisible to their chat, but the courteous move is to say hello in their Discord first and
explain what the traffic is. A demo that shows a payment crossing their mesh is also a great
introduction, so the social path and the technical path point the same way.

## No computer is needed at either end

The question that decides the shape of the work, answered before any hardware was bought.

Meshtastic publishes a [BLE client API][bleapi] whose whole purpose is third-party clients.
One service, three characteristics:

| UUID | Role |
| --- | --- |
| `6ba1b218-15a8-461f-9fa8-5dcae273eafd` | the Meshtastic service |
| `f75c76d2-129e-4dad-a1dd-7866124401e7` | `toRadio`, write a ToRadio protobuf to send |
| `2c55e69e-4993-11ed-b878-0242ac120002` | `fromRadio`, read the next inbound packet |
| `ed9da18c-a800-4f66-a670-aa7547e34453` | `fromNum`, notify, read until caught up |

nimmesh already runs CoreBluetooth in the central role, so talking to a radio is the same
class of work the app does today. That means a phone can put a transaction onto a LoRa mesh
directly, with no Mac in the loop:

```
 phone (offline)  --BLE-->  radio A  --LoRa-->  the mesh relays
                                                     |
                    Nimiq  <--internet--  phone B + radio B
```

Both ends are phones. The mac-node can be the gateway instead if one should sit at home, but
it is a convenience, not a requirement.

**Two caveats, stated rather than buried.** BLE bonding needs a PIN: screenless boards default
to a fixed `123456`, which should be changed. And this verifies that the API is public and
documented, not that nimmesh's Swift drives it. That is what the first radio proves.

### What to buy

**2x [RAK WisBlock Meshtastic Starter Kit US915][rak], about $35 each.** One gateway, one
carried. the mesh supplies every hop in between, so a third node for multi-hop is unnecessary.

Chosen over the cheaper Heltec V3 deliberately, for four reasons. It ships **pre-flashed with
Meshtastic**, so there is no firmware step. The RAK4631 is an nRF52840 rather than an ESP32
and draws far less power in a pocket. It sits on Meshtastic's own recommended list, where the
Heltec V3 does not. And the box already contains both the LoRa and BLE antennas.

That last point carries more weight than its price suggests. A bare Heltec needs an antenna
bought separately, and the antenna is both the part people forget and the part that dominates
range.

Battery and case are not included and are not needed to start. The gateway runs on USB and the
carried node runs off any USB power bank for a day of testing.

## Recommendation

Add **one** transport next, and make it Meshtastic over LoRa. Then SMS, which is nearly
free given the size fit. MeshCore after, since the hardware is already bought by then.

The prerequisite work is not the radio, it is the seam. Today the BLE transport is
assumed throughout the core. A `Transport` trait with BLE as the first implementation
turns every future radio into an additive change instead of a rewrite.

### Build order

1. **Prove the fit in code.** A test module asserting real Nimiq transaction sizes
   against every candidate MTU, so a future protocol change that breaks single-packet
   delivery fails CI instead of failing in a field. No hardware needed.
2. **Extract the `Transport` seam.** BLE becomes one implementation of it.
3. **Meshtastic gateway.** The `meshtastic` Rust crate over serial, `PRIVATE_APP`
   portnum, raw binary frames. The mac-node is the natural host since it is already the
   gateway.
4. **Field test.** Two radios, one carried out of BLE range, transaction lands on chain.
5. **SMS gateway.** Issue #24, and by then the seam already exists.

## Sources

- [btcmesh, Bitcoin over Meshtastic (chunked hex)][btcmesh]
- [MeshtasticBitcoinCore_Bridge][btcbridge]
- [Meshtastic port numbers, PRIVATE_APP 256][portnum]
- [Meshtastic Rust crate][rustcrate]
- [Meshtastic hardware buyer's guide][devices]
- [MeshCore vs Meshtastic, same hardware different firmware][hexaspot]
- [SMS 140-byte limit under 8-bit encoding][sms]
- [PotatoMesh, federated Meshtastic/MeshCore dashboard, Apache 2.0][potato]
- [the local community mesh, the local network and its node settings][kcmesh]
- [Meshtastic BLE client API, service and characteristic UUIDs][bleapi]

[btcmesh]: https://github.com/eddieoz/btcmesh
[btcbridge]: https://github.com/BTCtoolshed/MeshtasticBitcoinCore_Bridge
[portnum]: https://meshtastic.org/docs/development/firmware/portnum/
[rustcrate]: https://crates.io/crates/meshtastic
[devices]: https://nodakmesh.org/meshtastic/devices
[hexaspot]: https://hexaspot.com/blogs/news/meshtastic-vs-meshcore-explained-same-hardware-different-firmware
[sms]: https://www.twilio.com/docs/glossary/what-sms-character-limit
[potato]: https://github.com/l5yth/potato-mesh
[bleapi]: https://meshtastic.org/docs/development/device/client-api/
[rak]: https://store.rokland.com/products/rak-wireless-wisblock-meshtastic-starter-kit
[kcmesh]: https://meshtastic.org/
[meshtastic]: https://meshtastic.org/
