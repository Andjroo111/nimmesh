# ADR-0011 — The RPC-trust model: verifier cross-read, content-hash bind, and independent reveal confirmation

- **Status:** accepted (G8 M5, 2026-07-09)
- **Context:** The G8 review's **M5** finding. The funding verifiers are correctly *fail-closed
  against a hostile counterparty's crafted wire* — the `FundingProof` is only a locating hint and
  chain truth is re-established (S1 closed). BUT each verifier then **trusts its own node's RPC**
  for the data that decides whether to move money: the confirmation depth (`head − block + 1`), the
  NIM tx's inclusion height, and the escrow's post-`withdraw` state. Because each party uses its
  **own** RPC, this is a *compromised-infrastructure* threat (a MITM'd / lying / buggy endpoint),
  not a counterparty attack — but it is load-bearing for real funds. Everything here is
  **testnet/Amoy only**; the mainnet remainder stays behind `docs/MAINNET-GATING.md`.

## 1. The threat (what a lying/compromised RPC can do)

| Verifier | Trusted-RPC input | Lie → consequence |
|---|---|---|
| `PolygonHtlcVerifier` / `AmoyHtlcSwapVerifier` | `head` (`eth_blockNumber`) → depth `head − block + 1` | inflate `head` → fake "deep" → the responder funds real USDC against a shallow/nonexistent escrow |
| `AmoyHtlcSwapVerifier` | `getSwap(id).state == Live` | fake a live escrow → same |
| `NimHtlcVerifier` | `getTransactionByHash(H)` → `block_number` | echo the queried hash with a fabricated height → fake "NIM funded + deep" → responder funds USDC against nothing |
| `LiveInitiatorSigner` reveal | the `withdraw(S)` **receipt** | fake a successful receipt → initiator floods `S` on the mesh while USDC never moved |

A single compromised endpoint each party relies on can therefore manufacture a one-sided loss.

## 2. The decision — defense-in-depth, all fail-closed

We do **not** assume a trusted RPC on testnet. Instead every gate re-establishes truth from an
*independent* source where one is available, and never *silently trusts* a single response.

### 2.1 Optional independent cross-read (the load-bearing part)

Each gateway-backed verifier takes an **optional** secondary read source
(`with_secondary(...)`). When configured, a depth is reported **only** when the second endpoint
agrees; disagreement reads `Absent`/too-shallow (never advance). When **not** configured, today's
single-RPC behaviour holds and the single-RPC trust assumption is documented loudly (this ADR + the
struct docs). Public testnets have several endpoints, so the rig/example wires a real second one.

- **NIM (`NimHtlcVerifier`):** the secondary must agree on the **inclusion block** of the
  content-bound tx (exact match — an included tx has one deterministic height); its `head` folds
  into a **conservative (min) depth**, so a primary inflating `head` cannot fake depth. Disagreement
  / unseen / hash-mismatch on either endpoint → `Absent`.
- **USDC (`PolygonHtlcVerifier`, `AmoyHtlcSwapVerifier`):** the two `head`s must agree within
  `HEAD_CROSS_TOLERANCE_BLOCKS` (12 — Amoy ~2 s blocks, honest endpoints track within a couple);
  the **conservative (min) head** drives the depth. `AmoyHtlcSwapVerifier` additionally **re-reads
  the winning escrow's state as still `Live` on the secondary** before recording it. Beyond
  tolerance, a secondary error, or a not-`Live` re-read → `Absent`.

Why the NIM and USDC cross-checks differ: the NIM verifier already reads the tx by its
**content-derived hash**, so an *exact inclusion-block* agreement is available and strongest there;
the USDC verifier finds the escrow by a **log scan** (no second by-hash read), so `head` itself is
the cross-checked quantity, hence the tolerance. Both are conservative and fail-closed.

### 2.2 NIM content-hash bind

The canonical NIM tx hash **is** `Blake2b-256(serializeContent)`. `NimHtlcVerifier` now
**recomputes** it from the decoded creation and requires the RPC-returned tx to report **exactly**
that digest before its `block_number` is trusted; a returned tx whose identity is not our content
digest reads `Absent`. The HTTP `get_transaction` reports the node's OWN hash faithfully (no
fall-back to the queried hash), so the bind reflects the node's real answer. This binds the returned
inclusion data to our decoded content **identity**; binding the inclusion **height** against a node
that echoes the right hash with a fake height is the §2.1 cross-read — the two compose.

