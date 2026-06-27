# nimiq.nimmesh — Offline-Payment Risks & Mitigations

Untrusted mesh input is **hostile by default**. These constraints shape the
architecture; the loop must honour them. Severity = high / med / low.

## Reusable fleet seam (Part A)

The fleet has already factored the exact split nimmesh needs — **sign offline (pure
crypto) / broadcast online (plain JSON-RPC)** — across `sendhome`, `nimiq.sale`,
`nimiq-app-shell`, and `nimiq.win`:

- `sendhome/src/chain/nimiq-signer.ts` — offline signing (content bytes → proof, no key egress).
- `sendhome/src/htlc/wire.ts`, `hex.ts` — deterministic Nimiq serialization, byte-exact.
- `nimiq.sale/src/payments/nimiq-rpc-client.ts`, `nimiq-provider.ts` — `sendRawTransaction`
  broadcast + `detected → paid` confirmation staging.
- `nimiq-app-shell/src/wallet/*` — Hub / mini-app signing backends (returns a serialized blob).

The nimmesh **`MeshPayment` / `MeshGateway` / `MeshTransport`** interfaces mirror the
fleet `ChainProvider` `kind: mock | real` pattern, so the whole loop runs against a
`MockMeshTransport` + mock RPC in CI before any radio exists.

## Risk ledger (Part B)

### 1. Validity-window expiry — **high**
A signed tx dies on the mesh if not broadcast within its window. `VALIDITY_WINDOW =
7,200 blocks ≈ 2 h`; past it every honest node drops it. **This single constraint most
shapes the design.**
**Mitigation:** set `validityStartHeight = latest-known head` (claim full forward ~2 h,
never pre-date); beacon head height (`0x32`) so deep-offline signers anchor fresh; stamp
`validUntilMs`, set BLE TTL = `min(remaining window, hop budget)`, GC expired packets;
refuse to sign when the remaining window is already too short to plausibly relay (mirror
`sendhome/grace.ts` hard-floor). On expiry the only recovery is **re-sign** — say so in UX.

### 2. Double-spend offline / deferred broadcast failure — **high**
The offline signer has no live balance or nonce check. But Nimiq is **account-based with
explicit nonce + validity window**, so two offline txs from one account in overlapping
windows **cannot both be included** — nimmesh **cannot create an on-chain double-spend.**
The real hazard is a *deferred failure* shown to a RECEIVER who already accepted payment.
**Mitigation:** every offline-accepted payment is **unconfirmed-until-inclusion** —
show "pending — not yet settled", flip to "paid" only at confirmation depth (reuse
`nimiq.sale` `detected → paid` + `confirmStillIncluded` re-verify). Enforce
**one-signed-tx-per-account-window** discipline on the phone. Risk-tier by amount: small
tickets may accept-on-relay; large require online confirmation before goods move.

### 3. Insufficient funds discovered only at broadcast — **med**
Same root as #2; the offline signer can't see balance, so an overspend is accepted
locally and only fails at the gateway's RPC.
**Mitigation:** cache last-known balance on the phone and refuse to sign above it offline
(best-effort, not a guarantee). Never show "paid" until inclusion. On RPC error, relay a
**signed NACK** ("failed: insufficient funds / expired") back over the mesh to both
parties (reuse `nimiq.sale` `onExpired` `pending → expired` shape).

### 4. Mesh spam / DoS / battery drain / metadata privacy — **med**
Hostile input (cf. *Breaking Bridgefy*: one crafted message could down the network;
trackable IDs leak the social graph). A Nimiq tx also exposes sender/recipient/value/
extraData in cleartext, so flooding it tells everyone in range who-paid-whom-how-much.
**Mitigation:** **free spam filter** — verify `SignatureProof` + `tx.verify(networkId)`
before relaying; drop anything not a well-formed signed Nimiq tx. Dedup + TTL + hop-cap;
stop forwarding once a gateway-ACK hash is seen; per-peer relay rate limits; adaptive
duty-cycling for battery. Privacy: only opaque references in `extraData` (never PII),
optionally encrypt the in-flight envelope to the gateway via Noise.

### 5. Key / seed handling on the phone — **med**
Risk of the seed leaking into the mesh, logs, or a compromised relay.
**Mitigation:** **non-custodial by construction** — master seed stays in the OS secure
enclave (or Nimiq Pay / Hub / Keyguard); the signer takes content bytes and returns a
proof, never a key. **Only** public, broadcast-safe signed hex touches Bluetooth. Support
both origins: full wallet via Hub / mini-app SDK, or an app-scoped `EphemeralKeySigner`
derived via HKDF.

### 6. Replay / duplicate broadcast — **low**
The same signed tx flooded many times and submitted by multiple gateways.
**Mitigation:** confirmed **idempotent and desirable** — tx hash is deterministic, the
mempool dedups by hash, the chain enforces nonce + validity window, so re-broadcasting
identical bytes is a no-op ("already known"). Relay freely for redundancy; gateways
short-circuit on a hash already seen/included. The only non-idempotent danger is the
signer emitting **two different** txs for one intent — that is risk #2, not replay.