### 2.3 Independent reveal confirmation (M5 × M3)

The initiator no longer treats a `withdraw(S)` **receipt** as settlement on its own. Before the
reveal wire is handed to the mesh it independently **re-reads `getSwap(swapId).state == CLAIMED`**
(and only then, held until the withdraw is buried past the M3 reveal depth — ADR-0004/the burial
hold, not regressed). A faked receipt no longer floods `S`; the escrow must actually read `CLAIMED`
on-chain. With a secondary configured this re-read is cross-checked too.

### 2.4 `word_u64` overflow guard (LOW)

The USDC verifiers decoded 32-byte ABI words to `u64` by copying the low 8 bytes; a `> 2^64` word
would silently truncate. `word_u64` now asserts the high 24 bytes are zero and returns `Option`; an
over-`u64` amount/timelock/state reads `None`, so the log/state is skipped (fail-closed). No honest
`NimmeshHtlc` emits such a word — defense-in-depth only.

## 3. Proof

- **Offline, lying-RPC unit tests** (mock seams that lie about head/depth and return a
  mismatched content-hash / fake block):
  - NIM content bind — a foreign reported hash + real-looking height → `Absent` even against a
    fully-funded contract; the honest matching hash → `Found`.
  - NIM cross-read — a secondary that hasn't seen the tx / disagrees on the inclusion block →
    `Absent`; agreement → `Found`; a primary inflating `head` is defeated by the conservative
    secondary head (depth reads shallow → gate refuses).
  - USDC cross-read — head disagreement beyond tolerance / a secondary error / a secondary escrow
    re-read that is not `Live` → `Absent`; within-tolerance agreement uses the conservative head.
  - `word_u64` — accepts a domain word, refuses an over-`u64` word (direct + behavioral).
  - Reveal confirmation — a successful `withdraw` receipt whose escrow does NOT read `CLAIMED`
    withholds `S`; `CLAIMED` + buried → the reveal is released (M3 preserved).
- **Live, read-only** (`examples/live_rpc_cross_read.rs`, no key/funds): two GENUINELY independent
  public Amoy endpoints (`rpc-amoy.polygon.technology` + `polygon-amoy-bor-rpc.publicnode.com`)
  agree on `head` within tolerance → the verifier trusts the conservative depth. Receipts in
  `docs/swap/M5-RECEIPTS.md`.

## 4. What mainnet additionally requires (OUT of scope here — needs:owner)

M5's *testnet* hardening is the above (the seam + binds + reveal confirmation). Mainnet additionally
needs, in the gated guard-lift PR (never auto-merged):

1. **A trusted / self-hosted endpoint actually wired** as the live secondary. A second INDEPENDENT
   NIM **testnet** RPC is already a self-hosted node (only `rpc.testnet.nimiqwatch.com` is public),
   so the NIM cross-read is *seam-ready* but not exercised against two public NIM endpoints today;
   on mainnet the secondary should be a node the operator controls, not two third-party endpoints.
2. **M6 — mainnet confirmation-depth retune** (ADR-0003): Amoy/testnet depths are NOT reorg-safe on
   a mainnet chain; a live secondary agreeing on a *shallow* head is still shallow.
3. The **guard-lift** itself (`guard_testnet`/`guard_amoy`, `fund_nim`, the NIM-verifier network
   pin, `HttpGatewayRpc::new_mainnet` / the polygon mainnet allow-list) — its own dedicated review.

M5 and M6 are explicitly the two mainnet-only fixes the G8 review flagged; this ADR closes the
*testnet-buildable* half and hands the rest to `docs/MAINNET-GATING.md` §8.2/§8.4.
