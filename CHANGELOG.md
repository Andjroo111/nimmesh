# Changelog

All notable changes to nimiq.nimmesh. Each PR bumps the version and adds an entry.


## [0.88.0] — 2026-07-15

### Changed — sub-30s swap settlement: deterministic finality instead of depth-waiting (needs:owner, money-path)

The first real capped mainnet NIM⇄USDC swaps settled correctly but took ~3+ minutes: ~2-3 min
of that was WAITING — 64 Polygon blocks of probabilistic depth-count, 10 NIM blocks, and a
~15 s re-verify heartbeat between every step. This release replaces probabilistic waiting with
deterministic finality where the chain offers it, and re-checks faster. The fail-closed gate
itself (`require_funded`) is UNCHANGED — only what the verifiers report and when they run moved.
Full rationale: ADR-0003 addendum (2026-07-15).

- **Polygon `finalized`-tag verification.** Heimdall v2 (live 2025-07-10) gives Polygon PoS
  ~5 s deterministic milestone finality (reorgs capped ~2 blocks). Both USDC verifiers
  (`PolygonHtlcVerifier` + `AmoyHtlcSwapVerifier`) now read
  `eth_getBlockByNumber("finalized")`: an escrow at or below the finalized height reports
  `FINALIZED_CONFIRMATIONS` (= u32::MAX, "maximally buried"), clearing the depth floor at
  once. Every failure path — tag unserved, RPC error, escrow above finality — falls back to
  the exact pre-existing depth count (slower, never weaker). Under the M5 cross-read, BOTH
  endpoints must vouch (min of the two finalized heights, capped at the cross-checked head);
  lying-RPC tests prove a dishonest primary cannot fake finality.
- **Mainnet depths retuned (≤ $5 envelope):** NIM 10 → **2** (Albatross BFT: micro-block
  reorgs need a slashable equivocation proof; ~2 s), USDC 64 → **8** (fallback only — the
  finalized tag is the primary signal), BTC 2 unchanged. The old NIM 10 / USDC 64 profile
  survives verbatim as `ConfirmationPolicy::mainnet_paranoid()` — a named one-line revert.
- **Fast in-flight tick (~3 s).** New `MeshNode::poll_swap_fast()` FFI: the iOS bridge runs a
  native 3 s timer while a swap sheet is live (mac-node rides its 2 s beat); the worker job
  re-runs ONLY the funding re-verification + next money-path step. Retransmit (TTL-32 ≈ 8 min
  budget), GC, gossip-sync, beacons, and the match window all stay on the ~15 s cadence —
  and the fast poll is idle-free + rate-limited core-side so it cannot hammer the shared-IP
  RPC. Deterministic fence tests (ADR-0005).

### Added — the "settled in Xs" stopwatch

The swap mirror / `FfiSwapMatch` now carry `started_at_ms` + `settled_in_ms` (the
verify-note telemetry pattern; wall-clock display only, consensus stays head-anchored), and
the swap sheet renders "Swap settled · settled in X.Xs" (en/es/de/fr/pt) — the settlement
budget is a number on the phone after every swap.

Expected budget (typ.): USDC burial 2-3 min → ~5-10 s; NIM burial ~10 s → ~2 s; re-verify
beats 15 s → 3 s. Total ~3+ min → **~20-30 s**.

## [0.87.0] — 2026-07-15

### Fixed — a failed CSPRNG in the app handed the swap an all-zero seed, and every door accepted it

G11 (#82). The live FFI doors validated `intent_seed` / `nim_claim_seed` for **length only**. That
seed is doubly load-bearing: it is the PRF master every per-swap secret `S` derives from, and (via
`InMemoryEnclaveKey::from_secret`) the Ed25519 key that *is* the node's swap identity.

`SwapMesh.swift` drew it with `_ = seed.withUnsafeMutableBytes { SecRandomCopyBytes(...) }` — the
`OSStatus` **discarded**, over a zero-filled `Data(count: 32)`. A failing RNG therefore left the
seed all zeros, silently. (`Mnemonic.swift`, the wallet path, `precondition`s the identical call;
only the swap money path dropped it.) With a zero seed every `S` is `sha256(master ‖ swap_id ‖
label)` over a *publicly known* master, and `swap_id` travels the wire in cleartext — so any relay
that saw a Propose could recompute `S` and claim the counterparty leg first. That is **S1, the
CRITICAL theft this agenda exists to close, reopened through the app's RNG**.

Nothing downstream caught it. Every 32-byte value is a valid Ed25519 seed (the secp256k1 secrets
were protected only by the accident that zero is not a valid scalar — which does not cover a
stuck-byte value either). And `live_safety`'s C1 gate passes: a zero-seeded PRF still counts as
"not the sim source", so `secret_is_sim` flips false. C1 checks **that** you replaced the sim
secret source, not that what you replaced it with carries any entropy.

- **New `swap_secret` module** — one auditable entropy inventory (mirroring `ffi_secret_redaction`):
  the entropy gate, the OS-CSPRNG drawer, the per-swap secret PRF, and `sim_secret` (moved here from
  `swap_node`, name unchanged — the OWNER-GATED doclint pins it; the move also brought `swap_node`
  back under the 800-line ceiling it was sitting exactly on).
- **`check_seed_entropy` at every live door** — Rust no longer trusts the app's RNG, the same way
  G1 taught it not to trust a peer's message. In `swap_live_ffi_live_impl` the gate sits inside
  `seed32`, the single chokepoint all four secrets funnel through, so a new secret field inherits it.
  It is a health test (NIST SP 800-90B-style repetition/stuck-output), **not** an entropy estimator:
  it catches the canonical catastrophic failures (unwritten buffer, latched RNG, hand-typed
  placeholder), not a weak-but-well-formed PRNG.
- **`drawSwapSeed()` over UniFFI + Swift uses it** — the actual fix, removing the failure mode by
  construction rather than detecting it. A failing RNG now aborts instead of returning zeros.
- **The PRF recipe is defined once** — the two live doors carried copy-pasted twins of it.
- Test suites no longer seed themselves with `[7; 32]` / `[0x5A; 32]` — those are exactly what the
  gate refuses. `swap_secret::test_seed(tag)` gives deterministic, gate-clearing seeds.
- `getrandom` is now a direct (non-optional) dependency; `x25519-dalek` already linked it
  unconditionally, so this adds no dependency or portability surface.

See [ADR-0013](docs/adr/0013-swap-entropy-gate.md).

## [0.86.1] — 2026-07-15

### Fixed — the never-strand refund sweep never ran, and could not have refunded a mainnet lock

Andjroo's 20 NIM from the two stalled 2026-07-14 mainnet swap runs sat in expired HTLCs
(`NQ85 SECV…XHHN`, `NQ19 X84E…CGRV`, 10 NIM each) after he opened the Swap sheet with the
real toggle on — "nothing happened. Still the same amount." Three stacked defects:

1. **Nothing ever invoked the sweep.** `swapMeshRefund` existed on the bridge (G10b), but no
   webui code called it — the "sweep on open" the checkpoint promised was never wired.
2. **The refunder was testnet-pinned twice.** The Swift sweep passed the testnet RPC url, and
   the core `NimHtlcRefunder` hardcoded `NetworkId::Testnet` in both its RPC guard and the
   refund tx's `network_id` — a mainnet refund built here would be unrelayable.
3. **A wrong-chain read could forget a mainnet lock.** Querying a mainnet HTLC address on the
   testnet chain reads "no account" → balance 0 → `AlreadyResolved` → the Swift sweep dropped
   the lock from persistence while 10 NIM stayed locked on mainnet. (Bug 1 is the only reason
   bug 3 never fired.)

Fixes, each proven by a test or the Playwright harness:

- Core: `NimHtlcRefunder` is network-aware (`from_parts_on`) and stamps the refund tx with its
  OWN network's wire id; new `NimHtlcRefunder.newMainnet` FFI constructor over
  `HttpGatewayRpc::new_mainnet`. Byte-exact mainnet-wire test + testnet-url refusal +
  bindings-parity refusal. **Deliberately NOT behind the `mainnet_swap` arming switch**: a
  `TimeoutResolve` can only pay the HTLC's own sender (chain-enforced — this wallet), so the
  never-strand door reduces exposure and must stay open even if a later build disarms.
- Swift: the sweep probes BOTH chains per lock (a lock record carries no network) and forgets
  a lock ONLY when every probed chain reads the contract empty; any broadcast, still-locked,
  or error keeps it. `AlreadyResolved` docs now state the caller contract explicitly.
- webui: the sweep actually runs — at boot (~4 s after launch) and on every Swap-sheet open —
  with an honest gold one-liner (`#swap-refund-note`, i18n ×5) walking
  "Refunding locked NIM to your wallet…" → "Locked NIM refunded ✓", re-sweeping every 20 s
  while anything is pending so a broadcast is chain-confirmed before the record is forgotten.

## [0.86.0] — 2026-07-15

### Fixed — the FFI configs printed their private keys (G11 / #82)

Four `uniffi::Record` config types take raw key material into the core, and every one of them
**derived `Debug`** — so `{:?}`, `dbg!`, a Swift `String(describing:)`, or a UniFFI panic in a
crash report rendered the raw bytes:

| Record | Leaked | What it controls |
|---|---|---|
| `FfiLiveInitiatorConfig` | `intent_seed`, `evm_gas_secret` | the master for **every** per-swap secret `S`; a spendable Amoy account |
| `FfiLiveResponderConfig` | `nim_claim_seed`, `evm_funding_secret` | the key that **redeems the NIM HTLC**; the **funded** account escrowing the USDC |
| `FfiParticipantConfig` | `intent_seed` | ephemeral identity + secret master |
| `FfiUsdcSendConfig` | `source_secret` | the account holding the USDC |

`intent_seed` is the sharp one: it is the master for the per-swap secret PRF, so a seed in a log
lets an attacker derive `S` for every swap that node will ever run and pre-claim the counterparty
leg — the theft S1 closed on-chain, reopened through stdout. Logs are not a trust boundary.

The derives are replaced by hand-written `Debug` impls in a new `ffi_secret_redaction` module: a
key-material field renders as `<redacted 32 bytes>` (length only — never a prefix, never a hash),
public fields render normally so a bad url or amount is still debuggable. They live together as the
one auditable inventory of what is secret at the door, and the sibling test suite asserts the
rendering never contains what a derive would print — so re-deriving `Debug` on any of them fails
CI instead of silently reopening the leak. `{:#?}` is covered too (it is what panics use).

No ABI change — the record fields and generated bindings are untouched. Decision + what this
deliberately does NOT cover (the seed still crosses FFI inbound; no zeroize; entropy still
caller-supplied) in [`ADR-0012`](docs/adr/0012-ffi-secret-redaction.md).

## [0.85.0] — 2026-07-15

### Added — stop advertising a swap intent without tearing the node down (G9 / #80)

The discovery FFI could START a standing swap advert (via the `new_swap_participant`
constructor) and READ match/metric state (`active_swaps` / `discovery_metrics`), but the only
way to STOP advertising was `shutdown()` — which destroys the whole node and drops any in-flight
swap. The app worked around it by shutting the node down and rebuilding a fresh one, a coarse
teardown for what should be a "stop looking for a counterparty" toggle.

This adds `MeshNode::stop_advertising()`: a runtime withdraw of the standing discovery intent
while the node — and any swaps already in flight — keep running. After it, the maintenance tick
finds no standing intent to re-flood (`readvertise_intent`) and a crossing complementary intent
no longer originates a fresh swap (`handle_intent`); existing coordinators run to completion and
the node keeps relaying.

- New `Job::StopAdvertising` worker command + `SwapSession::stop_advertising()` (takes the
  `standing_intent` out of the node identity). Non-blocking; a no-op on a relay node with no advert.
- Regression test `stop_advertising_withdraws_the_standing_intent`: proves a withdrawn intent is
  never re-advertised again, asserted while the re-advertise budget is still unspent — so the halt
  is provably the withdrawal, not the natural G37 re-advertise cap.

Bindings regenerated (Swift). Follow-up (deferred): a symmetric runtime `advertise_intent(...)` that
re-signs a fresh standing intent through the session's enclave identity key (no seed re-crossing
FFI) — it intersects the G11 enclave seam and lands in its own slice.


## [0.81.0] — 2026-07-14

### Fixed — a stalled swap now re-checks funding on the clock, and shows the verifier's verdict

The first real mainnet phone↔phone swap ran discovery → Propose → Accept cleanly (0.80.3's
peer-replay fix), the initiator FUNDED its NIM HTLC on mainnet for real (10 NIM, sha256/
hash-count-1, correct shape, 290+ confirmations) — and then the responder NEVER funded its
USDC leg. Stuck at "NIM HTLC funded" indefinitely, two runs in a row.

ROOT CAUSE: the responder verified the initiator's NIM HTLC on-chain ONLY when a
`FundingProof` MESSAGE arrived. That proof lands seconds after funding, when the HTLC is 1-2
confirmations deep — below the mainnet policy's 10 — so the verify correctly failed
`TooShallow` and the responder stayed at Accepted. Recovery then depended entirely on a
RETRANSMITTED proof arriving AFTER depth 10 was reached. But the retransmit budget is bounded:
`RETRANSMIT_TTL = 32` re-floods, and the maintenance tick that drives them rides the 15 s
keepalive beacon — so retransmits run out after ~8 minutes (32 × 15 s). The house shares one
public IP and the NIM RPC rate-limits per IP, so during that window the responder's chain
reads kept failing closed (RPC error → Absent → not-funded-yet) instead of confirming depth.
Once the 32 retransmits were spent, the message-only path had NO way left to advance — even
though the HTLC was, by then, 290+ blocks deep. Permanent stall. The USDC mirror is worse by
construction: the counterparty leg needs 64 Polygon blocks (~2 min), and the initiator that
must confirm it before revealing `S` had the same message-only fragility.

FIX (adds NO new trust — a clock instead of a message): on every maintenance tick, for each
swap sitting in a phase that awaits counterparty funding AND for which a `FundingProof` has
already been received (so the verifier holds its locating hint), re-run the EXACT same
fail-closed `verify_and_observe_funding` gate — same per-chain depths (NIM 10, USDC 64), same
refusal semantics. A swap now advances the instant its counterparty HTLC reaches its required
depth, with no dependence on a retransmit landing inside the bounded window; the tick then
drives the next money-path step (a responder funds its USDC/BTC leg; an initiator reveals
`S`). Gated on a received proof so a tick never polls the rate-limited chain for a swap whose
counterparty hasn't claimed to have funded. Deterministic tests (ADR-0005 fence, no
wall-clock): the responder advances on the next tick after depth 10 with no message; the
initiator's mirror advances after USDC depth 64; an absent/mismatched hint refuses across 128
ticks (no weakening).

### Added — the verifier's live verdict, surfaced in the swap sheet

Andjroo asked to SEE the verifier's verdict instead of guessing. The session now records the
LAST counterparty-funding verdict per swap — the Ok/advanced case and each refusal (not-funded
-yet, `TooShallow{have,need}`, mismatch, under-funded, timeout-too-short) with a climbing
attempt counter and a telemetry timestamp — mirrored over FFI (`FfiSwapMatch.verify_note` +
`verify_attempts` + `verify_at_ms`) the same way the phase mirror works. The swap sheet paints
it as a small mono diagnostic line under the phase timeline (the `ble ▸` precedent), e.g.
`verify ▸ NIM too shallow 3/10 · attempt 4 · 12s ago` / `verify ▸ verified — funding USDC`.
Pure telemetry — nothing in the swap state machine reads it, and it never authorizes a
transition.

Internal: the observable mirror (`sync_swap_phases` + `active_swaps`) moved to a new
`swap_mirror` module (both files were at the 800-line ceiling); `drive_swap`'s phase-action
tail was extracted to a reusable `drive_phase_action` the tick path shares.

## [0.80.3] — 2026-07-14

### Fixed — a node swap now inherits the peers that are already connected

The first live mainnet swap attempt sat at "Listening for a swap offer on the mesh · 0
peers" with both phones linked. ROOT CAUSE: `BleMeshRadio.linkUp` announces a peer to the
node only on its FIRST link (the two directed BLE links per pair are ref-counted), and
`linkCount` lives on the RADIO, which outlives any node. Opening the Swap sheet REPLACES
the node (wallet node → swap participant), so a peer that linked before the swap was never
announced to the new node: the radio stayed linked, the new node saw nobody, and discovery
could never start. Same family as the v0.51.5 weak-`node` bug — the radio counted a peer
the Rust node never did.

Fix: `BleMeshRadio.node`'s `didSet` replays every live link onto the incoming node (on the
radio queue; idempotent — `add_peer` is a set insert; a no-op on first launch). Restoring
the wallet node on sheet close inherits the links the same way.

## [0.80.2] — 2026-07-14

### Fixed — the Send sheets use the wallet's REAL components (Andjroo's round-4 catch)

He asked directly whether the references were consulted — they weren't, fully: BOTH send
sheets (NIM and USDC) carried a hand-drawn Contacts icon, the USDC sheet lacked the
wallet's header back arrow, and the ENTER ADDRESS section sat too high. Now: the verbatim
`@nimiq/style` `contacts.svg` (filled card, knocked-out person; 48px at opacity .4 — the
wallet's `.nq-icon` treatment) in both sheets, the verbatim `arrow-left.svg` back button
(amount → address → close), a taller contacts block, and ENTER ADDRESS ~40% down the
sheet — all measured off his 12 Pro capture of the real wallet, render-vs-capture
compared by eye before shipping.

## [0.80.1] — 2026-07-14

### Fixed — the Send USDC address box now matches the wallet's

Andjroo's third-round catch on the Send screen: the address entry was a full-width blank
rectangle; the real wallet's is a narrow centered box (~54% of the sheet) whose three text
rows sit on TWO inset ruled lines, and its blue note breaks "Send to Polygon USDC /
addresses only!". Both matched to his capture by direct render comparison (measured
geometry, two explicit rule gradients — not a repeating one, which painted a third rule).

## [0.80.0] — 2026-07-14

### The REAL USDC send + the wallet-faithful send flow (fixes #219's send modal)

Andjroo: "The send screen is not correct." #219 shipped a Send modal that (1) had a Send button on
the address step where the real wallet has none (a valid `0x` auto-advances), (2) rendered a grey
info banner the real wallet doesn't have, and (3) drew the disabled Send in the wallet's light-blue
CTA gradient, so it looked enabled. This ships the real send AND rebuilds the flow to the capture
(`~/.nimmesh-refs/usdc-view/real-wallet-send-usdc-modal.png`).

**The real send (money-path, user-initiated on device — the same trust model as the NIM
`sendTransaction` bridge; nothing sends autonomously).**
- New Rust FFI **`send_usdc_mainnet`** (`usdc_send_ffi.rs`): builds an ERC-20 `transfer` calldata
  (new `evm_abi::erc20_transfer`, selector `0xa9059cbb`, cast-anchored), a `LegacyTx::polygon_mainnet`
  (chain id 137), signs with the caller's derived secp256k1 secret (the evm signer the swap legs use),
  and broadcasts via `HttpPolygonRpc::new_mainnet` (the allow-listed cross-read client). Gates baked
  in: refuses unless `mainnet_swap_armed()` (the #213 predicate), refuses a zero/malformed recipient
  or amount, refuses when `amount > balanceOf(sender)`, and refuses when the sender's POL can't cover
  gas. The token is the code-pinned native Circle USDC, never caller-supplied. The secret enters the
  core once and never crosses back; only the tx hash + public sender return. No cap beyond balance
  (the user sends their own funds, like a NIM send).
- **Gas (measured).** A live read-only `eth_estimateGas` for a native-USDC `transfer` returned
  `0xee49` = **61 001** gas (a fresh-recipient SSTORE pushes toward ~85 k). The send estimates via
  the new `eth_estimateGas` codec + a 25 % buffer (capped at 200 k), falls back to a fixed **120 000**
  limit when the estimate is unavailable, and clamps the node's gas-price suggestion into **[30, 100]
  gwei** (a live read saw ~280 gwei — Polygon spikes are real).
- **Swift bridge `sendUsdc`** (`PolygonSend.swift`): picks the funded source account automatically
  (`claim` first, then `fund` — whichever holds ≥ the amount; errors honestly if neither), shows a
  native confirm ("Send X USDC to 0x…? … REAL Polygon USDC on mainnet") BEFORE anything is signed,
  passes the derived secret into the FFI once, and returns the tx hash. After a send the page refreshes
  the USDC balances + history (the native cache merges the new transfer in) and shows the success
  treatment. `WebHostView.swift` stays at the 800-line ceiling (the logic lives in the new file).

**The flow/anatomy fix (matches the capture).**
- **Address step:** Contacts col + divider + `ENTER ADDRESS` box, and **no CTA** — a valid full `0x`
  (typed / pasted / scanned via the native scanner) AUTO-ADVANCES to the amount step. The blue
  "Send to Polygon USDC addresses only! ⓘ" note is centred at the sheet bottom with the QR scan at
  the bottom-right corner. The grey info banner is gone.
- **Amount step:** the recipient shown (the wallet's grey EVM-counterparty avatar + truncated mono
  `0x`), the wallet amount-input, an available line, and an honest **"Network fee paid in POL"** line,
  then the Send CTA. While the mainnet path is **unarmed** (always, in a plain browser) the Send is a
  **neutral grey** disabled state that can never be mistaken for the light-blue CTA. Note: no
  USDC-specific amount-screen reference exists in the library, so the amount step **mirrors the app's
  own NIM send confirm+amount anatomy** (`#send-confirm`), adapted for USDC.
- New strings translated across all 5 languages (en/es/de/fr/pt).

**Tests / verification.** New offline Rust tests (armed-gate refusal, zero/over-balance/insufficient-POL
refusals, the exact `transfer` calldata + EIP-155-137 signing that gets broadcast, the gas clamp, the
fallback limit) + an `eth_estimateGas` codec round-trip. A Playwright mock-bridge harness screenshot-diffs
the address step vs the capture and asserts auto-advance (not on a partial), the amount step's balance +
POL fee, the mocked happy-path success, and armed-vs-unarmed states (`docs/screenshots/usdc-send-*.png`).
Also un-staled two `--all-features`-only gated tests that Andjroo's arming release (#214) had left
asserting the pre-arming (unarmed) behaviour — now state-agnostic, matching the `mainnet_swap.rs` pattern.


## [0.79.0] — 2026-07-14

### USD Coin view rebuilt to the REAL wallet (supersedes #217's hand-built card)

Per Andjroo's fidelity rule, #217's `#view-usdc` was hand-built and did not match wallet.nimiq.com.
This rebuilds the view to the real wallet's USD Coin account anatomy, matched against three captures
of the live wallet's USD Coin flow (`~/.nimmesh-refs/usdc-view/`) using the actual `nimiq-ui`
components/idioms.

- **Account view** now mirrors the real wallet: header = back + a "Search transactions" pill
  (wired to filter rows live, by address or amount) + kebab; an account line with the **official
  USDC circle icon** (the vendored SVG, not redrawn) + "USD Coin" and a right-aligned big
  `X.XX USDC` / grey `Y.YY $`; and month-grouped transaction rows exactly per the capture — date
  column (big day / small month), the wallet's **grey avatar circle** (generic contact silhouette,
  not a Nimiq identicon and not a blank), full monospace `0x` counterparty faded by the list mask,
  time below, and a right-aligned signed amount (`−` plain / `+` on the light-green pill) with grey
  fiat. The hand-built total card, "on Polygon" chip, and hand-built account rows are gone.
- **The three derived accounts** (escrow "fund" / receive "claim" / fee "gas") moved behind the
  kebab into a faithful `USDC accounts` sheet, tap-to-copy, with the honest gas/POL note — no longer
  hand-built rows on the main view. The real wallet shows one balance; ours is one asset across three
  wallet-derived accounts.
- **Receive USDC** modal mirrors the capture: "Receive USDC", the exact subtitle, a navy
  vendored-`qr-creator` QR (same render params as the NIM receive QR) encoding the **claim** address,
  the start···end truncated `0x` under it (tap to copy), and the blue "You can receive from Polygon
  USDC addresses only!" note.
- **Send USDC** ships **honest-disabled** this pass: the modal matches the capture (Contacts column +
  divider, `ENTER ADDRESS` 0x box, the blue "Send to Polygon USDC addresses only!" note, scan icon),
  but there is no standalone USDC-send FFI yet (the core only exposes the HTLC swap-leg constructors,
  not an arbitrary erc20 transfer), so a real money-path would be a new Rust FFI + regen bindings +
  device build + real-funds risk. Rather than fake it, the Send button is a deliberately-styled
  disabled state with an honest "Sending USDC arrives in the next build. You can already receive."
  note — never a dead-looking button.
- The bottom Receive/Send bar is now context-aware (USDC sheets in the USD Coin view). New strings
  translated across all 5 languages (en/es/de/fr/pt). No Rust touched.

## [0.78.0] — 2026-07-14

### The USD Coin card comes alive — tap it for real balances + transfers (like the NIM card)

On the wallet home the NIM card drilled into an address view with balances and history, but the
BTC and USDC cards were dead tiles. The app now holds REAL mainnet USDC on wallet-derived Polygon
accounts (the responder's escrow "fund", the initiator's "claim" receive, and a "gas" fee account);
after the first mainnet swap settles, claimed USDC lands on the derived claim address — invisible
until now. This makes it visible.

- **USDC drill-in** (`#view-usdc`, mirroring the NIM `#view-address` pattern): tap the home USD Coin
  card → a Polygon detail view showing (a) the live total USDC balance + a row per derived account —
  **Swap escrow account** / **Swap receive account** (USDC), and the **Fee account** (POL, with a
  "pays transaction fees" note); each address is tap-to-copy in an identicon-free monospace
  treatment (EVM `0x…` addresses never get a Nimiq identicon); and (b) a month-grouped list of USDC
  `Transfer` events in/out of those accounts, with direction + 6-decimal amounts — reusing the
  wallet's `.transaction` / `.month-label` styling verbatim.
- **Native Polygon reads (no core change).** In-page `fetch()` is dead on device (the page is a
  `file://` origin — WKWebView blocks it), so every chain read is a native `URLSession` bridge case,
  extracted to **`apple/NimmeshApp/Sources/PolygonReads.swift`**: `usdcBalances` (`eth_call balanceOf`
  per derived address + `eth_getBalance` for the gas account) and `usdcHistory` (`eth_getLogs` on the
  USDC `Transfer` topic, filtered by each address in both directions). Reads-only — no key, no
  broadcast, no money-path. History is scanned in **10 000-block chunks** (the drpc free-tier
  `eth_getLogs` cap, fails closed to a smaller step on a range error) from a per-address persisted
  anchor, and merged into a never-lose-history `UserDefaults` cache (union by `txHash`+`logIndex`,
  monotonic last-scanned block) — the same offline-continuity contract as the NIM tx cache. Block
  timestamps come inline from the RPC when present; otherwise the row shows its block number (never a
  faked time).
- **Home USDC card goes live** — balance = the sum across the receive-bearing derived accounts, fiat
  via the existing CoinGecko `usd-coin` price path; honest `0.00` when unfunded, and a previously
  shown balance is never blanked offline (the 0.72.2 lesson — hydrate from cache).
- **BTC card** — no longer a dead tile: tapping it shows an honest note, "No Bitcoin account on this
  device yet — coming with the BTC swap leg." No fake data.
- i18n ×5 for every new string. Playwright mock-bridge verified: card tap opens the view, balances +
  account rows render, history rows + month groups render, the BTC note shows, and the offline cache
  path (failed refresh + cold start) renders without blanking. Non-money-path → auto-merge on green.



### Fixed — the initiator's claim-gas account is fundable BEFORE the swap starts

Pre-flight gap caught before the first mainnet run: the initiator phone pays its Polygon
`withdraw(S)` claim from a wallet-derived gas account (`nimmesh-swap-evm-gas-v1`) that no
screen displayed — an empty one stalls the swap at reveal (only the timelock refund
recovers it). New read-only bridge probe `swapEvmAddresses` (gas/claim/fund addresses, no
secrets cross, node untouched) + the Swap sheet shows the gas account tap-to-copy the
moment the real toggle turns on, before Confirm. i18n ×5, Playwright-verified (armed
labels, respond-mode hides it).

## [0.77.0] — 2026-07-13 — ARM MAINNET SWAP (needs:owner, money-path)

### The deliberate arming act — real mainnet funds

This flips the single master switch from OFF to ON. Until this entry, the mainnet swap path was
inert: `mainnet_swap::MAINNET_SWAP_ENABLED = false` and `MAINNET_HTLC_ADDRESS` was empty, so every
mainnet swap constructor refused. This change:

- records the deployed, source-verified `NimmeshHtlc` escrow on Polygon mainnet:
  `0x842617Ee5365FBa589509c7ffF1fD3Db30a29177`;
- sets `mainnet_swap::MAINNET_SWAP_ENABLED = true`.

With both set, `mainnet_swap_armed()` is now `true` and the mainnet live-swap constructors
(`MeshNode::new_live_swap_initiator_mainnet` / `…responder_mainnet`) assemble the mainnet money path
(native USDC, chain id 137, mainnet confirmation depths, the hard `SwapCaps::mainnet_first_swap`
caps). The first swap is a ≤ $5 self-swap between Andjroo's own wallets, watched, timelock-refunded.

**This is a `money-path` change — Andjroo-merge only. The C1 live-safety gate + SwapCaps are
unchanged; only the master switch + the escrow address moved.**

## [0.76.0] — 2026-07-13 — §8.3 mainnet first-run wiring (INERT until Andjroo arms)

### The mainnet swap constructors + app wiring — off by default, dormant until the arming PR

Wires the callers the guard-lift (#210) needs so the first ≤ $5 mainnet self-swap can run — **without
flipping any const.** `mainnet_swap::MAINNET_SWAP_ENABLED` stays `false` and `MAINNET_HTLC_ADDRESS`
stays empty, so every new path **refuses and is inert** until Andjroo's later arming PR flips both.

- **Core: mainnet live-participant constructors over UniFFI** — `MeshNode::new_live_swap_initiator_mainnet`
  / `…_responder_mainnet`, mirrors of the testnet ctors. They **refuse** unless
  `mainnet_swap::mainnet_swap_armed()` (flag on AND HTLC recorded) — always false today. When armed
  they assemble the mainnet money path in CODE (never caller config): NIM via `HttpGatewayRpc::new_mainnet`
  + `NimHtlcVerifier::new_mainnet`; USDC via `HttpPolygonRpc::new_mainnet` (the two-host cross-read) +
  `LegacyTx::polygon_mainnet` (chain id 137) + native USDC `NATIVE_USDC_POLYGON_MAINNET` +
  `MAINNET_HTLC_ADDRESS` as the escrow; `ConfirmationPolicy::mainnet_defaults()`;
  `SwapCaps::mainnet_first_swap()` enforced; all C1 floors asserted (`live_safety`). The armed
  assembly is factored (`mainnet_money_path` + `assemble_*_mainnet`) so it is unit-tested with an
  injected HTLC address **without** flipping the shipped const.
- **Signer chain id** — `LiveInitiatorSigner`/`LiveResponderSigner` gained `with_evm_chain_id`
  (default Amoy `80002`); the mainnet assembly binds every Polygon tx to `137` (EIP-155 replay
  protection). Testnet path unchanged (byte-identical default).
- **FFI probe** — `mainnet_swap_armed() -> Bool` + `mainnet_swap_reason() -> String` so the app can
  honestly label the state. `false` on any shipped build.
- **App** — SwapMesh.swift + webui: when `mainnetSwapArmed()` is true the Swap sheet's real path and
  the Respond-to-swaps panel use the mainnet ctors with LOUD orange "REAL MAINNET FUNDS" labels and
  the ≤ 50 NIM / ≤ 5 USDC caps; the responder panel shows the wallet-derived MAINNET EVM funding
  address (identical to testnet — EVM addresses are chain-agnostic) tap-to-copy. While unarmed the
  testnet path + labels are exactly as before. Playwright mock-bridge: armed + unarmed renders verified.
- **`scripts/arm-mainnet-swap.sh <htlc>`** — the operator tool that PREPARES (never merges) the
  arming PR: records `MAINNET_HTLC_ADDRESS`, flips `MAINNET_SWAP_ENABLED`, bumps the version, writes
  the CHANGELOG entry, and opens a `needs:owner` + `money-path` PR. Merging stays Andjroo's click.

Inert on a merged branch (the master switch is `false`, no ctor selects the mainnet path, the HTLC
address is empty). Full green gate (fmt + clippy `--all-features -D warnings` + `cargo test --all`
468 / `--all-features` 613 + size-guard). Auto-merge allowed under the standing nimmesh grant (inert).

## [0.75.0] — 2026-07-13 — GUARD-LIFT (needs:owner, money-path, DO NOT auto-merge)

### The mainnet-swap guard-lift — off by default, byte-identical until Andjroo flips one flag

The deliberate, reviewable diff that lifts exactly the `docs/MAINNET-GATING.md` §8.4 points so the
first ≤ $5 self-swap can run on mainnet — **threaded through a single master switch,
`mainnet_swap::MAINNET_SWAP_ENABLED`, which is `false`.** While it is false every gated guard still
refuses mainnet, so a merged branch behaves exactly like testnet-only (the 465-test default suite is
unchanged). Andjroo flips it (reviewed) to arm the swap; the agent never does, and this PR is
Andjroo-merge-only.

- **`mainnet_swap` (new module):** the off-by-default switch + `live_swap_allowed(network)` — the
  single predicate the lifted guards consult. Testnet always; mainnet only when the flag is armed.
- **Guards lifted (behind the flag):** `fund_nim`'s testnet-network refusal and `MeshNode::build`'s
  testnet-only live-signer assertion now allow mainnet **iff** `MAINNET_SWAP_ENABLED` — with it off,
  identical refusals to before. `HttpGatewayRpc::new_mainnet` (NIM leg) already existed; the Polygon
  mirror is new: `guard_polygon_mainnet` + `HttpPolygonRpc::new_mainnet` admit ONLY the two
  allow-listed independent cross-read hosts (`polygon.drpc.org` + `polygon-bor-rpc.publicnode.com`),
  and `NimHtlcVerifier::new_mainnet` pins the NIM verifier to mainnet.
- **Mainnet money-path config:** `ConfirmationPolicy::mainnet_defaults()` = **NIM 10 / USDC 64 /
  BTC 2** (ADR-0003, justified for ≤ $5 self-swaps); `POLYGON_MAINNET_CHAIN_ID = 137` +
  `LegacyTx::polygon_mainnet`; and the canonical **NATIVE** Polygon-mainnet USDC
  `0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359` (Circle's official docs — NOT bridged USDC.e).
- **Hard per-swap caps wired in code (not config):** `SwapCaps::mainnet_first_swap()` (≤ 50 NIM /
  ≤ 5 USDC / ≤ 20 000 sat) is enforced at the coordinator gate — the responder refuses to ACCEPT and
  the initiator refuses to PROPOSE any swap above it (tested: an over-cap Propose yields no Accept;
  the same swap uncapped is accepted, proving the cap is what refused it).
- **NIM mainnet cross-read = single-RPC (residual risk):** no second public Nimiq mainnet RPC exists
  (only `rpc.nimiqwatch.com`); the NIM leg runs single-source on mainnet unless Andjroo stands up his
  own node as the M5 secondary. Merging this PR accepts that residual risk (see the PR body).

The whole guard-lift is **inert on a merged branch**: `MAINNET_SWAP_ENABLED = false`, no constructor
selects the mainnet path, and `MAINNET_HTLC_ADDRESS` is empty until Andjroo deploys + records the
verified `NimmeshHtlc`/`NimmeshForwarder` on Polygon mainnet (deploy plan in the PR body — the agent
never deploys). Full green gate (fmt + clippy --all-features + `cargo test --all`/`--all-features` +
size-guard). **DO NOT AUTO-MERGE — Andjroo-review only.**

## [0.74.1] — 2026-07-13

### Changed — OTA ships 0.73.0/0.74.0 on-device

No code change. Rebuilt ipa so both phones carry the phone-as-responder swap mode (#208 —
the "Respond to swaps" panel with the tap-to-copy funding address) and the BTC-leg verifier
(#209). The responder stays testnet-pinned until the guard-lift (#210) is Andjroo-merged.

## [0.74.0] — 2026-07-13

### Added — the BTC-leg funding verifier (#72 tail, the third chain)

The swap had gateway-backed `FundingVerifier`s for the NIM leg (`nim_verifier`) and the USDC leg
(`polygon_verifier`) but not the **Bitcoin** leg. `btc_verifier::BtcHtlcVerifier` closes that gap —
the BTC sibling, against the same `require_funded` gate:

- **Locates + binds the funding by its script-derived address.** A BTC HTLC lives at a P2WSH
  address both sides derive from the public terms (hashlock + both pubkeys + CLTV). The verifier
  recomputes the exact scriptPubKey and treats an output as funding only if it matches those bytes
  EXACTLY — a P2WSH output at our address that is NOT our script reads `Mismatch`; ordinary change
  outputs are ignored. Amount + timeout are reported and judged by the gate (an underfunded HTLC is
  `Underfunded`, not silently invisible — the `polygon_verifier` discipline).
- **Resolved ≠ funding.** A later tx spending the funding outpoint (a claim or refund) reads
  `Absent`, mirroring the NIM/Polygon verifiers.
- **Depth = tip − funding-block + 1** through `ConfirmationPolicy`; a reorg re-burying it shallower
  is refused again (the gate is stateless).
- **M5 cross-read (ADR-0011):** an optional independent second source (mempool.space +
  blockstream.info) must agree on the funding tx's block height, and its tip folds into a
  conservative (min) depth — a single lying/MITM'd indexer can't fake "funded + deep". Disagreement
  or error → `Absent`.
- **Fail-closed everywhere:** every transport/parse error, an unseen tx, an unconfirmed funding, or
  a cross-read disagreement reads shallow/`Absent`. A transport blip can delay a swap, never
  authorize one.

The reads seam (`BitcoinReads`: address txs / tx status / tip) maps to esplora's endpoints; the
pure verification logic + its offline reads-fake tests are default-feature (so plain `cargo test`
runs all 14 — found-at-depth, too-shallow, unconfirmed, underfunded, wrong-script, spent-output,
transport-error, cross-read disagree/agree/error, foreign-leg, wrong-swap). The `BtcHtlcParams`
P2WSH derivation is behind `bitcoin-leg` (proven against the reference vector) and the live HTTP
reads (mempool.space + blockstream.info, mainnet + testnet bases, host-allowlist guard) behind
`bitcoin-gateway`. Like `polygon_verifier`, `chain_backed` stays `false` (raw CLTV seconds, no
ADR-0010 term mapping) — **testnet-inert: nothing constructs it on a live path until the guard-lift.**
Live proof is GATED (the BTC wallet is empty).

## [0.73.0] — 2026-07-13

### Added — phone-as-responder swap mode (the missing half of a phone→phone swap)

The app could only ever be the swap *initiator* (gives NIM, receives USDC) — the responder
(gives USDC, receives NIM) lived only in the Mac rig (`--swap-responder-live`). This adds the
responder to the phone, so two phones can swap with no Mac in the loop: one initiates, the
other responds. It rides the SAME app-facing FFI ctor the Mac uses,
`MeshNode.newLiveSwapResponder`, which is **testnet/Amoy-pinned and C1-asserted by
construction** — it stays inert for real funds until the Andjroo-gated guard-lift merges.

- **Swap sheet: a "Respond to swaps" toggle** (sibling to the "Real testnet coins" toggle,
  shown only when the native bridge is present). Turning it on advertises "gives USDC, wants
  NIM"; the phone funds NOTHING until its real `NimHtlcVerifier` sees the counterparty's NIM
  HTLC on-chain at depth, then escrows real Amoy USDC and claims the NIM leg with the revealed
  secret. Mutually exclusive with the initiator's "real" toggle; the honest LIVE-testnet note
  and the mainnet-never guarantee are shown out loud, exactly like the initiator path.
- **The responder's Amoy escrow + gas account and its NIM claim address are wallet-DERIVED**
  (HKDF off the recovery-phrase entropy, labels `nimmesh-swap-evm-fund-v1` /
  `nimmesh-swap-nim-claim-v1`) — the same recoverable pattern as the initiator's claim/gas
  accounts, so a reinstall strands nothing and received NIM is always recoverable. The escrow
  address is surfaced in-app (tap-to-copy) so it can be funded with test USDC + POL; the NIM
  claim identity is deliberately NOT the wallet's main key (the G45 privacy rule).
- **i18n** for the new strings in all five languages; the node is restored to the normal
  mainnet node on toggle-off / sheet close (never leave the wallet without a node).

App wiring only — no core change, no mainnet path. Playwright-verified against the mocked
bridge (16 checks: toggle reveal + relabel, mutual exclusivity, `swapMeshStart({respond})`
args, the derived funding address, the listening status, and node restore on close).

## [0.72.3] — 2026-07-13

### Fixed — field reports from the FIRST true phone→phone mainnet mesh payment

Three payments (1 + 15 + 2 NIM, blocks 56054933/56055090/56055105) crossed from an
airplane-mode iPhone 17 over raw Bluetooth to an iPhone 12 Pro that broadcast them to
mainnet — no Mac in the loop. Andjroo's field feedback, all webui:

- **#203 Send sheet reset:** reopening Send carried the previous amount (and a stale
  disabled Send button) until an app restart. The open handler now clears the amount
  (with an input re-sync event) and re-enables the button.
- **#204 history flicker:** rows vanished and reappeared on the receiving phone — two
  live-tier sources (load-balanced RPC backends returning different subsets, mesh compact
  answers) alternated whole-list replaces. `applyHistory` now MERGES by hash: rows never
  vanish within a session, a row never un-confirms, newest first, capped at 50.
- **#205 frozen reach line:** the Send sheet's "will it send?" line was set once at boot,
  so a phone launched offline said "queued until one is in range" forever. It now rides
  the 3-second mesh beat (same fix class as the v0.51.6 Network header).

All three Playwright-verified against the mocked bridge. Still open from the field run:
#206 (reach label should count the Rust gateway's own RPC success — the 12 Pro broadcast
all night while labeled "meshed"; its iOS 16.0 Swift stack fails TLS to
rpc.nimiqwatch.com, whose chain anchors on a 2026 ISRG root old iOS lacks).

## [0.72.2] — 2026-07-12

### Fixed — a foreign-network gateway can no longer poison the wallet balance

Field bug (Andjroo's 2-phone mainnet test): the home balance read **0 NIM in airplane mode**
even though the offline cache (0.53.3) was intact. Root cause: `WorkerCtx` built its G15
`BalanceCache` with the **unpinned** constructor, which adopts the FIRST answer's network —
the Mac, running as a *testnet* swap responder for days, answered the mainnet phone's
balance query with the address's testnet balance (0); the cache pinned itself to testnet,
painted 0 over a funded wallet, and then rejected every genuine mainnet answer as a
network mismatch. Two-layer fix: the cache is now **pinned to the node's own network at
build** (`BalanceCache::for_network`, exactly like the head cache one line above — e2e
regression: a mainnet node never caches a testnet answer, first-heard included, and still
accepts a real mainnet answer after it), and the webui's mesh-balance paint gained a
**monotonic head-height guard** so a stale replay can never repaint or re-cache the page
(history had this tier guard since 0.57.0; balance never did — Playwright-verified).

## [0.72.1] — 2026-07-12

### Changed — OTA ships the M5-hardened core + installs on the second test iPhone

No code change. The Ad Hoc provisioning profile "nimmesh Ad Hoc" was regenerated (via the ASC
API) to include a second registered device — an iPhone 12 Pro (iOS 16.0) — alongside the
iPhone 17 Pro Max, unblocking the true 2-phone BLE mesh test (issue #83). The ipa is rebuilt
at the current core so the OTA finally carries 0.68.0–0.72.0's G10 live-swap wiring and G8 M5
RPC-trust hardening on-device (OTA previously served 0.67.0). New profile uuid
`acbbb5b4-2853-4e63-b558-1f3b1733d069` (same name; expires 2027-06-29).

## [0.72.0] — 2026-07-10

### Wired — G8 M5: the funds-moving Amoy verifier example now cross-reads

`examples/live_amoy_verifier.rs` constructs its `PolygonHtlcVerifier` with
`.with_secondary(HttpPolygonRpc::new(AMOY_RPC_URL_2))` (default: an independent public Amoy
endpoint), so the verifier's real `observe()` path exercises the M5 head cross-read against two
genuinely different providers on the live money path. The run moves real testnet USDC/POL and is
human-gated (needs `AMOY_TEST_KEY` + a funded key); the wiring compiles in CI and runs whenever
Andjroo supplies the key. `docs/swap/M5-RECEIPTS.md` §1b. Testnet/Amoy only.

## [0.71.0] — 2026-07-10

### Hardened — G8 M5: verifier cross-read + independent reveal confirmation (ADR-0011)

The core of the M5 RPC-trust hardening (testnet-only). Each verifier previously trusted its OWN
node's RPC for the data that moves money (depth, inclusion height, escrow state, the withdraw
receipt) — a compromised/MITM'd endpoint could fake "funded + deep" or a settled reveal.

- **Optional independent cross-read** on all three funding verifiers via `with_secondary(...)`.
  When a second endpoint is configured a depth is only trusted if the sources agree, else
  fail-closed: NIM requires exact **inclusion-block** agreement and folds in the conservative
  (min) head; Amoy/Polygon require the two `head`s within `HEAD_CROSS_TOLERANCE_BLOCKS` (12) and
  drive depth from the conservative head; `AmoyHtlcSwapVerifier` additionally re-reads the winning
  escrow as still `Live` on the secondary. When none is configured, the single-RPC trust
  assumption holds and is documented loudly (ADR-0011).
- **Independent reveal confirmation (M5 × M3):** the initiator no longer treats a `withdraw(S)`
  RECEIPT as settlement — before `S` reaches the mesh it re-reads `getSwap(swapId).state == CLAIMED`
  on-chain (and still holds until buried past the reveal depth; the M3 burial hold is not
  regressed). A faked/optimistic receipt can no longer flood `S` while the USDC has not moved.
- **ADR-0011** documents the RPC-trust model (single-RPC assumption, the optional cross-read, the
  content-hash bind, the reveal confirmation) and what mainnet additionally needs (M6 depths + a
  trusted/self-hosted endpoint). `docs/MAINNET-GATING.md` §8.2/§8.4 M5 checkbox updated.

Proof: lying-RPC unit tests (head/depth lies, mismatched content hash, un-`CLAIMED` receipt) all
fail closed and pass on agreement; `examples/live_rpc_cross_read.rs` shows the head cross-read
AGREEING on two independent public Amoy endpoints, read-only, no funds
(`docs/swap/M5-RECEIPTS.md`). **Testnet/Amoy only — no mainnet guard touched.**

## [0.70.0] — 2026-07-10

### Hardened — G8 M5: the NIM verifier's content-hash bind

`NimHtlcVerifier` queried `getTransactionByHash(Blake2b(content))` and trusted the returned
`block_number` without ever confirming the RPC's returned tx actually IDENTIFIES as that digest —
a node could attach a fabricated inclusion height to a response whose tx identity is not the one
we decoded. Now `observe` **recomputes** `Blake2b-256(serializeContent)` fresh from the decoded
creation and requires the returned tx's reported `hash` to equal it before trusting the height; a
mismatch reads `Absent` (fail-closed). The HTTP `get_transaction` now reports the node's OWN hash
faithfully (no fall-back to the queried hash), so the bind reflects the node's real answer.

This binds the returned inclusion data to our decoded content identity. Full protection against a
node that echoes the RIGHT hash with a FAKE height is the cross-read (next slice) — noted in code
and the forthcoming ADR-0011. Testnet-only.

Test: a node that returns a foreign reported hash with a real-looking height → `Absent` even
against a fully-funded contract account; the honest matching hash → `Found`.



### Hardened — G8 M5 (LOW): the `word_u64` over-`u64` overflow guard

The USDC-leg verifiers decoded 32-byte ABI words to `u64` by copying the low 8 bytes and
IGNORING the high 24 — a `> 2^64` amount / timelock / escrow-state word would silently truncate
to a plausible-looking small number. `polygon_verifier::word_u64` and
`amoy_swap_verifier::word_u64` now assert the high 24 bytes are zero and return `Option`; a word
that would overflow reads `None`, so the log/state is skipped (fail-closed) rather than advanced
on a truncated value. Defense-in-depth against a malformed or hostile RPC response; no honest
contract ever emits such a word. First slice of the G8 M5 RPC-trust hardening (testnet-only).



### Added — G10c: the LIVE proof through the app-facing FFI constructors

`live_ffi_mesh_swap` — a real NIM⇄USDC atomic swap driven end to end through the EXACT
`#[uniffi::export]` constructors the app + Mac node call (not the rig door): the initiator via
`MeshNode::new_live_swap_initiator` (the phone's ctor — wallet enclave key funds the real NIM
HTLC, derived gas key lands `withdraw(S)`, derived receive address takes the USDC) and the
responder via `MeshNode::new_live_swap_responder` (the Mac's ctor — verifier-gated USDC escrow
+ NIM claim with the revealed S), both `adopt`ed onto the deterministic ether and `poll_sync`/
`poll_beacon`-driven exactly as the shims tick them.

Ran green on both live testnets (every tx in `docs/swap/G10-RECEIPTS.md`): NIM HTLC funded →
verifier-gated `newSwap` (1 USDC) → verifier-gated `withdraw(S)` (held off the mesh until buried
past the M3 reveal depth) → NIM claim with the mesh-carried S → +1 USDC on the initiator's claim
address, +500 000 luna swept home. The C1/H2/M3/M4 review fixes are baked into the constructors,
so the run also validated the safe path.

Also: `mac-node --swap-responder-live` (the live counterparty rig through the same responder
ctor); `MeshHarness::adopt` (the seam to drive externally-FFI-built nodes over the harness ether);
`EvmLog.transaction_hash` (so receipts recover a `NewSwap` tx without an indexer); the iOS + macOS
node builds compile `-F polygon-gateway`.

## [0.67.0] — 2026-07-10

### Added — G10b: the app wiring for the LIVE testnet swap

`SwapMesh.swift` gains a "real testnet" path beside the Act-1 sim, driven by a new
`{ real: true }` flag on `swapMeshStart`:

- **`swapLiveStart`** builds the live participant via `MeshNode.newLiveSwapInitiator`: the
  wallet's enclave key funds the real NIM HTLC (via `Wallet.enclaveKey`, the same key the
  wallet signs with — the seed never crosses FFI); the claimed USDC pays out to a
  wallet-derived Amoy receive address; a wallet-derived Amoy gas account pays `withdraw(S)`
  (both derived HKDF-off-entropy in `Wallet.swapEvmSecrets`, so a reinstall strands nothing,
  and the gas address is surfaced for topping up with POL). TESTNET/Amoy pinned.
- **Honest labels**: the Swap sheet gains a "Real testnet coins (NIM ⇄ USDC)" toggle (shown
  only inside the app); flipping it swaps the sim's "Simulation: no real funds move yet" for
  "TESTNET — real test coins are moving", forces the NIM⇄USDC pair, and the live status line
  reads "Live over Bluetooth — TESTNET, real test coins" through the real coordinator phases.
- **Never-strand in the app**: every real NIM lock is mirrored to `UserDefaults`, surfaced in
  the status line ("N NIM lock pending refund"), and `swapMeshRefund` drives the core's
  `NimHtlcRefunder` (idempotent — `still-locked` / `refund-broadcast` / chain-truth `resolved`).
- The iOS framework build (`build-adhoc.sh`) now compiles `-F gateway-rpc -F polygon-gateway`
  so the live signer + Amoy stack are in the app.

Verified: the UI flow (toggle → honest label → NIM⇄USDC pair → `swapMeshStart({real:true})`
with a computed `usdcMicro` → live phases advance → lock note) driven headlessly against a
mocked bridge (Playwright).

## [0.66.0] — 2026-07-10

### Added — G10a: the LIVE swap-participant constructors over UniFFI (testnet/Amoy, app-facing)

`swap_live_ffi` — the production door that carries Act 2's live-proven NIM⇄USDC money path
onto the app surface, built directly on the 0.65.0 review fixes:

- **`MeshNode::new_live_swap_initiator`** (the phone, NIM-giver): wallet enclave key over the
  `EnclaveKey` foreign trait (funds + refunds the real NIM HTLC; the seed never crosses),
  ephemeral intent identity + CSPRNG secret PRF from caller randomness (G45/G11), the real
  `AmoyHtlcSwapVerifier` gate, an EVM receive address for the claimed USDC (no key custody to
  receive) + a caller-held Amoy gas key for `withdraw(S)` (ADR-0011).
- **`MeshNode::new_live_swap_responder`** (the Mac rig, USDC-giver): funds NOTHING until the
  real `NimHtlcVerifier` sees the initiator's HTLC at depth; escrows real Amoy USDC and
  claims the NIM leg with the revealed `S`.
- **Safety in the door, not the caller:** both ctors assert `SwapSession::live_safety()` and
  are pinned by `guard_testnet`/`guard_amoy` at construction (no network parameter exists);
  a one-shot funding latch means one construction can never move more than the one
  advertised trade; a `LiveLockBook` (caller-held) records every real NIM lock off the exact
  broadcast wire, and `NimHtlcRefunder` turns an expired lock back into the wallet's funds
  (`StillLocked` / `Refunded` / `AlreadyResolved` — idempotent, chain-truth-released).
- `evm_address_for_secret` (behind `polygon-leg`) so the app derives its Amoy accounts
  without native EVM math. Every door refuses honestly (`Unsupported`) in builds without
  `polygon-gateway`+`gateway-rpc` — the shared bindings never diverge.

Swift bindings regenerated; the new symbols verified in `generated/nimmesh_core.swift`.
Offline tests: validation, guard pinning, C1-at-the-door, latch, lock book, byte-exact refund.

## [0.65.0] — 2026-07-09

### Security — the G8 money-path review's testnet fixes (C1 / H2 / M3 / M4), enforced in the core

The independent G8 review of the live money-path came back NO-GO for mainnet until specific
fixes land. This PR lands every **testnet-scope** fix at the structural level, so the G10
live-participant constructors (next PR) are safe by construction — no guard was lifted,
nothing here touches mainnet.

- **C1 — a live signer can never ride an unsafe session, through ANY door.**
  `SwapSigner::is_live()` (default `false`; the live NIM⇄USDC signers answer `true`) ×
  `FundingVerifier::chain_backed()` (default `false` — fail-closed; only `NimHtlcVerifier` +
  `AmoyHtlcSwapVerifier` opt in) × `SwapSession::live_safety()` (chain-backed verifier, non-sim
  secret source, non-zero confirmation floors). `MeshNode::build` — the funnel every
  constructor passes through — now refuses (loud panic; the FFI doors will surface it as an
  `Err` first) to build a node whose signer is live over a session that fails `live_safety`,
  or on any network but testnet. The `AcceptAllVerifier`/`sim_secret` footguns can no longer
  guard real funds by omission. (`PolygonHtlcVerifier` deliberately stays non-eligible: its
  raw-seconds timeout would make the floor vacuous — the mapped `AmoyHtlcSwapVerifier` is the
  live Amoy gate.)
- **H2 — the #189 tombstone now survives settle → reap → restart.** `initiated_ever` is
  persisted in the session snapshot (a backward-compatible trailer of 16-byte ids; a torn
  trailer is a hard `Truncated`, never a silently dropped tombstone) and re-armed by
  `restore_bytes`. Regression: a SETTLED, reaped initiator swap stays tombstoned across the
  byte round-trip.
- **M3 — the reveal deadline gates BEFORE the signer, and the reveal is held until buried.**
  The driver now checks `reveal_deadline_ok` before handing `S` to the signer (a live claim
  broadcasts `withdraw(S)` — the coordinator's own gate inside `claim_and_reveal` fired too
  late for a live signer). The live initiator additionally holds the `PreimageReveal` off the
  mesh until the withdraw is buried past 1 confirmation (`REVEAL_MIN_CONFIRMATIONS = 2`), and
  a landed withdraw is never rebroadcast (the replay path releases the same claim later).
- **M4 — term anchors are wired to the head, and absolute timelocks are sanity-bounded.**
  `SwapContext`/`HtlcExpectation` now CARRY the mesh-head anchor the terms were minted
  against (initiator stamps it at initiation, responder on the `Propose`); the live signers
  and both live verifiers map against that per-swap anchor instead of a constructor-frozen
  zero (the `term_anchor` config fields are gone). Both funders refuse any mapped on-chain
  timelock beyond `now + 6 h` (or at/behind now) BEFORE broadcasting — a mis-anchored term
  can no longer mint a multi-week lock. Snapshot codec extended accordingly.

M5/M6 (independent reveal confirmation, mainnet depth retune, the mainnet guard lifts) are
the MAINNET-only remainder — deliberately not touched; they stay in the Andjroo-gated Phase 4.

## [0.64.1] — 2026-07-08

### Fixed — #189: a revealed swap secret can never be reissued (money-path, pre-mainnet gate)

Act 2's live run surfaced it: `derive_swap_id` is deterministic in the two parties'
identities, and every secret source is a pure function of `swap_id` — so a repeat match
between the same counterparties (which standing intents produce the moment a swap settles
and is reaped) reissued the SAME `S`. A settled swap has already PUBLISHED that `S`, so
the repeat HTLC was claimable with public chain data and no counterparty escrow — a
one-sided loss against the initiator (proven live on testnet).

Fix: the session permanently **tombstones every `swap_id` it initiates** (`initiated_ever`,
rebuilt from the recovery snapshot for initiator swaps so it survives a restart). Both
`handle_intent` (buffering) and `initiate_from_intent` (window close) refuse a tombstoned
id, so a repeat trade must re-advertise under a fresh ephemeral identity → fresh `swap_id`
→ fresh `S` (the G45 privacy-preferred path anyway). Two regression tests: a reaped
coordinator leaves the tombstone standing; a restored initiator swap stays tombstoned.

## [0.64.0] — 2026-07-08

### Added — A2c: a REAL two-node NIM⇄USDC atomic swap, executed live on both testnets

`live_mesh_swap_nim_usdc` — Act 2's headline proof, run to green against the real chains
(every tx hash in `docs/swap/ACT2-RECEIPTS.md`): two participant `MeshNode`s over the
deterministic harness drove the SHIPPING protocol end to end with real testnet money —
discovery matched the standing intents, alice funded a real NIM HTLC (5 tNIM), bob's REAL
`NimHtlcVerifier` gated his `approve`+`newSwap` (1 USDC, deployed `NimmeshHtlc` v2) on the
chain, alice's `AmoyHtlcSwapVerifier` gated her reveal, `withdraw(S)` landed on Amoy, bob
read `S` off the reveal and claimed the NIM leg — one secret, two chains, zero manual steps.

Built to be safely re-runnable: every lock is persisted the moment it exists; on startup the
example refunds anything a dead run left behind (or refuses to start while a lock is still
timelocked — never more than one swap's funds in flight); a ONE-SHOT funding latch keeps the
continuous standing-intent advertising (G37, by design) from funding a second real swap the
moment the first settles — run 1 caught exactly that, and its repeat lock was refunded
through the example's own recovery path. Completion is detected from CHAIN TRUTH (escrow
Claimed + HTLC emptied), never from the reap-racy phase mirror, and the claimed NIM is swept
home to the treasury. Plus tiny store accessors (`NimFundingStore::records`,
`PolygonFundingStore::found_all`) the rig reads for receipts/refunds.

## [0.63.0] — 2026-07-08

### Added — A2b: the LIVE NIM⇄USDC money-path signer + the last real funding gate

The first `SwapSigner` that moves real (testnet) money, and the NIM-leg funding verifier that
closes the #72 tail. All behind seams — the sim, the phone, and every existing test are
byte-identical in behaviour:

- **`live_swap_signer`** (behind `polygon-gateway` + `gateway-rpc`): `LiveInitiatorSigner`
  (funds the real NIM HTLC at Accepted; lands the real Amoy `withdraw(S)` at BothFunded —
  only a MINED success ever reveals `S`, and the reveal wire is `S ‖ raw tx`) and
  `LiveResponderSigner` (funds `approve`+`newSwap` at InitiatorFunded with a POL budget
  preflight that reserves the refund; claims the NIM HTLC at Revealed). Gas policy = the
  clamps/limits the live G6/G7 runs proved. Every signing path is byte-asserted offline
  against the proven builders over deterministic chain fakes.
- **`nim_verifier::NimHtlcVerifier`** (always compiled, offline-tested over `MockRpc`): the
  NIM sibling of `polygon_verifier` — locates the initiator's HTLC from the FundingProof wire
  (an untrusted hint, HASH-BOUND to the chain: the content-derived tx hash + contract address
  can only confirm the exact decoded creation), then gates on inclusion depth + the live
  contract account. Fail-closed everywhere.
- **`live_swap_signer::AmoyHtlcSwapVerifier`**: the initiator-side USDC gate — the deployed-
  contract scan anchored at the FundingProof-named tx's receipt (the public RPC's ~50-block
  `eth_getLogs` cap kills blind lookbacks), recording the found `swapId` for the claim.
- **Two new default-no-op seam methods**: `SwapSigner::note_peer` (the driver reports the
  protocol-carried counterparty addressing — `SwapContext` holds only our own) and
  `FundingVerifier::note_funding_wire` (the session feeds each FundingProof's wire as a
  locating hint; the chain stays the sole truth). Plus the rig door
  `MeshNode::new_session_participant` / `MeshHarness::add_session_participant` (caller-composed
  session + signer; testnet-pinned, not on FFI).
- **ADR-0010**: the term↔wall-clock timeout mapping (ms on NIM, seconds on Amoy, verify-side
  slack), the NIM⇄USDC role map + units, and the trust analysis of both new seams.
- Seam e2e over the REAL node loop: a mesh swap whose responder runs the REAL `NimHtlcVerifier`
  refuses to fund until the mock chain confirms the byte-exact creation, then settles both
  sides — the S1 gate proven at node level with live parts.

## [0.62.0] — 2026-07-08

### Changed — Act 2 opens: the swap signing seam is ready for real money (core only)

The three seam changes a live NIM⇄USDC signer needs, all behavior-neutral for the sim:

- **`SwapSigner` v2**: every method now receives the swap's full `SwapContext` (hashlock,
  timelocks, amounts, both parties' addressing — `btc_address` is chain-agnostic bytes, so
  an EVM claim address rides it with zero wire changes) and is fallible — a live signer
  that can't build/broadcast simply doesn't advance, and the timelock refund stays the floor.
- **The responder now claims its own NIM leg at Revealed** (with the secret learned from
  the reveal) before settling — previously the driver only flipped state; a real responder
  has money to collect.
- **G11 secret seam**: `SwapSession::with_secret_source` replaces the deterministic
  `sim_secret` for production — the FFI participant already injects a CSPRNG-backed PRF
  (sha256 over a domain-separated master from the caller's entropy, per-swap unique).

## [0.61.1] — 2026-07-08

### Fixed — every amount input refuses more than you hold (Andjroo's field report)

Swap, Cash Links, and Send all accepted any number and only failed later (or, in the
swap's sim case, pretended). All three now check the live balance up front — "More than
your balance," in all five languages.

## [0.61.0] — 2026-07-08

### Added — Cash Links: NIM that travels as text, claimable by anyone in a browser

The other half of the dead-zone story: paying someone who has NO app. A cash link is a
fresh single-use key funded by the wallet; the key + amount ride the URL in the
**official Nimiq Hub format** — verified against the production parser (hub.nimiq.com
rendered our fragment: "5 NIM · mesh test · Claim your Cash"). The recipient opens the
link in any browser, creates an account in 30 seconds, and claims. No infrastructure of
ours anywhere.

- **Side menu → Cash Links**: amount + optional message → Create. Funding is a NORMAL
  wallet transfer over the proven path — online RPC, or **over the Bluetooth mesh** when
  offline (the toggle appears when a mesh send is possible). Result: QR (scan it right
  off the screen — fully offline handoff), native Share sheet (AirDrop/SMS/Bitchat/
  anything), Copy, and **To chat** (drops the link into Mesh Messages, you still tap
  send). Honest label: *a link is cash — anyone holding it can claim*.
- **Your cash links**: kept in the iOS Keychain (the URL embeds the key — never
  UserDefaults, never logs). Live claimed/unclaimed chips from the chain when online.
  Pre-fund a few at home, hand them out anywhere like bills.
- Format notes: seed(32) ‖ value u64 BE ‖ optional len-prefixed message, unpadded
  base64url; address = the standard Blake2b single-sig derivation (the same Rust path
  the wallet itself uses via `AppSigner`). Funding is a plain transfer — the hub claims
  it fine; the cosmetic "CASH" extraData label needs extended-tx support in the signer
  and is a noted follow-up.

## [0.60.3] — 2026-07-08

### Fixed — the right-edge clipping was iOS focus-zoom, not layout

Andjroo's screenshots showed EVERYTHING at the right edge clipped by the same sliver —
the language pill, the sheet's close button, his own bubble, the send arrow — plus
sideways panning. That's iOS's automatic viewport zoom: focusing any input with a font
under 16px zooms the page ~7% and KEEPS the zoom after the keyboard closes. The chat
input was 15px. Fixed at both ends: the input is 16px, and the viewport is scale-locked
(`maximum-scale=1`, app-style — also protects the other small inputs like the recovery
word cells from ever triggering it).

Also confirmed live this release cycle: airplane-mode + Bluetooth-only chat delivered
instantly on 0.60.2 (`💬 Anon: Testing` at the gateway, matching the phone's 11:40 send).

## [0.60.2] — 2026-07-08

### Fixed — offline chat actually sends (Andjroo's field report) + chat input overflow

Bluetooth-only + airplane mode: messages "didn't send until Wi-Fi came back." Root cause
was a chain of three since the phone became a gateway (0.56.0):

- **The beacon keepalive went silent offline** — `emit_head_beacon` emitted nothing when
  the live RPC head fetch failed, so an offline phone-gateway produced zero BLE traffic,
  iOS idle-dropped the link (~50 s), and chat floods hit zero peers. The beacon now falls
  back to the **last live head** (receivers are monotonic — a stale re-beacon is harmless;
  the beacon IS the keepalive, it must not depend on the chain).
- **The failed RPC probe blocked the worker every tick** (up to the 10 s connect timeout,
  on the single thread that also floods chat). Probes now back off ~50 s between attempts
  while failing; the stashed head beacons in between. Clock-free `select_beacon` helper,
  unit-tested including "never probes during backoff."
- **Nothing ever recovered a chat dropped during a flap** — gossip-sync (`requestSync`)
  only ran on `poll_sync`, which no shim calls (the same class of bug as 0.58.0's
  `gc_tick`). `maintenance_tick` now rides `BeaconTick`, the one heartbeat real devices
  have. New regression test: a chat sent with the link down is recovered on the first
  heartbeat after it heals.
- **Chat input row overflow**: the flex input couldn't shrink (`min-width: 0`), pushing
  the send button off-screen and letting the sheet wobble sideways; the sheet and scroller
  now clip horizontally.

## [0.60.1] — 2026-07-08

### Added — the Mac node speaks Bitchat: cross-app messaging with Jack Dorsey's mesh

`shared/BitchatKit.swift` — a byte-exact, CryptoKit-only implementation of the Bitchat
public-chat wire protocol (their repo is public domain): their GATT service, the 14-byte
v1 header, the dual-key identity (peerID = SHA-256(noise pubkey)[0..8]), signed announces
(TLV nickname+keys), signed public messages, and the tricky signature canonicalization
(ttl-zeroed, signature-less, PKCS#7-padded to their block schedule). Endpoint only —
we never relay their packets; private (Noise) messages deferred.

- **`mac-node --bitchat`**: joins the real Bitchat mesh beside the nimmesh roles (own
  CoreBluetooth managers, zero contact with the proven radio) and **bridges**: verified
  Bitchat public messages cross into nimmesh chat tagged `₿ nick` (never bounced back);
  nimmesh chats heard are forwarded onto Bitchat once each. A nimmesh phone and a real
  Bitchat app can talk through the Mac today.
- **Startup self-test** (no test runner in this chain): padding block vectors incl. the
  gap zones where PKCS#7's one-byte limit forbids padding, encode/decode round-trip,
  ttl-independent signature verification, announce round-trip + peerID binding, tampered
  payload refused. The link refuses to start if any check fails — and the first run
  proved the point by catching a wrong pad vector.

## [0.60.0] — 2026-07-08

### Added — the mesh gets a messenger: public chat over Bluetooth (0x50)

Andjroo's ask: "we need a messenger so we can send them messages… or other people in our
network." Bitchat-style **public broadcast chat** riding the exact same flood machinery as
every payment packet — dedup, TTL hop cap, store-and-forward (a peer that walks into range
later catches up via gossip-sync), multi-hop relay. Fully offline. This is also the future
carrier for swap invites and cashlinks between strangers.

- **Core**: new `chat` module + `0x50` wire type. Payload = version ‖ wall-clock ‖ nickname
  (≤32 B) ‖ UTF-8 text (≤160 B — the SMS discipline; over-budget is refused, never
  truncated). Rolling 200-message log deduped by `(sender, seq)`; hostile-input decode
  (truncation/UTF-8/trailing bytes all rejected, malformed still blind-relays — we never
  censor what we can't parse). FFI `sendChat`/`chatMessages`.
- **App**: "Messages" in the side menu → a chat sheet (wallet-styled bubbles, nickname from
  the wallet label, content-key-gated rendering so polls never blank the list). Honest
  label: *Public — everyone on the mesh can read it* (the encrypted 1:1 lane is the Noise
  follow-up). i18n ×5.
- **Mac node**: greets the mesh when the first phone links and logs every message heard —
  a live chat partner for the one-phone test.

## [0.59.0] — 2026-07-08

### Added — the swap protocol goes live over real Bluetooth (Act 1: sim funds)

The first end-to-end delivery of 0.58.0's production participant door: a phone and the
Mac can now discover each other and run the full HTLC swap protocol — signed intent
advertisement, match, authenticated Propose, Accept, funding proofs, preimage reveal,
Settled — over the real BLE mesh. **Sim transaction bytes throughout (`MockSigner`):
the protocol is real, no value moves.** Testnet-pinned.

- **`mac-node --swap-responder`**: the counterparty rig. A TESTNET participant+gateway
  advertising a standing "gives 1000 sats, wants 10 NIM" intent (cheap ask so any
  real-price phone offer crosses on rate); logs discovery counters and live swap phases
  (`⇄ intents seen … · swap …`).
- **Phone swap-demo bridge** (`swapMeshStart`/`swapMeshStatus`/`swapMeshStop`): starting
  a swap from the Swap sheet swaps the app's node onto a TESTNET participant advertising
  the user's ask; the sheet's phase timeline now renders the node's REAL coordinator
  state polled live (the sim driver remains only as the browser fallback). Closing the
  sheet (or Done) restores the normal mainnet node. The intent/Propose identity is an
  ephemeral key from fresh randomness — the wallet key never leaves the Keychain.

## [0.58.0] — 2026-07-08

### Added — the production swap-participant door (G9 slice 3): real devices can now discover and negotiate swaps over the mesh

The entire mesh swap protocol (discovery, matching, signed Proposes, funding proofs,
preimage reveal, retransmit, refund safety, GC) has been merged and tested for months —
but every participant constructor was `#[cfg(test)]`-only, so no real device could use it.
This opens the production door, with **sim tx bytes only** (the `MockSigner` seam): the
protocol runs for real over real Bluetooth; no funds can move until the gated money-path
signer drops into the same seam.

- **`MeshNode::new_swap_participant` over UniFFI** (`swap_participant_ffi.rs`, the
  `gateway_ffi` pattern): builds a full mesh node that also runs the swap session.
  Config records `FfiParticipantConfig` + `FfiStandingIntent` (+`FfiIntentAsset`) carry
  the trade to advertise, the Δ_safe ladder, and 32 bytes of caller randomness for the
  **ephemeral** intent/Propose identity (G45 — never the wallet key; the same key signs
  the intent (G41) and the Proposes (S2/#73), so `recv_propose`'s authenticity gate
  holds end to end). Optional `gateway_rpc_url` makes it also a testnet gateway (the Mac
  responder rig). Testnet-pinned by construction — no mainnet parameter exists.
- **Swap upkeep now rides the beacon heartbeat**: `gc_tick` (intent re-advertise, match-
  window close, retransmit, refund exit, GC) only ran on `poll_sync` — which neither
  device shim ever calls. On a real phone the standing intent would never have flooded
  and a match would never have initiated. It now also runs on `BeaconTick` (~15 s), the
  same fix the data-mule retry needed in 0.56.0. Proven by a new test: a constructed
  participant re-advertises within three beacon polls.
- **Intent-initiated sim timelocks are head-anchored**: `initiate_from_intent` minted
  `T_A/T_B = 10_000/5_000` — born-expired against a real testnet head in the millions.
  Now `head + 10_000 / head + 5_000` (identical at head 0, so the deterministic suite
  is untouched).

## [0.57.0] — 2026-07-07

### Changed — Send is now the wallet's two-step flow; history stops flickering

Both from Andjroo's field feedback.

- **Send flow rebuilt to match the real wallet**: enter/paste/scan the recipient on
  step 1 (Continue lights only for a complete valid address), then a dedicated confirm
  page shows the recipient BIG — identicon + bold spaced address — before you set the
  amount and send. Pasting or scanning a full address jumps straight to confirm; a back
  chevron returns to editing the recipient. Replaces the old cramped single sheet with
  a tiny identicon tucked under the grid.
- **History no longer flickers offline**: the list was fed by three sources (localStorage
  hydrate, the native RPC cache, the mesh) that each re-rendered every ~10s cycle with
  different sets — so on a Bluetooth-only phone the stale native cache and the fresher
  mesh answer fought, each wiping the list (rows vanished then popped back). Now one
  freshness-gated funnel renders ONLY on genuine content change, a staler source can't
  overwrite a live one, and identicon data URLs are cached so a real re-render never
  blanks. Verified: zero list rebuilds across repeated offline refresh cycles.

## [0.56.0] — 2026-07-07

### Added — range, two ways: the data-mule and the phone-gateway

Andjroo's field report: drive away from the house and a send just dies. Two answers:

- **Data-mule (`pending_retry.rs`)**: the origin now keeps every still-pending tx it
  signed (bytes + validity) and re-offers it on the ~15s heartbeat whenever peers are
  present — sign a payment in the middle of nowhere and it delivers itself the moment
  the mesh reappears, any time inside the tx's ~2h chain-validity window. A tx flooded
  into the void retries IMMEDIATELY on first contact (no cadence wait). A receipt
  clears the retry; past-validity sends settle Failed honestly instead of "relaying"
  forever. Re-floods are idempotent (same relay key: old peers dedup, only new peers
  carry). e2e: sign with zero peers → connect later → tick → settled.
- **Phone-as-gateway**: the iOS framework now builds with `gateway-rpc`, and the app's
  node is a MAINNET gateway — any phone WITH internet broadcasts other people's mesh
  txs, answers balance/history queries, and beacons the chain head; a whole group with
  one online member has a working exit. Offline it self-gates (every RPC fails → no
  answer, no receipt — another gateway can still carry the tx) and behaves exactly like
  the old plain relay. Holds no keys for others, broadcast-only, same role as the Mac.
- **Honest reachability**: a self-gateway node always reports Online, so the reach line
  is now computed in the app: online = a real RPC round-trip in the last 30s; meshed =
  live BLE peers; else offline.
- Filed #172: LoRa bridge + Raspberry Pi relay nodes for kilometer-scale coverage.
- 800-guard: the G6 fragment engine-glue moved to `fragment.rs`.

## [0.55.0] — 2026-07-07

### Added — TRANSACTIONS over the mesh: the history list updates with zero internet

The last offline gap from Andjroo's field testing: a Bluetooth-only phone could see its
balance move (0.54.0) but the transaction rows stayed frozen. Now the rows travel too.

- **New wire pair** `nimiqTxHistoryQuery 0x35` / `nimiqTxHistoryResponse 0x36`
  (`tx_history.rs`, the G15 pattern): a node floods the query, any internet-bearing
  gateway answers up to 10 compact records (hash, counterparty, value, timestamp,
  incoming/confirmed flags) read from its RPC at the current head. Unverified /
  last-known trust model, monotonic per-head cache, network-validated.
- **First real user of the G6 fragmenter**: the ~716-byte answer is flooded as
  `fragment 0x20` chunks sized to the proven 256-byte frame class and reassembled by
  the existing engine path. Proven end to end in a headless mainnet e2e.
- **Gateway side**: `MeshGateway::history_of` (default `None` — additive for all other
  impls) + `GatewayRpc::get_transactions` (default unsupported; real impl via
  `getTransactionsByAddress`; mock seedable).
- **App**: on the same offline beat as the mesh balance, the app queries the mesh for
  history and re-renders the list when fresher rows arrive — a new incoming payment now
  shows up as a row on a fully offline phone within seconds.
- **Send-sheet identicon confirmation** (Andjroo's report): a complete valid address in
  the Send grid now renders the recipient's identicon under the grid (typing, paste, or
  QR scan) — the wallet's visual "this is who you're paying" cue. Hidden while invalid.
- **Restart-dedup fix**: the packet sequence (used as the blind relay-key timestamp) is
  now clock-seeded instead of starting at 1 — a restarted node with a stable sender id
  (the Mac) no longer reuses relay keys that a still-running peer already saw, which
  silently deduped every packet of the new session until the app relaunched.

Verified: 404 core tests incl. new codec/cache units + the fragmented mainnet e2e
(10 rows, direction flags, head stamp); Playwright offline run renders the NEW incoming
tx row + 495 NIM from mesh answers with RPC dead; identicon paste check.

## [0.54.0] — 2026-07-06

### Added — balance over the mesh: incoming funds show up with ZERO internet (G15)

Andjroo's live-test gap: 5 NIM sent to the phone while it was Bluetooth-only stayed
invisible until Wi-Fi came back. Now, whenever the RPC read is offline (failed or
served from the native cache), the app floods a `nimiqBalanceQuery` over BLE and the
Mac gateway answers with this wallet's on-chain balance (`nimiqBalanceResponse`) —
the core's G15 machinery, live end to end for the first time. The home balance
updates within a poll tick, fully offline. (History rows for the new funds still
need a network round-trip — balance is what travels over the mesh today.)
New bridge methods `meshQueryBalance` / `meshCachedBalance`; no core changes — the
gateway side has been answering since G15 shipped.

### Fixed — fiat appears instantly; per-transaction fiat no longer pulses forever

- The last-known rates persist per currency and hydrate at launch, so fiat lines
  paint immediately instead of waiting seconds for CoinGecko (and still paint when
  fully offline). The live fetch refreshes them and re-paints when it lands.
- The price fetch no longer delays the balance paint (it was serialized in between).
- Each history row's fiat amount is now actually filled (current rate) — the
  `fiat-loading` skeleton pulsed forever because nothing ever populated it. With no
  rate available at all it stops pulsing and stays blank, honestly.

Verified: Playwright offline run — home balance updates to the MESH-reported value
(501 → 506) with RPC dead, fiat renders from cached rates with the price fetch
failing, and row fiat is filled with the pulse gone.

## [0.53.4] — 2026-07-06

### Added — transaction detail with tap-to-copy addresses (Andjroo's ask)

Tapping a transaction in history now opens the wallet's TransactionModal treatment —
captured live from the testnet wallet for fidelity: "Transaction from/to" title, the
green "Received at" / "Sent at" date line, sender → recipient identicons with the FULL
address grids beneath, and the big signed amount with fiat.

- **Both address grids are real Copyables**: tap to copy the full spaced address, with
  the wallet's blue "Copied" tooltip. Grab a counterparty address straight from history
  to send funds back — fully offline over the mesh.
- **The Send address grid now accepts a paste**: a full NQ address pasted into any cell
  distributes across all nine blocks (same convenience as the import sheet's
  paste-whole-phrase). Copy from a transaction, paste into Send, done.
- New i18n strings in all 5 languages.

Verified: Playwright end-to-end — row tap opens the sheet (title/date/labels/amount/
fiat all correct for incoming and outgoing), tapping the counterparty grid puts the
exact spaced address on the clipboard, and pasting it into the Send grid fills all
nine cells. Screenshot compared against the live wallet capture.

## [0.53.3] — 2026-07-06

### Fixed — the REAL cause of the blank offline wallet: the RPC layer lied

0.53.2's cache never got a chance: `NimiqRpc.balance` returned **0** and
`NimiqRpc.transactions` returned **[]** on ANY failure — so going offline looked like
a *successful* fetch of an empty wallet. The UI rendered 0 NIM / no history and the
cache was overwritten with the empty data. (The 0.53.2 verification mocked rejecting
reads, which is not what the device does — lesson recorded.)

- `NimiqRpc.balance` / `transactions` now **throw** on transport/RPC failure — offline
  is no longer indistinguishable from "0 NIM, no transactions".
- The last-known balance + history now live in a **native UserDefaults cache** (keyed
  by wallet address, cleared on wallet deletion): a successful read updates it, a
  failed read answers with it (`cached: true`), so the webui renders last-known data
  with zero reliance on web storage surviving a relaunch.
- The 0.53.2 localStorage layer stays as an in-session belt-and-suspenders.

## [0.53.2] — 2026-07-06

### Fixed — Bluetooth-only launches showed a blank wallet (no balance, no history)

Andjroo's report from the live mesh test: with every radio except Bluetooth off, the
wallet showed no transactions at all — not even ones he had already seen. The chain
reads (balance + history) are live RPC fetches, and nothing survived a relaunch: an
offline first fetch rendered the honest-but-useless empty state.

Now the last-known balance and transaction history persist on-device (localStorage,
keyed by the wallet address — public chain data only, cleared on logout from both
menus). An offline launch hydrates from that cache before the first live fetch, so
the wallet shows what it last knew; live data overwrites the moment the network is
back. A failing first fetch can no longer blank an already-rendered history.

Verified: Playwright offline-launch run (every chain read rejecting) renders the
cached 501 NIM + 2 history rows with no empty state, and the same flow live-updates
when fetches succeed.

## [0.53.1] — 2026-07-06

### Fixed — fiat conversion + menu sparklines were dead ON DEVICE

Andjroo's report from the live mainnet test: the fiat line under NIM amounts never
showed anything on the phone. Root cause: the webui loads via `loadFileURL`, so the
page runs on a `file://` origin — and WKWebView blocks the page's `fetch()` to the
network entirely (CoinGecko's permissive CORS never even gets consulted). The desktop
browser used for verification allows it, which is why this only bit on device.

Fix: the two CoinGecko endpoints are now proxied through native URLSession bridge
methods `prices` / `market` — whitelisted coins and currencies only, no arbitrary-URL
surface; only the numbers cross the bridge. The webui prefers the bridge and falls
back to direct fetch in a plain browser. Fixes the home/total/address fiat lines, the
swap sheet's indicative pricing, and the side-menu 24h sparklines.

## [0.53.0] — 2026-07-06

### Added — the MAINNET mesh payment (Andjroo-gated, real funds)

Andjroo exercised the mainnet gate (docs/MAINNET-GATING.md section 7): he provides the
NIM, signs on his phone, and sends to an address he controls; the Mac gateway only
delivers. The mesh now carries a REAL payment end to end.

- **Network-coherent nodes**: `WorkerCtx` carries the node's `NetworkId` — the head
  cache accepts only same-network beacons, locally-originated envelopes are stamped
  with the node's `networkId`, and `anchoredIntent` yields intents on the node's
  network. Every existing constructor stays testnet; behavior there is unchanged.
- **Mainnet gateway (explicit, loud)**: `HttpGatewayRpc::new_mainnet` (refuses
  testnet-looking URLs — the mirror of `guard_testnet`), `RpcGateway::new_mainnet`
  (enforces `networkId = 24`), and FFI `MeshNode.newGatewayMainnet`. The Mac node
  broadcasts on mainnet ONLY with the explicit `--mainnet` launch flag; it holds no
  keys and signs nothing.
- **Phone on mainnet**: FFI `MeshNode.newOnNetwork` — the app's node is pinned to the
  app's (mainnet) network, so the offline mesh send signs REAL funds exactly like the
  online Send; the mesh only changes delivery. The Send-sheet row's network tag is now
  set from the bridge (orange MAINNET pill).
- **Proofs**: new `mainnet_e2e_tests` — a mainnet-signed transfer anchors to the
  mainnet beacon and settles with the exact signed bytes broadcast; a testnet tx is
  refused by the mainnet gateway (nothing broadcast); a mainnet node ignores testnet
  beacons. Receipt codec moved to `settlement.rs` and test-only participant ctors to
  `node_tests.rs` (800-line guard).

## [0.52.0] — 2026-07-06

### Added — the offline mesh payment path (TESTNET proof)

The milestone build: a phone with **no internet** can now sign a NIM transaction and hand it
to the Bluetooth mesh, and the Mac node — now a **gateway** — broadcasts it to the Nimiq
TESTNET chain and floods the receipt back so the sender sees it settle.

- **Core FFI**: new `MeshNode.newGateway(senderId, radio, rpcUrl)` UniFFI constructor builds
  the real HTTP broadcast client (`HttpGatewayRpc` + `RpcGateway`, G8) behind the existing
  testnet-only guard — a known mainnet RPC host is refused at construction, and the gateway
  drops any non-testnet `networkId`. The constructor is exported in EVERY build so the shared
  bindings never diverge; without the `gateway-rpc` cargo feature it returns
  `GatewayInitError.Unsupported`. Verify-before-relay stays ON (G12).
- **Mac node = gateway**: `mac-node` now builds the core with `--features gateway-rpc` and
  constructs the gateway node against `rpc.testnet.nimiqwatch.com` (falls back to a plain
  relay, loudly, if the core lacks the feature). Its ★ "payment relayed" line is the
  utility-earned event.
- **Phone offline send**: an explicit "Send over Bluetooth mesh" opt-in row (orange TESTNET
  tag) in the Send sheet — never a silent fallback on the mainnet app. Shown only when a
  gateway head beacon has been heard AND a peer is live. The path: `anchoredIntent` (G9,
  validityStartHeight = mesh-heard head, testnet network) → Keychain sign →
  `submitSignedTransfer` → poll `paymentStatus` for the gateway receipt
  (pending → settled/failed). New bridge methods `meshSendInfo` / `meshSendTransaction` /
  `meshPaymentStatus`; status copy in all 5 languages.

## [0.51.6] — 2026-07-06

### Fixed — mesh status was stale + link flapped (2-node test)

- **Live mesh status**: the Network header / mesh banner / world map were set ONCE at boot,
  so they froze at "offline · 0" even while the radio was connected (the debug line proved
  the node had the peer). Now polled every 3s.
- **Keepalive**: the BLE link idle-dropped every ~50s. Both the app and the Mac node now
  emit a head beacon every ~15s (real G9 mesh traffic) so iOS doesn't time the link out.

## [0.51.5] — 2026-07-06

### Fixed — phone read 0 peers while connected: radio's node ref was released (2-node test)

The on-device diagnostic pinned it: the phone's BLE radio WAS fully connected (auth ok,
scanning, advertising, the Mac discovered + subscribed, both links up, radio counted 1
peer) but the mesh-node count stayed 0. Cause: the radio held the MeshNode with a WEAK
reference (ADR-0002 cycle-breaker), and on-device the node was released out from under it,
so linkUp's `node?.onPeerConnected` silently no-op'd — the radio counted the peer, the node
never did. The radio now holds the node strongly (app-lifetime; cycle broken in stop()).
Debug readout now also shows the node's own peer count to confirm parity.

## [0.51.4] — 2026-07-06

### Added — live BLE diagnostics on the Network screen (on-device 2-node test)

A mono debug line on the Network screen shows the phone radio's live state — Bluetooth
authorization, scan/advertise status, and counters for discovered/connected/subscribed +
per-role link counts + peer count. Makes the phone's Bluetooth visible so we can see why
the phone isn't counting the Mac node as a peer (new `meshDebug` bridge).

## [0.51.3] — 2026-07-05

### Fixed — mesh peer count crashing to 0 despite a live link (real 2-device test)

First real 2-device test (Andjroo's iPhone ↔ the Mac mini node) surfaced it: a pair links
TWICE over BLE (each device is central to the other) under the SAME peer id, so a flap on
either directed link fired onPeerDisconnected and dropped the peer entirely — the phone
read "offline · 0" while the Mac still saw 1. Both radios now REFERENCE-COUNT the two
directed links: onPeerConnected fires only on the first link up, onPeerDisconnected only
when the last link drops (per-role dedup so re-fires don't miscount). The peer count now
holds steady through the connection churn.

## [0.51.2] — 2026-07-05

### Added — the funding verifier PROVEN live on Amoy (#72 tail)

- **`examples/live_amoy_verifier.rs`** (behind `polygon-gateway`): drives
  `polygon_verifier::PolygonHtlcVerifier` — the real-chain G1 funding gate — through all three
  verdicts against the deployed HTLC. A real escrow reads **`Found`** with a depth that GROWS
  across reads (2 → 8), `require_funded` flips to PASS at the testnet USDC policy depth (5); a
  wrong-hashlock expectation reads **`Mismatch(Hashlock)`**; after `withdraw(S)` it reads
  **`Absent`** (a resolved slot is not funding — the stateless re-check). Receipts in
  `docs/swap/AMOY.md`.
- **Recorded RPC gotcha**: the public Amoy endpoint caps `eth_getLogs` to ~50 blocks; the
  example anchors its scan at the funding block and preflights the query loudly (the verifier
  itself is fail-closed, so a too-wide range would otherwise silently read `Absent`). Production
  verifier wiring pages from the deploy block in cap-sized chunks.
- The offline logic proof landed in #132; this closes the LIVE half of the verifier. Still open
  on #72: NIM/BTC gateway verifiers + wiring a real verifier into the production participant path.

## [0.51.1] — 2026-07-05

### Added — invite friends to the mesh (growth loop)

A mesh is only as useful as it is populated, so the app now makes it easy to pull people
in. An "Invite friends to the mesh" button anchors the Network screen's "you ARE the
network" card (and a matching side-menu row), opening the NATIVE share sheet (new `share`
bridge → UIActivityViewController) with a friendly message + the install link. Falls back
to navigator.share, then clipboard. Localized in all five languages.

## [0.51.0] — 2026-07-04

### Added — G7 (#78) IMPLEMENTED: relayer-sponsored (gasless) funding, live on Amoy

- **`contracts/src/NimmeshForwarder.sol`** — the minimal hand-rolled ERC-2771 forwarder
  (ADR-0008): EIP-712 `ForwardRequest` (struct calldata — ten flat args blew the EVM stack),
  strictly sequential nonces, required deadline, target failures REPORTED not bubbled
  (`Forwarded` event, nonce burns either way), EIP-150 under-gas guard so a stingy relayer
  can't fake a target failure. 8 new Foundry tests (25 total): attribution, replay/expiry/
  tamper/forgery rejection, honest `verify`.
- **The clarifying insight**: caller-open `withdraw`/`refund` (ADR-0007) already make claims
  gasless — the forwarder exists to attribute the FUNDER (`newSwapWithPermit` binds funder =
  `_msgSender()` and the permit verifies against it). One contract, gasless both directions.
- **`evm_forward`** (Rust, behind `polygon-leg`): `ForwardRequest` typehash + EIP-712 digest
  (binds the wrapped calldata) + the `execute` dynamic-tuple calldata builder — all
  byte-anchored against `cast` vectors.
- **Deployed to Amoy**: forwarder `0x94618C…67e1` + a fresh forwarder-bound `NimmeshHtlc` v2
  `0xb3B370…6736` (v1's forwarder is immutable `0x0` by design). **Live gasless proof**
  (`examples/live_amoy_gasless_swap.rs`): an in-process-derived user with zero POL and account
  nonce 0 funded and settled a real escrow — nonce and POL still 0 at the end; the chain
  attributed the funder to the signer. Receipts: `docs/swap/AMOY.md`.
- ADR-0006's implementation half is done → #78 closes; S4 fully closed.

## [0.50.0] — 2026-07-03

### Added — G6 (#77): the USDC HTLC is LIVE on Polygon Amoy + the real-RPC round-trip

- **Deployed `NimmeshHtlc` to Amoy** — `0xaaCa309B5EF3e57D3f206220F230F5cB2562F7f3`, escrowing
  Circle's canonical Amoy USDC (`0x41E9…7582`), ERC-2771 forwarder disabled for now (the G7
  implementation deploys one). Deployment record + operations notes: **`docs/swap/AMOY.md`**.
- **`examples/live_amoy_usdc_htlc.rs`** (behind `polygon-gateway`) — the round-trip against the
  REAL chain, every byte from our own stack (`evm_abi` calldata → `evm_rlp` EIP-155 legacy tx →
  `evm_signer` k256 → `polygon_gateway` JSON-RPC): the live contract's `swapIdFor` byte-matches
  `usdc_swap_id`; `approve` → `newSwap` → `getSwap == Live`; `withdraw(S)` verified by the
  **SHA-256 precompile on-chain** → `Claimed`; premature `refund` REVERTS, post-timelock
  `refund` → `Refunded`; USDC fully round-trips home. Gas hygiene learned from a real Amoy fee
  spike (84 gwei suggested vs ~25 baseline): the node's suggestion is clamped and the whole gas
  budget is preflighted against the new balance read.
- **`polygon_gateway::get_balance`** (`eth_getBalance`) — request/parse through the same
  offline-tested codec (`parse_quantity_u128`: wei balances overflow `u64`), mirrored on
  `MockPolygonRpc`.
- CI is untouched: the example needs `required-features` + env + a funded key and never runs in
  the gate.
- Size-guard fold-in: `polygon_gateway.rs` crossed 800 once `get_balance` joined #132's log/head
  codecs — its offline test suite moves to a `polygon_gateway_tests.rs` child module (`#[path]`,
  private access preserved).

## [0.49.24] — 2026-07-03

### Changed — the Network map uses the wallet's REAL world bitmap (Andjroo)

The hand-drawn land mask is replaced with the real wallet's own NetworkBitMap
(nimiq/wallet src/data/NetworkBitMap.ts, 129x52 grid, 2080 land cells, column-staggered
like upstream) — the map is geographically correct now, same source of truth as
wallet.nimiq.com's network map. Your gold node sits in the central US with the peer
cells and arcs around it; pan, momentum and live highlights unchanged.

## [0.49.23] — 2026-07-03

### Fixed — the map drag starts reliably now (Andjroo)

Two causes for "hard to get moving": the drag surface was only the small map patch, and
WKWebView's own gesture recognizer sometimes stole the first touches from the pointer
stream. Now the WHOLE navy card is the drag surface (nothing else on it is tappable),
driven by raw touch events with preventDefault so iOS can never intercept; the mouse
path stays for desktop. Same momentum feel.

## [0.49.22] — 2026-07-03

### Fixed — the map pan feels native now (Andjroo)

The drag was raw 1:1 with a dead stop on release. It now tracks flick velocity
(low-pass filtered) and continues with a momentum fling + friction after you let go,
like a native scroller — quick flicks glide across the world.

## [0.49.21] — 2026-07-03

### Fixed — the Network map pans by touch for real now (Andjroo)

WKWebView was swallowing native horizontal scrolling on the map area, so swipes did
nothing on device. The map now pans via pointer DRAG handled in JS (the same mechanism
as the swap bar's handle, proven on device), with touch-action pinned so iOS hands us
every touch. Also guards the sizing pass against a zero-height first frame.

## [0.49.20] — 2026-07-03

### Fixed — the Network map pans again + the full world fits its corner (Andjroo)

WebKit collapses a width:auto SVG to its container, which killed the horizontal pan and
squeezed the map (North America looked cut off). The map's intrinsic width is now set
explicitly from the viewBox aspect when the screen opens: the full north-south extent
fits the corner area and the map scrolls left-right, opening centered on your node.

## [0.49.19] — 2026-07-03

### Fixed — the Network map sized to its corner + the explainer auto-opens (Andjroo)

- The world map now sits in the TOP-RIGHT area beside the stat stack (the original
  wallet composition) instead of spanning the whole card — clear of the stats and of
  the explainer. Still dense, still pannable, still centered on your gold node.
- The "With nimmesh, you ARE the network" card now opens AUTOMATICALLY (closable with
  its x; the info button brings it back).

## [0.49.18] — 2026-07-03

### Changed — the Network screen, wallet-shaped: one screen, dense pannable map (Andjroo)

Reshaped to the real wallet's network screen per Andjroo's captures:

- ONE SCREEN, no scrolling: the card fills the viewport; the explainer is now an in-card
  overlay opened by the info button instead of a block you scroll to.
- The map is 2x denser (1318 cells, 78x39 grid with organic eroded coastlines), fills the
  card BEHIND the stat stack like the wallet, and SWIPES left/right (wider than the
  screen, opens centered on your node).
- Your node is the wallet's GOLD hex; nearby peers are light-blue hexes with thin arcs
  drawn from you to each active peer (live counts).

## [0.49.17] — 2026-07-03

### Fixed — the hex world map no longer reads as cropped (Andjroo, round 2)

The map's top rows were near-solid strips of hexes, which read as an image sliced off
mid-continent. The land mask now TAPERS at the top — a fringe of scattered arctic
islands, then a broken coastline, then the continents (171 cells) — so the map has a
natural silhouette instead of a torn edge.

## [0.49.16] — 2026-07-03

### Fixed — the hex world map's top row was cut off (Andjroo)

The map was bottom-anchored (absolute), so on taller screens it grew up underneath the
TX TIME stat and its top rows hid behind the text. It now flows below the stats in
normal layout — no overlap at any screen size (verified at 390x844 and 430x932).

## [0.49.15] — 2026-07-03

### Changed — the Network screen's hexagons became a real hex WORLD MAP (Andjroo)

The decorative random hex scatter is now a recognizable hexagon world map spanning the
stat card (the wallet's network-map concept, 162 cells from an equirectangular land
mask), and it shows LIVE mesh data: your hex lights light-blue when the radio is on and
one green cell lights per nearby peer (counts real; positions abstract).

## [0.49.14] — 2026-07-03

### Fixed — the hamburger icon (Andjroo)

The menu icon's three lines now DECREASE in length like the wallet's (long, medium,
short) — ours had the middle line longest. Fixed on both the home and Network topbars.

## [0.49.13] — 2026-07-03

### Fixed — Andjroo's dock + type round

- **The mesh line is back, short form**: the green mesh triangle + "mesh offline · 0
  nearby" above the action bar — clearly the mesh edition, without the core/head debug
  tail (that detail lives on the address banner + the Network screen).
- **The big total matches the wallet's weight** (regular, not bold — the $ was too thick).
- **The Help pill clears the dock** (safe-area-aware positioning; it was half-buried
  under the action bar on device).
- **Action-bar spacing** at the wallet's metrics (tighter gap, small bare QR at the
  right edge); dead swap-circle styles removed.

## [0.49.12] — 2026-07-03

### Removed — the action bar's swap circle (Andjroo)

The bottom bar is the wallet's exact trio now: Receive | Send | scan. Swap keeps its two
wallet-native entries — the swap circles between the home's asset cards and the side
menu's Swap pill.

## [0.49.11] — 2026-07-03

### Removed — the bottom mesh status line (Andjroo)

The `mesh offline · 0 nearby · core X · head N` line above the action bar is retired:
post-rebuild, the mesh state lives where the design wants it — the address screen's green
mesh banner and the Network screen (both still fed live by the same beat). The bottom
dock is action-bar-only now, like the wallet's; the Help pill moved down accordingly.

## [0.49.10] — 2026-07-03

### Changed — the swap sheet IS the wallet's "Swap Currencies" modal (Andjroo's captures)

Matched to Andjroo's live wallet captures of the real Swap Currencies modal:

- Title "Swap Currencies"; the pair as the wallet's dropdown pills (NIM ⇄ BTC/USDC — the
  right pill toggles the asset); the fee line underneath ("0.00 $ fee · <your balance>").
- The balance bar is now ADDRESS-COLORED like the wallet (the left segment takes your
  identicon's color, read from the rendered identicon; navy fallback) and the connector
  curves between the labels and the bar ends are back (verbatim upstream paths).
- The wallet's dual amount boxes: −NIM (editable, auto-width) with fiat under, +BTC/USDC
  in the green box with green fiat under — the incoming amount is INDICATIVE from live
  CoinGecko prices (NIM+BTC+USDC fetched together); the "rate agreed with your partner"
  note and the simulation label stay honest.
- CONFIRM (uppercase, the wallet's) replaces "Find a swap partner"; localized ×5.

## [0.49.9] — 2026-07-03

### Changed — the swap sheet gets the wallet's SwapBalanceBar (Andjroo)

The two-circle "You send / You receive" pair is replaced with the REAL wallet's
swap-balance-bar (the registry's pixel-verified port of the wallet's signature swap
element), fully live:

- Your identicon + wallet name + live NIM balance on the left; Bitcoin or USD Coin on the
  right (the asset toggle re-skins the bar with the component's native colors).
- DRAG the white handle to choose how much NIM to swap — the amount field follows; type
  an amount and the bar follows. The incoming side renders as the wallet's diagonal
  hatch; the percent scale updates live.
- The connector curves are omitted honestly (upstream precomputes them per split) rather
  than approximated.
## [0.49.8] — 2026-07-03

### Added — the gateway-backed USDC funding verifier (#72 tail, slice 1 — offline half)

- **`polygon_verifier`** (behind `polygon-gateway`): the first REAL-chain implementation of the
  G1 funding-verification seam. `PolygonHtlcVerifier` finds `NewSwap` escrows on the deployed
  `NimmeshHtlc` **indexed by our recipient** (`eth_getLogs` topic 3), requires the hashlock
  match + a `getSwap` **Live** state (a claimed/refunded slot is not funding), takes the
  deepest live candidate (the `LedgerVerifier` mirror), and reports depth from
  `eth_blockNumber` — so `require_funded`'s per-chain `ConfirmationPolicy` floor and the
  stateless reorg re-check (#74/G3) apply unchanged on the real chain. **Fail-closed**: any
  RPC failure reads `Absent` — a transport blip can delay a swap, never authorize one.
  Logic tested offline against a `PolygonReads` fake (found/depth, reorg-reburial refusal,
  wrong-hashlock mismatch, resolved-slot, deepest-wins, transport/foreign-leg/malformed-
  recipient fail-closed) + the `NewSwap` topic-0 cast vector.
- **`polygon_gateway`**: `eth_blockNumber` + `eth_getLogs` request/parse codecs (`EvmLog`) with
  fixture tests, and the two `HttpPolygonRpc` methods.
- Still OPEN on #72: the live Amoy proof of this verifier (rides the G6 deployment) and the
  NIM/BTC gateway verifiers.

## [0.49.7] — 2026-07-03

### Added — the Rust side of single-transaction permit funding (feeds G6/G7)

- **`evm_permit`** (behind `polygon-leg`): hand-rolled EIP-2612/EIP-712 pre-images — the
  canonical typehashes (asserted against the published constants), `eip712_domain_separator`,
  `permit_digest` (bound to every field), and `permit_sig_v` (27/28 — an EIP-712 signature,
  not the EIP-155 tx `v`). Digest vectors cross-derived with `cast keccak` on the Foundry
  `MockUsdc` domain fields. Live-path guidance: read the token's `DOMAIN_SEPARATOR()` rather
  than rebuilding it (Amoy USDC is name "USDC", version "2").
- **`evm_abi`**: `htlc_new_swap_with_permit` calldata (selector `0x0dc15831`, 8 static words,
  byte-layout-tested) for the contract's single-tx funding entry point, plus the two permit
  read builders `erc20_domain_separator()` (`0x3644e515`) and `erc20_nonces(owner)`
  (`0x7ecebe00`). The app can now fund the USDC escrow with **no separate approve tx** once it
  signs the permit digest — the on-chain half landed with the G5 contract (ADR-0007).

## [0.49.6] — 2026-07-03

### Changed — size headroom: extract `swap_node.rs`'s test-only hooks (loop maintenance)

- `swap_node.rs` sat at 799/800 (flagged since #116). Its four `#[cfg(test)]`-only items —
  `IntentMetricsSnapshot`, `IntentMetrics::snapshot()`, `swap_phase`, `start_swap` — move to a
  `swap_node_test_hooks.rs` **child module** (`#[path]` + `pub(crate)` re-exports, so every
  caller keeps its `crate::swap_node::…` path and the hooks keep their access to the module's
  private internals). Behavior-neutral — production code untouched.

## [0.49.5] — 2026-07-03

### Changed — REBUILD Phase 5: the final diff-everything pass

Captured every screen (home, address detail, side menu, account modal, settings, network,
receive, send, swap, language) against its wallet reference and closed the visible gaps:

- **The privacy eye** next to TOTAL BALANCE (the wallet's): one tap masks every amount
  (total, cards, address header, account modal), persists across launches.
- The big fiat total at the wallet's size (58px); asset cards at the wallet's 16px radius.

The rebuild contract (docs/REBUILD.md) is complete: all five phases shipped.

## [0.49.4] — 2026-07-03

### Added — REBUILD Phase 4: the mesh-native Network screen

The side menu's Network row now opens a full navy Network screen on the wallet's own
network-screen layout (reference: sweep/06-network.png), with OUR numbers:

- The dark stat card: NIM pill, MESH (live state + glyph), CONNECTED TO (live nearby
  peers), FEE $0/tx, TX TIME 1-2 sec, over a hexagon world-map motif.
- Top bar: hamburger (menu), "Back to addresses" pill, and an info button that brings
  back the explainer.
- The explainer card, rewritten for the mesh: "With nimmesh, you ARE the network" -
  relaying signed payments phone to phone over Bluetooth, settling on-chain when any
  relay reaches the internet. Localized in the five languages.

## [0.49.3] — 2026-07-03

### Changed — REBUILD Phase 3: the account modal

The account sheet now has the wallet's account-modal layout (reference:
sweep/andjroo-account-modal.png): identicon + wallet name + total fiat in the header, then
Backup (the hub), NEW Rename (native prompt; the name updates on the home card, the side
menu, the address header and the modal, persists across launches), NEW Export History
(CSV via the share sheet, clipboard fallback), and Log out. Save Login File / Change
password / Add account intentionally omitted (nimmesh = Face ID + backup codes, one
wallet). All new strings localized in the five languages.

## [0.49.2] — 2026-07-03

### Changed — REBUILD Phase 2: the navy side menu

The hamburger now opens the wallet's navy slide-out menu (reference:
sweep/andjroo-side-menu.png):

- The nimmesh logo, the 24H chip, and REAL NIM + BTC 24h sparkline charts (CoinGecko
  market_chart in the language's currency, ~10 min cache, honest-empty offline) with
  trailing-symbol prices and green/red percent change.
- The portfolio donut (gold ring, NIM 100% / USDC 0% / BTC 0% legend) + the Swap pill
  (opens the swap sheet). Buy/Sell intentionally omitted (no fiat ramps).
- The account row (identicon + name -> account sheet), Network (honest coming-soon until
  P4), and Settings.
- NEW Settings sheet: language (flag + opens the language sheet), Backup (-> the backup
  hub), Log out, and the version line. The provisional account pill is gone from the home
  chrome (the menu provides account access now); the language flag pill STAYS top-right
  (Andjroo's keep).

## [0.49.1] — 2026-07-03

### Changed — REBUILD Phase 1: the wallet's portfolio home + address drill-in

The app now has wallet.nimiq.com's information architecture (references:
sweep/01-home-overview + andjroo-home-activated-assets + andjroo-address-detail-staked):

- **Home = the portfolio overview**: hamburger top-left (opens the account sheet until the
  P2 side menu lands), the language flag pill top-right (the wallet's globe slot — Andjroo's
  keep), TOTAL BALANCE eyebrow + the big fiat total (trailing-symbol format like the
  wallet), the NIM asset card with the identicon address row (name, NIM, fiat, chevron),
  Bitcoin + USD Coin cards with the wallet's three swap circles (each preselects its pair
  and opens the swap sheet), the navy Help pill.
- **Address detail (drill-in)**: back + "Search transactions" pill + more-menu, the
  account-header (identicon, name, balance, faded copyable address, fiat), and — in the
  wallet's staking-banner slot — the green MESH BANNER (live state: "mesh meshed · 2
  nearby / Relays payments phone to phone, offline"). Transactions now group like the
  wallet: THIS MONTH, then month + year.
- The mesh status line + action bar stay on both views (Andjroo's keep).
- Fiat everywhere now uses the wallet's trailing-symbol convention ("64.02 $", "0,44 €").

## [0.49.0] — 2026-07-02

### Added — the real USDC HTLC contract (G5, #76) — Foundry unblocked

- **`contracts/src/NimmeshHtlc.sol`** — the Solidity escrow the Rust model
  (`swap_usdc_leg.rs`) has mirrored in sim since P2, now real: `newSwap` (byte-compatible with
  `evm_abi::htlc_new_swap`), **`newSwapWithPermit`** (EIP-2612 single-transaction funding,
  front-run-tolerant — closes S4's approve→transferFrom race), `withdraw(S)` on the **SHA-256
  precompile** (the cross-chain lock), `refund`, keccak-derived swap-id single-occupancy, and
  a minimal immutable-forwarder **ERC-2771** seam for relayer-sponsored funding (ADR-0006).
  `withdraw`/`refund` are caller-open with fixed payouts (the self-funded fallback needs no
  forwarder machinery). Semantic edges recorded in **ADR-0007**.
- **Foundry suite** (`contracts/test/`, self-contained — no submodules): escrow/claim/refund,
  both boundary seconds (claim ≤ timelock, refund > — matches the Rust model's code; the stale
  module doc-comment in `swap_usdc_leg.rs` is fixed to say so), keccak-lock rejection,
  duplicate-slot + double-resolve rejection, permit + front-run tolerance, forwarder
  attribution. **Byte-match anchor**: the published vector `0x81137ded…31e0` is asserted by
  both the Solidity test and a new Rust test (`usdc_swap_id_matches_the_contract_vector`).
- **CI**: new `contracts (solidity)` job (pinned Foundry v1.7.1) — `forge fmt --check` +
  `forge test`.
- Deployment to Amoy + real-RPC integration = **G6 (#77)**, still gated on an RPC URL + funded
  testnet key.

## [0.48.5] — 2026-07-02

### Changed — the real loading animation (Andjroo's hub capture)

The launch splash now IS the hub's loading page: the official Nimiq hexagon
loading-spinner (the pixel-verified registry component — two-tone stroke, the 4s dash
animation, navy via currentColor) centered on the light page with the nimmesh logo
top-left, replacing the static gold hexagon.

## [0.48.4] — 2026-07-02

### Changed — the scanner, wallet-exact (Andjroo's captures)

Rebuilt the native QR scanner to match the real wallet's scanner screen for screen:

- **Corner brackets** framing the scan area — grey while waiting for permission, NIMIQ
  GOLD over the live camera feed (rounded caps, the wallet's geometry).
- **White Cancel pill, bottom center** (was a translucent top-left button).
- **The navy "Unblock the camera" state**: a missing camera permission no longer makes the
  scan button silently do nothing — the scanner opens on the wallet's navy screen with
  "Unblock the camera for nimmesh to scan QR codes." / "Grant camera access when asked.",
  and after a denial the hint becomes a light-blue "Open Settings" deep link.
- All scanner strings localized in the app's five languages (native side reads the
  persisted language).

## [0.48.3] — 2026-07-02

### Docs — G7 gas-abstraction ADR (#78): relayer-sponsored claims (EIP-2771) + mandatory self-funded fallback

- **ADR-0006** records the gas model for the USDC leg: primary path = relayer-sponsored
  meta-transactions (EIP-2771 trusted forwarder — the recipient signs the `withdraw` intent,
  the relayer submits and pays the MATIC, the contract recovers the signer via `_msgSender()`),
  with the plain self-funded `withdraw(S)`/`refund()` as a mandatory, forwarder-independent
  fallback. Trust surface captured: a relayer can grief/censor but cannot steal; handing `S`
  to a relayer is treated as a reveal, so the failover to self-funding must land inside the
  claim window (ties into ADR-0004's reveal-deadline guard).
- **Decision only, marked revisitable pending Amoy validation — implementation stays gated on
  G6 (#77)**; the G5 contract (#76) picks up the ERC-2771 requirements. Options catalogue:
  `docs/swap/USDC-GAS.md`.

## [0.48.2] — 2026-07-02

### Changed — size headroom: extract `node.rs`'s inline test mod (loop maintenance)

- `node.rs` sat at 799/800 lines — one under the size-guard ceiling — with more FFI surface
  (G9 slice 3) still to land. Its lone inline `#[cfg(test)] mod tests`
  (`id_helpers_truncate_and_pad`) moves to a `node_tests.rs` sibling registered in `lib.rs`,
  matching the `swap_session_tests.rs` / `swap_coordinator_tests.rs` convention;
  `to_sender_id` / `to_tx_id` widen to `pub(crate)` for the sibling. Behavior-neutral — no
  production logic touched. `node.rs` is now 786 lines.
- The deferred cycle-log lines for #114 (G9 slice 1) and #116 (G9 slice 2) are folded into
  `docs/swap/INTEGRATION-LOOP.md`, replacing the stale "G0 is next" placeholder.

## [0.48.1] — 2026-07-02

### Added — live fiat value + tap-to-copy address (Andjroo)

- **The home balance now shows its fiat value in the LANGUAGE'S currency** (the fleet flag
  mapping: EN→USD, ES→MXN, DE/FR→EUR, PT→BRL), price from CoinGecko cached ~5 min and
  refreshed on the balance beat, formatted locale-correctly via Intl ("$0.44" / "0,44 €" /
  "R$ 2,48"). Offline the line stays empty rather than faking a number.
- **The home address is a real Copyable** (the registry component, vendored): tap → the
  full spaced address on the clipboard, light-blue tint + the wallet's blue "Copied"
  tooltip (~800ms, upstream timing), keyboard-accessible, localized label.

## [0.48.0] — 2026-07-02

### App integration — active-swap match list over UniFFI (G9 slice 2, #80)

The native app can now **see its in-flight swaps** across the FFI boundary, not just the aggregate
discovery counters (slice 1).

- **`MeshNode::active_swaps() -> Vec<FfiSwapMatch>`** (`#[uniffi::export]`) — one `FfiSwapMatch`
  (`swap_id` in lowercase hex + current `FfiSwapPhase`) per swap this node participates in, sorted by
  id for a stable order. Reads the observable phase mirror (`ctx.swaps`), so it is read-only and
  non-blocking; a plain relay that holds no `SwapSession` returns an empty list.
- **`FfiSwapPhase` is now a single, always-compiled FFI type.** It moved out of the `bitcoin-leg`
  `swap_ffi` facade into the always-compiled `swap_intent` module (beside slice 1's
  `FfiIntentMetrics`), so the discovery-only app build can read a swap's phase without the chain-leg
  feature. The `bitcoin-leg` `SwapEngineHandle::phase` reuses the same enum — no duplicate mirror, and
  `SwapPhase` (`swap.rs`) stays a pure domain type with no FFI derive.
- **`FfiSwapMatch`** (`uniffi::Record`) — the new match record, also in `swap_intent`.
- **Bindings regenerated** (Swift + Kotlin) — `activeSwaps()`, the `FfiSwapMatch` record, and the
  relocated `FfiSwapPhase` appear in both and compile. (Generated bindings are git-ignored artifacts.)
- **Tests:** a settled initiator lists exactly her one swap with `swap_id` = hex and phase `Settled`
  over FFI; a blind relay that holds no session lists none.

Remaining G9 slice (follow-up): the **advertise/stop-advert** write API needs a production
participant/session path (all `new_participant*` are `#[cfg(test)]`), co-developed with G10/G11.

## [0.47.1] — 2026-07-02

### Changed — real coin logos in the swap pair (Andjroo)

The lettered placeholder circles are gone: NIM is its gold hexagon, and BTC/USDC are the
wallet's official coin SVGs (verbatim `BitcoinIcon`/`UsdcIcon`, canonical brand colors via
currentColor). The receive coin switches with the asset toggle.

## [0.47.0] — 2026-07-02

### App integration — discovery metrics over UniFFI (G9 slice 1, #80)

First slice of G9 (discovery over UniFFI). The native app can now **read its node's discovery
state** across the FFI boundary.

- **`MeshNode::discovery_metrics() -> FfiIntentMetrics`** (`#[uniffi::export]`) — a snapshot of the
  G42 discovery counters: intents `seen`, swaps `matched`, and the per-gate drop tallies
  (`dropped_rate`/`expiry`/`throttle`/`signature`) + `readvertised`. Read-only, non-blocking; a plain
  relay node that runs no `SwapSession` reports all-zero.
- **`FfiIntentMetrics`** (`uniffi::Record`, `u64` counts) lives in the always-compiled `swap_intent`
  module — discovery has no chain-leg dependency, so it is *not* behind the `bitcoin-leg` `swap_ffi`
  facade. The internal `usize` `IntentMetricsSnapshot` (consumed by `swap_health`) is unchanged; the
  FFI record is a distinct `u64` mirror (`IntentMetrics::ffi_snapshot`), matching the repo's `Ffi*`
  boundary-type convention.
- **Bindings regenerated** (Swift + Kotlin) — `discoveryMetrics()` + the `FfiIntentMetrics` record
  appear in both and compile. (Generated bindings are git-ignored build artifacts.)
- **Tests:** a matched NIM-giver reports `seen`/`matched ≥ 1` via the exported accessor; a plain relay
  reports all-zero (proving the reader never panics off the participant path).

Remaining G9 slices (follow-ups): expose the active-swap **match list** (needs an always-compiled
phase enum) and the **advertise/stop-advert** write API (needs a production participant/session path,
co-developed with G10/G11). This slice is the read/observability half.

## [0.46.1] — 2026-07-02

### Added — the mesh HTLC swap UI, slice 1 (Andjroo's new goal)

The user-facing face of the merged mesh-swap engine: a **Swap** entry in the action bar
(the wallet's official SwapIcon, round button between Receive and Send) opening a swap
sheet built on the wallet's swap anatomy plus the mesh-native step:

- **Setup**: NIM -> BTC or USDC pair (asset toggle), the real amount-input treatment, the
  honest peer-to-peer rate note ("agreed directly with your swap partner").
- **Discovery** — the NEW, mesh-native beat: "Finding a swap partner on the mesh", your
  intent relayed phone to phone (what Fastspot-based wallets don't have).
- **The HTLC phase timeline**, mirroring the real SwapEngine state machine: proposal sent ->
  partner accepted -> NIM HTLC funded -> partner HTLC funded (BTC/USDC) -> secret
  revealed -> settled, per-leg tags, green check dots.
- Clearly labeled **"Simulation: no real funds move yet"** — slice 2 wires the
  already-exposed SwapEngine FFI through the Swift bridge; slice 3 adds real signers
  (money-path gated).

All five languages; Playwright end-to-end; `nq lint` 0 errors.

## [0.46.0] — 2026-07-02

### Test harness — deterministic mesh-drain re-enables the discovery-stress tests (#84)

Three multi-node discovery-stress tests were `#[ignore]`d in CI because their convergence driver
(`pump_until`) raced a **wall-clock budget** (`CONVERGE = 10s`) against two background worker threads
(node job queue → `MockEther` delivery). On CI's 2-core runners under `cargo test --all` the CPU
oversubscribes, the workers starve, and a six-node scenario misses the budget — a flake, not a bug
(they passed with `--ignored` locally). This replaces the clock with a **fence-driven drain to global
quiescence** (ADR-0005) — no timing in the convergence path.

- **Two `#[cfg(test)]` FIFO barriers.** `MeshNode::fence()` (`Job::Fence`) blocks until the worker has
  run every earlier job *and its `radio.send`s*; `MockEther::fence()` (`EtherMsg::Fence`) blocks until
  the delivery thread has delivered every pending transmit into its destination queue. Because both
  block on a channel `recv` rather than spinning, the test thread yields the CPU to the workers — the
  opposite of the busy-poll that starved them.
- **Quiescence via one monotonic counter.** `MockEther::enqueued()` counts transmissions ever handed
  to the ether. The new `settle` helper fences all nodes → reads `enqueued` → fences the ether →
  fences all nodes, repeating until a full pass moves **zero** new transmissions = true global
  quiescence (race-free: any concurrent send bumps the counter and forces another pass, so it can
  never falsely report done). `drive_until` wraps it: poll every node's tick, `settle`, break the
  instant the target holds — a fixed round cap, never a clock.
- **Re-enabled (lib suite now 0 ignored):** `many_complementary_pairs_all_discover_and_settle`,
  `a_partitioned_pair_discovers_after_the_link_heals_within_budget`,
  `a_reconnected_peer_resets_the_re_advertise_budget_and_the_pair_settles`. Pass 10/10 locally and
  under 6 background CPU burners, sub-second — the flake is structurally gone.
- **Zero production-behaviour change.** The fences are `cfg(test)`; the only always-compiled addition
  is the ether's `enqueued` counter (one relaxed `fetch_add` per transmit on a test/example substrate
  the real BLE path never uses). Removes `pump_until`/`CONVERGE`.

## [0.45.0] — 2026-07-02

### Hardening — faster un-funded slot reclaim (G4 slice 2b, #75 → part of S5; closes G4)

The last piece of G4. `SwapCoordinator::is_stale` reaped an un-funded negotiation only once the head passed the far `T_A` (NIM) timelock — so a `Propose` flood of never-funded swaps could squat a node's concurrency slots for the whole `T_A` window (the S5 Sybil slot-jam).

- **Reap at the fund deadline, not `T_A`.** An un-funded swap is now stale once
  `head > T_B − min_claim_window_blocks` — the instant its own `fund()` would refuse `WindowTooShort`
  (the counterparty leg's claim window has closed), so it can never complete. In the fixtures that's
  `5000 − 1800 = 3200`, vs the old `10_000` — the slot frees ~3× sooner. Uses `T_B` (the shorter leg)
  with the same claim-window margin funding uses, keeping it consistent with the fund-time gate.
- **Funds-locked swaps are still never stale** (their refund path must stay tracked) — unchanged.
- **Tests:** coordinator `is_stale` fires at `3201` (not `3200`) and stays stale through `9_999` — well
  before the old `T_A` deadline — while a funds-locked coordinator is never stale; the session
  GC-tick boundary test updated to `3200`/`3201`. Node-worker head-beacon GC tests (head `10_001`) are
  past both thresholds and unaffected.

With this, **G4 (#75) is complete** (reveal-deadline guard 0.43.0 · dust/ms→s/doc 0.44.0 · this).

## [0.44.0] — 2026-07-02

### Money-path nits — dust limit, ms→s ceil, doc fix (G4 slice 2a, #75 → part of S6)

Three small mainnet-nits from G4, each with a regression test (the reveal-deadline guard shipped in 0.43.0; the un-funded slot-reclaim tweak is deferred to slice 2b):

- **BTC dust limit (`swap_btc_leg.rs`).** The payout after fees is now rejected below the standard
  546-sat dust limit (new `BtcError::DustOutput { have, min }`, `DUST_LIMIT_SAT = 546`), on both the
  claim and refund paths — a dust output is non-standard and would silently burn the swap's value. A
  payout exactly at 546 is allowed.
- **ms→s CLTV rounds up (`swap_ffi.rs`).** `T_B` (Unix-ms) → BTC CLTV (seconds) now **ceils**
  (extracted `cltv_seconds_from_ms`), not truncates: a floor'd CLTV was up to ~1 s *earlier* than the
  agreed `T_B`, shrinking the initiator's claim window. Ceiling keeps the BTC refund available no
  earlier than `T_B`.
- **Doc fix (`nimiq/htlc.rs`).** The `HtlcCreationData` layout comment wrongly called `timeout` a
  "u64 block height"; it is a Unix-**ms** timestamp (compared to `block_state.time`, proven live on
  testnet — as the field's own doc already said). Corrected.

## [0.43.0] — 2026-07-02

### Security — reveal-deadline liveness guard (G4 slice 1, #75 → part of S6)

`Swap::reveal_and_claim` advanced `BothFunded → Revealed` on role+phase alone, with **no check on the head**. The initiator reveals `S` by claiming the counterparty leg (timeout `T_B`); revealing too late risks the claim not confirming before `T_B` — letting the counterparty refund that leg *and* use the now-public `S` to take the other leg. This slice closes that.

- **Gate** `reveal_and_claim(current_head, params)` on a new pure `assess_reveal_deadline`: refuse
  (`SwapError::RevealTooLate(RevealVerdict::DeadlineTooClose { have, need })`, phase unchanged, `S`
  never published) when `T_B − head < min_claim_window_blocks` — the same claim window the fund-time
  `WindowTooShort` check requires against that same leg. On refusal the node keeps `S` secret and
  refunds once `T_B` passes (worst case stays "refund, never theft").
- **Threshold choice:** the agenda framed this loosely as "within `Δ_safe` of `T_B`"; the correct
  quantity is the *claim window* before `T_B` (`Δ_safe = T_A − T_B` protects the responder's
  post-reveal NIM claim and is already enforced at fund time). Recorded in
  `docs/adr/0004-reveal-deadline-guard.md`.
- **Threaded through every reveal path:** `SwapCoordinator::claim_and_reveal(head, …)`,
  `SwapEngine::reveal_and_claim_btc(head, params)`, the mesh node, and the sim. **FFI signature
  change:** `SwapEngineHandle::reveal_and_claim_btc(head_ms, ladder)` — the native app must regenerate
  bindings and pass the current head + its ladder when revealing.
- **Telemetry:** `Swap::reveal_deadline_margin(head)` returns blocks-until-`T_B` for a node to log as
  the reveal window shrinks.
- **Tests:** pure `assess_reveal_deadline` boundary (safe at `window == need`, too-close one block
  past, saturates to 0 past `T_B`); `Swap` refuses a tight reveal and keeps the secret; a
  coordinator-level test proving the threaded `head` reaches the guard.
- **Deferred to G4 slice 2** (same issue #75): BTC dust-limit (≥ 546 sat), ms→s truncation
  (`swap_ffi.rs`), the `nimiq/htlc.rs:61` doc comment, faster un-funded slot reclaim. (`swap.rs` is
  now 792/800 lines — extract its tests to a `swap_tests.rs` sibling before adding more there.)

## [0.42.0] — 2026-07-02

### Security — per-chain confirmation-depth + reorg policy (G3 slice, #74 → part of S6)

Funding verification (G1/#72) already refused an on-chain HTLC that wasn't buried to a `min_confirmations` depth — but that depth was a single flat floor (`= 1`), wrong across chains and silent on reorgs. G3 makes the depth **per chain** and pins down "re-verify on reorg".

- **`ConfirmationPolicy`** (`swap_funding_verify`) carries one min-confirmation depth per chain and
  resolves it from the leg being verified: `required(Asset)` and
  `required_for_leg(leg, counterparty)` (the NIM leg uses the NIM depth; the counterparty leg uses
  BTC's or USDC's). Reuses `swap_intent::Asset` — no parallel chain enum. Builder +
  `Default = testnet_defaults`, so an un-configured node is safe-by-default (never zero-conf).
- **Testnet defaults** (deliberately low, mainnet-gated to re-tune): NIM `2`, BTC `3`, USDC/Polygon
  `5` — increasing with reorg risk. Never ship to mainnet (Phase 4 raises them).
- **Reorg = a property of a stateless gate**, not a new subsystem: `require_funded` re-runs on every
  `FundingProof`/tick with a fresh observation and holds no "already funded" memory, so a leg that
  reorgs below its policy depth is refused again (`TooShallow`), and an orphaned funding tx reads as
  `Absent` (`NotFundedYet`). Every subsequent money-path step re-runs the gate, so a reorg between
  funding and reveal is caught before the reveal. `LedgerVerifier` gained `reorg_to` / `orphan_all`
  to prove it. Continuous post-advance monitoring (rolling back a *funded* leg) stays with the
  gateway-backed verifier (#72 tail, gated on real chains).
- **Wiring:** `SwapSession` holds a `ConfirmationPolicy` + `counterparty_chain` (default testnet /
  BTC — the mesh path is BTC-shaped; USDC drives `SwapEngine` directly) and resolves the depth per
  leg in its `FundingProof` handler. The pure `require_funded` / `verify_and_observe_funding`
  signatures are unchanged (no coordinator call-site churn).
- **Tests:** policy per-chain defaults + leg resolution + builder; ledger reorg (deep→shallow refused
  again) + orphan (reads Absent); a session-level test proving the mesh gate applies the policy's NIM
  depth (2-deep refused, 3-deep advances). See `docs/adr/0003-confirmation-depth-reorg-policy.md`.

### CI — quiet two more wall-clock discovery-stress flakes (#84)

`a_partitioned_pair_discovers_after_the_link_heals_within_budget` and
`a_reconnected_peer_resets_the_re_advertise_budget_and_the_pair_settles` are `#[ignore]`'d in CI, like
their sibling `many_complementary_pairs_all_discover_and_settle` already was: they wait on a settle
completing within a wall-clock budget over the threaded `MeshHarness`, which blows `CONVERGE` under
`cargo test --all` on CI's oversubscribed 2-core runners (they pass locally and with `--ignored`).
Correctness stays covered by the deterministic e2e/adversarial suites; the sync-driven re-enable is
tracked in #84. Not a code change — no money-path impact.

## [0.41.0] — 2026-07-02

### Security — enforce authenticated swap Proposes (G2 slice 2b, #73 → closes S2)

The final S2 piece: a swap `Propose` is now **signed at origination and verified on receipt**, so a
relay can neither inject a proposal nor tamper with proposed terms in transit (the settlement-message
authentication gap the security review flagged).

- **Sign at the initiate flow.** A participant node holds its NIM identity behind an `EnclaveKey`
  seam (`SwapSession::with_propose_signer`); `swap_node::initiate_from_intent` authenticates each
  `Propose` under it (`SwapProposal::signing_bytes` → `to_signed_envelope`) before it floods. The
  seed never crosses the seam — only the public key + signature ride the wire.
- **Enforce at `recv_propose`.** `SwapCoordinator::recv_propose` now rejects any `Propose` whose
  wire signature does not verify under a NIM key that hashes to the Propose's own `nim_address`
  (self-certifying bind), *before* touching state — new `CoordError::UnauthenticProposal`. An
  unsigned, tampered, or forged (relay-re-signed) Propose never spins up a responder coordinator.
- **Adversarial tests.** `recv_propose` rejects unsigned + on-wire-tampered + forged Proposes and
  accepts a valid signed one; every mesh/session/discovery fixture now originates a key-derived,
  authenticated Propose. Coordinator tests extracted to `swap_coordinator_tests.rs` (file-size cap).
- **Scope note:** binding the Propose to a pre-known counterparty pubkey (full anti-MITM) isn't
  meaningful in the one-sided discovery model — a responder has no committed expectation of the
  initiator's identity — so the self-certifying signature (pubkey → `nim_address` + signed terms) is
  the delivered guarantee. Funding authenticity remains G1's on-chain check.

## [0.42.0] — 2026-07-02

### Added — the mesh HTLC swap UI, slice 1 (Andjroo's new goal)

The user-facing face of the merged mesh-swap engine (G-track): a **Swap** entry in the
action bar (the wallet's official SwapIcon, round button between Receive and Send) opening
a swap sheet built on the wallet's swap anatomy plus the mesh-native step:

- **Setup**: NIM -> BTC or USDC pair (asset toggle), the real amount-input treatment, the
  honest peer-to-peer rate note ("agreed directly with your swap partner").
- **Discovery** — the NEW, mesh-native beat: "Finding a swap partner on the mesh", your
  intent relayed phone to phone (this is what Fastspot-based wallets don't have).
- **The HTLC phase timeline**, mirroring the real SwapEngine state machine: proposal sent ->
  partner accepted -> NIM HTLC funded -> partner HTLC funded (BTC/USDC) -> secret
  revealed -> settled, with per-leg tags and green check dots.
- Clearly labeled **"Simulation: no real funds move yet"** on every screen — slice 2 wires
  the already-exposed SwapEngine FFI (initiator/responder, fund/observe/reveal/refund)
  through the Swift bridge; slice 3 adds the real signers (money-path gated).

All five languages; Playwright end-to-end (pair toggle, amount, search beat, full timeline
to settled); `nq lint` 0 errors.

## [0.41.2] — 2026-07-02

### Changed — the codes flow ENDING, keyguard-exact (Andjroo's captures, round 4)

- Code 2's copy is the real one: "Send this code to yourself using another email or
  messenger. For your safety, both codes must be stored separately."
- Confirm 2 asks the real question: "Did you send Code 2 with a different method than
  Code 1?" with "YES, ALL DONE".
- New final screen: **"All set up?"** — 5/5 progress, BOTH bubbles with their codes and
  green check circles, the light-blue-on-dark restore note ("If you ever lose this phone,
  you can restore your wallet by entering both codes."), and LET'S GO returning straight
  to the wallet (the codes path no longer detours through the generic done screen).

## [0.41.1] — 2026-07-02

### Changed — the account menu's backup entry is just "Backup"

Andjroo completed a backup and thought the flow was gone (the banner rightly disappears).
It never was: the account menu (identicon, top right) reopens the full hub anytime —
Face ID unlock, then re-view the words or the (deterministic, unchanged) codes. The menu
item was labeled "Backup recovery words", underselling that it opens BOTH backup types;
now simply "Backup". Re-entry verified in the harness with backedUp=true.
Also aligns the OTA payload with the 0.41.x version line (the swap track bumped 0.41.0).

## [0.40.0] — 2026-07-02

### Changed — backup codes, keyguard-exact (Andjroo's captures, round 3)

Matched his live keyguard captures of the "Send yourself two backup codes" flow:

- **Keyguard order**: unlock FIRST, then the intro (5-step progress bar).
- **The message-bubble illustration** (BackupCodesIllustrationBase, colors verbatim):
  code 1 = purple bubble (#693BC4→#8F3FD5, tail bottom-left), code 2 = red
  (#DC1845→#F33F68, tail bottom-right), numbered white circles, placeholder lines on the
  intro, the real code inside the bubble on the send screens, faded ghost for the other
  code, GREEN + check when confirmed.
- **The real copy**: "The codes combined grant access to your account…", orange "Anyone
  with both codes will have full access!" (two lines), "LET'S GO", "Send yourself Code 1",
  "COPY CODE 1/2", "How to send to yourself ›" (tip on tap).
- **Per-code confirm screens**: "Did you send Code 1/2 to yourself?" with
  "YES, CONTINUE TO CODE 2" / "YES, FINISH BACKUP" and the small "No, go back" pill;
  copying a code auto-advances to its confirm (the keyguard's copy-then-confirm rhythm).
- Fixed an unhandled promise rejection when the clipboard write is denied.

## [0.39.0] — 2026-07-02

### Added/Changed — unlock step + keyguard card shape (Andjroo's captures, round 2)

- **"Unlock your Backup"** — the missing keyguard step 2. A dedicated screen (big lock,
  UNLOCK capsule) that gates BOTH the words and the codes paths behind Face ID / device
  passcode (`LocalAuthentication`, new `authenticate` bridge method,
  `NSFaceIDUsageDescription`). Devices with no passcode pass through (nothing to unlock
  with). The progress bar is now 4 steps on both paths.
- **The flow is a CARD now, not full-screen** — like the real keyguard: light page with
  the nimmesh logo header, and the navy radial card pinned to the bottom with rounded
  top corners and a soft shadow (`.kg-card`, min-height 62vh, safe-area padding).
- **"Keep them safe." breaks onto its own line** in the words warning (all 5 languages,
  `white-space: pre-line`).

## [0.38.2] — 2026-07-02

### Fixed — backup flow buttons sat on the screen edge (Andjroo, on-device)

The flow footers (SHOW RECOVERY WORDS / codes / Done) had zero bottom padding and no
safe-area inset, so the CTA hugged the home indicator. All five footers now pad
`34px + env(safe-area-inset-bottom)`.

## [0.38.1] — 2026-07-02

### Changed — the 24-word backup, keyguard-EXACT (Andjroo's live captures)

Andjroo captured the real wallet.nimiq.com -> keyguard.nimiq.com flow on his phone; matched
it screen for screen:

- **New intro screen** — "There is no Password Recovery!" with the orange warning and the
  keyguard's three orange rules (paper-edit / copy / fire icons, verbatim assets).
- **Step progress bar** (green segments) across the whole flow, words AND codes paths.
- **Words screen** — title "Write these 24 Words on Paper", ORANGE "Anyone with these
  words can access your account!" warning, FILLED word tiles with zero-padded numbers
  (01..24) replacing the outline cells, uppercase "VALIDATE BACKUP" capsule. The copy
  button is gone (the keyguard has none — paper is the point).
- **Validate screen** — title "Validate your Backup", per-round ordinal question
  ("What is the 14th word?", localized), UPPERCASE word pills, exactly like the capture.
- The create-wallet flow gets the same words + validate treatment.
- Hub close is the modal's circled x.

## [0.38.0] — 2026-07-02

### Added — real backup fidelity: two backup types + the word check (Andjroo's ask)

Matched the real wallet's backup system (verified against wallet `BackupModal.vue` and
Keyguard `ValidateWords.js` / `BackupCodes.js` upstream sources):

- **The backup hub** (from the banner + account menu) is the wallet's BackupModal: navy,
  "Important: Create a backup", TWO types — "Send yourself two backup codes" (~3min,
  orange shield) and "Write down 24 recovery words" (~10min, green shield) — with the
  wallet's own MessagesIcon/WordsIcon/ShieldIcon artwork, and "Remind me later".
- **The word check.** After the 24 words (both the create flow and the backup flow),
  the Keyguard's ValidateWords quiz, behavior-exact: 3 rounds, the target drawn from a
  different third of the phrase each round, a giant target number, 6 alphabetized
  candidate words from the same mnemonic; a wrong pick flashes red + shakes, reveals the
  right word in green, and starts over; three greens pass.
- **Backup codes**, the wallet's XOR one-time-pad scheme done natively: code1 = HKDF-SHA256
  of the entropy, code2 = (version byte + entropy) XOR code1 — either code alone is
  useless, both together restore the wallet; 44-char base64 with the keyguard's `/`→`!`,
  `+`→`;` substitution; deterministic per wallet; checksum-verified on import
  (order-agnostic). PROVEN with 200 random round-trips in both orders + tamper rejection
  against the real BIP39 code. Onboarding gains "Use backup codes" restore.
- **The banner finally turns off.** `backedUp` persists (UserDefaults via the bridge) and
  now feeds the G19 nudge with the REAL state (it was hardcoded false); completing either
  backup type (or restoring from codes/words) silences it. Log-out clears it.
- Removed: the old checkbox self-attestation ("I have written down my words") — replaced
  by the actual check; the old standalone words viewer — replaced by the hub flow.
- Verified: Playwright end-to-end (hub, wrong-answer reset, quiz pass, codes flow, codes
  import in reversed order, banner off), `nq lint` 0 errors, Swift crypto round-trip.

## [0.37.0] — 2026-07-02

### Changed — Send + Receive refined against the real wallet (Andjroo's review ask)

Screenshot-diffed both sheets against the authentic mobile captures
(`references/screenshots/wallet-app/logged-in/`) and the pixel-verified `amount-input`
registry component:

- **Send:** recents now show the real wallet's placeholder name bars under the identicon
  hexagons; real breathing room before ENTER ADDRESS; the address grid is the live
  wallet's treatment (one rounded light box, full-width row hairlines, SHORT column ticks
  instead of full cell borders); the amount is the real `amount-input` component behavior
  (large, centered, grey at rest -> navy with a value -> light-blue focused, bold baseline
  NIM, content-tracking width); the footer is the wallet's layout ("Address unavailable?"
  line + "Create a Cashlink" pill — tapping it honestly says Cashlinks aren't in nimmesh
  yet, i18n'd).
- **Receive:** the request-amount field uses the same `amount-input` treatment (small
  variant); the CTA row matches the capture (pill centered, bare QR icon at the right
  edge instead of a circled button).
- Verified: Playwright screenshots (rest + scan-filled + receive), `nq lint` 0 errors.

## [0.36.1] — 2026-07-02

### Fixed — official QR icons (rule 15: never fabricate Nimiq icons)

Both QR glyphs were hand-drawn (caught by Andjroo). Replaced with the VERBATIM
`@nimiq/style` icon SVGs (the same ones `@nimiq/vue-components` exports):
- Send bar scan button → `scan-qr-code.svg` (corner brackets + QR, the icon Andjroo showed).
- Receive sheet QR toggle → `qr-code.svg` (what the wallet's ReceiveModal QrCodeIcon uses).

## [0.36.0] — 2026-07-02

### Fixed — the QR code, done right (on-device feedback, round 5)

Andjroo: "the QR code isn't working and is not correct." Both true, three causes:

- **It could fail to render at all:** qr-creator loaded from a CDN, so a slow or absent
  network left a permanently blank canvas. Now VENDORED at `webui/qr-code/qr-creator.min.js`
  (pinned 1.0.0) — offline-first like everything else.
- **It was not what the real wallet renders.** Checked the wallet upstream
  (`ReceiveModal.vue` / `QrCodeOverlay.vue` / `PaymentLinkOverlay.vue`): the wallet fills
  QR modules with SOLID nimiq-blue `#1F2348` (not the component's gradient default) and,
  with no amount, encodes the BARE formatted address (spaces included) — not a `nimiq:`
  URI. Ours now matches exactly; with a requested amount it encodes the `nimiq:` request
  URI (G18 semantics), also navy. Rendered at devicePixelRatio (qr-creator has no DPR
  handling) so it is crisp. Verified by DECODING the rendered canvas with jsQR: no-amount
  QR decodes to the exact spaced address; amount QR to `nimiq:<addr>?amount=<nim>`.
- **The scan button was a placeholder.** Now a real NATIVE camera scanner
  (`QrScanner.swift`: AVFoundation metadata scanning, full-screen preview, cancel, haptic
  on hit; `NSCameraUsageDescription` added; permission-checked; sim-safe). New bridge
  method `scanQr`. The page parses a bare NQ address (spaced/any case) or a `nimiq:`
  request URI (also inside a wallet link), validates the Nimiq base32 shape, fills the
  Send sheet's address grid + amount, and opens it. Playwright-verified end to end with a
  mocked scanner.

## [0.35.0] — 2026-07-02

### Changed — mainnet only + live balance/history (on-device feedback, round 4)

- **Testnet is GONE from the app** (Andjroo: "get rid of the testnet completely… mainnet
  needs to just be removed [as wording]"). The Send-sheet network toggle, the REAL-funds
  warning, every testnet/mainnet string, the faucet method, and the `currentNetwork` /
  `setNetwork` / `network` bridge methods are all removed. `NimiqRpc` is hard-pinned to
  mainnet (`rpc.nimiqwatch.com` — verified serving getBlockNumber, getAccountByAddress,
  getTransactionsByAddress AND sendRawTransaction). The Rust core's tests/tools remain
  testnet-pinned; only the app surface changed. NOTE: the Keychain account key keeps its
  historical `testnet-bip39-mnemonic` name on purpose — renaming it would orphan the
  wallet already stored on the device.
- **Live wallet data.** Andjroo sent 2 NIM and it never appeared: balance + history loaded
  ONCE at launch and never refreshed. Now `refreshWalletData()` polls every 10s while the
  app is visible, refreshes on returning to the foreground (visibilitychange), and fires
  immediately + 4s after a successful send (~1s blocks). History re-renders only when the
  (hash, confirmed) set actually changes, so no flicker.

## [0.34.0] — 2026-07-02

### Fixed — on-device feedback, round 3 (dialogs, flags, mainnet)

- **`confirm()` did not exist on device — every confirm-gated action silently no-oped.**
  A WKWebView has NO built-in JS dialogs; without a `WKUIDelegate`, `confirm()` returns
  false immediately. That is why "Delete it and start fresh", "Log out of this device" AND
  the mainnet switch all did nothing on the phone (they worked in browser tests, which have
  real dialogs). The bridge now implements the three `WKUIDelegate` dialog panels as native
  `UIAlertController`s (alert / confirm / prompt), presented from the top view controller,
  each completion handler called exactly once.
- **Flag hexagons are back in the language picker** (Andjroo: we shouldn't have dropped
  them). The fleet-standard flag-hex (nimiq-app-shell `buildFlagHex`: flag-icons artwork
  clipped into the Nimiq hexagon + faint grey flags-on-white edge, fleet mapping en→US,
  es→MX, de→DE, fr→FR, pt→BR) is vendored as five LOCAL svg files in `webui/flags/` —
  offline, no CDN. The pill shows the current flag + caret (fleet pattern); the language
  sheet rows show flag + native name.
- **The app now defaults to MAINNET** (Andjroo, 2026-07-02: "we need to be on mainnet" —
  the owner exercising the `MAINNET-GATING.md` gate for the real-funds phone test). A
  persisted toggle choice still wins; testnet is one tap away; the Send-sheet mainnet
  warning stays; the faucet stays testnet-only; the Rust core stays testnet-pinned.

## [0.33.0] — 2026-07-02

### Fixed — the chrome actually works (on-device feedback, round 2)

Andjroo's first Ad Hoc install surfaced three broken pieces of chrome. All three are now real:

- **Leftover wallet on a fresh install:** the wallet lives in the iOS Keychain, which
  SURVIVES an app uninstall — so a reinstall silently adopted the previous install's wallet
  and skipped onboarding. The bridge now detects a reinstall (UserDefaults is wiped with the
  app; a wallet present on the very first launch is a previous install's) and the UI shows a
  **"Wallet found on this device"** screen: keep it, or delete it and start fresh
  (create/import). Plus a permanent escape hatch: **Log out of this device** in the new
  account menu (confirms + reminds that without the 24 words the wallet is unrecoverable).
- **The connect-wallet pill did nothing:** it was fleet chrome for websites (Nimiq Hub
  connect) loaded from a CDN — meaningless inside an app that IS a wallet, and broken in the
  WKWebView. Replaced with an **account pill** (your identicon, top right) opening an
  **account sheet**: identicon + 3x3 address, copy address, backup recovery words, log out,
  and a network/core meta line.
- **The language pill did nothing:** it loaded from jsDelivr (dead offline — in an
  offline-first mesh app) and the page had no translations to switch. Now fully **local +
  offline i18n** in EN / ES / DE / FR / PT: static strings via `data-i18n`, dynamic strings
  (mesh line, send statuses, backup nudge, reachability, tx list, dialogs) via `T()`, choice
  persisted in UserDefaults over the bridge (localStorage in a plain browser), switch
  reloads for full consistency.
- **Onboarding logo fix:** every onboarding screen's gold hexagon referenced a gradient
  defined inside the (hidden) welcome screen — a display:none gradient is not a usable paint
  server, so the logo rendered empty. Each screen now carries its own uniquely-id'd def.

New bridge methods: `walletStatus` (exists + recovered), `resolveRecovered(keep)`,
`deleteWallet`, `getLang`/`setLang`. `Wallet.delete()` clears the Keychain entry.
Verified with Playwright at 390px across EN/ES/DE scenarios (mocked bridge; screenshots in
the PR): home, account sheet, language sheet, recovered screen, onboarding words, Send sheet.

## [0.32.0] — 2026-06-28

### Added — app icon + TestFlight signing (first build delivered)

- **App icon:** the verbatim Nimiq brand hexagon (gold gradient) on the canonical navy radial
  gradient (`--nimiq-blue-bg`), rendered 1024×1024 with no alpha (App Store requirement) into
  `Assets.xcassets/AppIcon.appiconset` (single-size). `project.yml` adds the catalog +
  `ASSETCATALOG_COMPILER_APPICON_NAME`. (TestFlight rejects a build with no icon, error 90022.)
- **Signing:** this account can't use Apple's cloud-managed signing, so `build-testflight.sh`
  now signs manually with a distribution cert + App Store profile (created once via the App
  Store Connect API) held in a dedicated `nimmesh-build` keychain. Archive + export both use
  manual signing; the script unlocks the keychain and keeps it searchable.

**First TestFlight build uploaded** (v0.31.0, build 202606282326) — the headless
archive → export → upload pipeline is proven end to end.

## [0.31.0] — 2026-06-27

### Added — TestFlight build + upload pipeline (remote / over-the-air delivery)

For working on the app while away from the Mini's network (USB/Wi-Fi dev installs can't reach a
phone on a different network, and free-signing expires in 7 days). TestFlight delivers builds
over the internet from anywhere and they last 90 days.

- `apple/scripts/build-testflight.sh`: headless archive → export → validate → upload via an App
  Store Connect API key (no Xcode GUI, no 2FA prompts). Marketing version from `Cargo.toml`,
  build number = timestamp (always unique). Flags: `--skip-rust`, `--no-upload`.
- `apple/project.yml`: `ITSAppUsesNonExemptEncryption = false` (only exempt crypto: Ed25519
  signatures + TLS) so TestFlight skips the per-build export-compliance question.
- `docs/TESTFLIGHT.md`: the one-time setup ($99 enroll → App Store Connect API key → app record)
  and the run loop.

Pending the owner's paid Team ID + API key before the first upload; dev builds (USB/Wi-Fi, free
team) keep working in the meantime.

## [0.30.0] — 2026-06-27

### Fixed — onboarding fit + no more mock-data flash (on-device feedback)

First device run surfaced two issues:
- **The design-mock home flashed** (static `752 NIM` + demo rows) before the bridge swapped in
  the real wallet. Added a **launch splash** (`#app-splash`, gold hex on the page background)
  that covers the static content until the bridge resolves the wallet state (onboarding, or real
  balance + history loaded), then reveals. 5s fallback so it never sticks (plain browser).
- **Onboarding was bottom-heavy / overflowed** (buttons crammed at the bottom, the 24-word grid
  didn't fit without zooming). The navy recovery-words sheet now **fills the height** below the
  logo chrome (grid at top, Continue anchored at the bottom via `margin-top:auto`), and the
  welcome/sheet get `env(safe-area-inset-bottom)` padding. Verified at 390px (taller phones get
  more room).

## [0.29.0] — 2026-06-27

### Changed — onboarding rebuilt to match the real Nimiq Wallet + Keyguard

The first hand-built onboarding was off-brand. Rebuilt it against the **real** wallet, using
references captured live via Playwright (per the nimiq-ui rule: match real screens, never
hand-guess):

- **Welcome** now mirrors `wallet.nimiq.com`: big title + a green "Create new wallet" gradient
  card + an "Import wallet" row (vs the real Create Account / Login rows).
- **Recovery words (create + backup)** and **import** now use the real Keyguard pattern: a
  **navy gradient sheet** with a **3-column numbered grid** (captured from the live testnet
  Keyguard `#recovery-words` screen). Import is 24 numbered inputs with paste-the-whole-phrase
  distribution + space/enter auto-advance; filled cells get a green ring.
- **Confirm address** now renders the real `address-display` component (the 3×3 `format-nimiq`
  chunked grid in Fira Mono), not a wrapping text line.
- Captured references saved to `nimiq-branding-cli/references/screenshots/wallet-app/` and
  `~/.nimmesh-refs/onboarding/` for future diffs.

## [0.28.0] — 2026-06-27

### Added — recovery-phrase wallet: create / import / back up (C1e)

The wallet is now a real, recoverable wallet instead of a silently-generated key. Onboarding
(create or import) runs before the home, and the key is derived from a Nimiq-standard 24-word
recovery phrase. This is the prerequisite for real funds: you import a wallet you've already
backed up, or create one and write the words down.

- `apple/.../Mnemonic.swift` (new): BIP39 (official 2048-word English list, bundled as a
  resource) + SLIP-0010 ed25519 derivation at Nimiq's path `m/44'/242'/0'/0'`. **All secret
  material stays native** (Swift + Keychain); the phrase/seed never crosses the Rust FFI.
  Verified byte-exact against the official BIP39 (Trezor) and SLIP-0010 ed25519 test vectors by
  `apple/scripts/verify-mnemonic-main.swift`.
- `apple/.../Wallet.swift`: stores the 24-word phrase in the Keychain (ThisDeviceOnly); derives
  the signing key from it (cached). `hasWallet` / `createNew` / `importMnemonic` /
  `recoveryPhrase`; no silent auto-create.
- `apple/.../WebHostView.swift`: bridge methods `walletExists`, `createWallet`,
  `importWallet`, `recoveryPhrase` (the phrase is shown only in-app for backup; never to Rust,
  the mesh, or a log).
- `webui/index.html`: onboarding overlay (welcome → create-with-24-word-grid + confirm, or
  import) and a "view recovery phrase" backup screen wired to the backup banner. The import flow
  shows the derived address with a **"check this matches your existing wallet before funding"**
  gate — the bulletproof correctness check for real funds.
- `apple/project.yml`: bundle the wordlist resource; bake in automatic signing + the owner's
  Personal Team so real-device builds work after `xcodegen generate` with no manual setup.

## [0.27.0] — 2026-06-27

### Fixed — the home shows REAL balance + REAL transaction history (no more mock numbers)

The first on-device run surfaced that the home was still wearing the design mock's hardcoded
data (`752 NIM`, `$0.46`, the demo `+25 / -2.5 / +110 000` rows). The wallet identity + network
were real, but the displayed numbers were never wired to the chain. Now they are.

- `apple/.../TestnetRpc.swift`: `NimiqRpc.transactions(address, max)` over
  `getTransactionsByAddress(addr, max, null)` (the public node serves history).
- `apple/.../WebHostView.swift`: a read-only `walletHistory` bridge method that normalises each
  tx (direction / counterparty / value / confirmed) for the UI; no key or seed crosses.
- `webui/index.html`: the home balance is driven by the live `walletBalance`, the transaction
  list by live `walletHistory` (with an honest "No transactions yet" empty state for a fresh
  wallet); the fake `$0.46` fiat is dropped (testnet has no market value); the backup nudge now
  reads the real balance instead of the mock `752`. The mock rows remain only as a no-bridge
  browser preview (the app replaces them via `#tx-list`).

### Fixed — real-device code signing

- `apple/project.yml`: scoped `CODE_SIGNING_ALLOWED/REQUIRED = NO` to the **simulator SDK only**
  (`[sdk=iphonesimulator*]`), so headless/CI simulator builds stay sign-free while real-device
  builds sign normally via Xcode automatic signing. (Device install was failing "executable is
  not codesigned" because the off-switch was unconditional.)

### Note — Keychain persistence is device-only (expected)

On the **simulator** the unsigned build has no entitlements, so Keychain reads/writes fail
(`errSecMissingEntitlement -34018`) and the wallet regenerates a key every launch. On a **signed
device** the Keychain works and the wallet (address) persists. A recovery-phrase backup/restore
flow is still TODO before real funds (`docs/DEVICE-TEST.md`).

## [0.26.0] — 2026-06-27

### Added — mainnet-capable network toggle (gated; default testnet)

The wallet can now point at **mainnet**, behind a deliberate in-app switch. This is the last build
step before the hardware phone test: everything up to a real-funds send is now wired, and the only
thing the app will not do autonomously is broadcast real mainnet funds (that is the phone test).

- `apple/.../TestnetRpc.swift`: `TestnetRpc` → **`NimiqRpc`** with an `isMainnet` toggle
  (`UserDefaults`, default `false`). `rpcURL` switches testnet ↔ mainnet (both nimiqwatch public
  nodes); `network` returns the matching Rust `NetworkId` the signer anchors to; the faucet stays
  testnet-only.
- `apple/.../WebHostView.swift`: bridge gains read-only `currentNetwork` + a deliberate `setNetwork`;
  `sendTransaction` anchors to `NimiqRpc.network`; `fundFromFaucet` is guarded `!isMainnet`.
- `webui/index.html`: a Testnet/Mainnet segmented control on the Send sheet with a confirm-on-switch
  and a "⚠ Mainnet sends REAL funds" warning; the mesh bar + toggle reflect the selected network
  (default testnet). Resting state verified testnet-active/no-warning; mainnet-active shows the warning.
- ATS exception (added with the live send path) lets the app reach the testnet/mainnet RPC over HTTPS.

**Gating (unchanged):** mainnet is never the default and the app never auto-sends. A mainnet send is
always a user action on a real device with real funds. `docs/DEVICE-TEST.md` is the runbook.

## [0.25.0] — 2026-06-27

### Added — G5: the native CoreBluetooth BLE shim (the offline mesh radio)

The offline mesh now has a real radio. `apple/NimmeshApp/Sources/BleMeshRadio.swift` implements the
Rust `BleRadio` foreign trait with CoreBluetooth — **dual-role**: a `CBPeripheralManager` advertises
the nimmesh service + a write/notify characteristic (inbound bytes); a `CBCentralManager` scans,
connects, subscribes, and writes (outbound). Fire-and-forget `send` (outcome → `node.onSendResult`),
the node held **weakly** (ADR-0002 gotcha d), peers keyed by their CB UUID.

- `WebHostView.swift`: the bridge now constructs a real `MeshNode(senderId:, radio: BleMeshRadio)` (the
  radio comes up — advertise + scan); `meshStatus` + `reachability` read the **live node** (peer count
  + heard-gateway). Simulator: 0 peers (BLE unsupported, no crash); device: real peers.
- `node.rs`: `peer_count()` FFI (the live mesh-status reading the shim drives).
- `docs/DEVICE-TEST.md` (new): the **phone-test runbook** — free-tier Apple ID signing (no $99 needed),
  install on 2 phones, the offline-mesh testnet test, and the real-funds mainnet test.

**Phone-test scope (genuinely yours):** the byte-pipe + discovery compile clean and the app runs the
mesh stack without crashing in the sim, but **on-device BLE interop + tuning is the 2-phone test** —
MTU for the 256-B packet (the Rust G6 fragmenter splits larger), the iOS background overflow-UUID dead
spot, and collapsing the two directed links per pair. The core protocol (relay/dedup/TTL/store-and-
forward) is already proven headlessly; this wires it to a real radio.

213 tests green, `cargo clippy --all-features -D warnings` + `cargo fmt --check` + size-guard clean,
iOS `xcodebuild` BUILD SUCCEEDED, the app runs in the sim with the BLE node live (mesh bar:
`offline · 0 nearby · testnet · core 0.24.0 · head …`).

## [0.24.0] — 2026-06-27

### Added — C1c-2: the app sends on testnet (sign → broadcast), + ATS fix + banner one-line

The wallet can now originate a real transaction. The Send screen signs with the Keychain key and
broadcasts to live testnet; the home proves the chain connection.

- `apple/NimmeshApp/Sources/TestnetRpc.swift` (new): a `URLSession` JSON-RPC client for testnet —
  `getBlockNumber` (head, for `validityStartHeight`), `sendRawTransaction`, `getTransactionByHash`
  (inclusion), `getAccountByAddress` (balance), faucet tap. Mirrors the proven Rust `HttpGatewayRpc`
  envelope (unwrap `result.data`; errors as string or object). **All crypto stays in Rust** (AppSigner);
  this is network IO only. **Testnet-only.**
- `WebHostView.swift`: async bridge methods `headHeight` / `walletBalance` / `fundFromFaucet` /
  `sendTransaction` (= fetch head → sign with the Keychain key → broadcast; returns the tx hash).
- `webui/index.html`: the **Send screen** now has an amount field + a real **Send** button (sign +
  broadcast, with status) instead of the compose-only stub; the mesh bar shows the live testnet head.
- **`project.yml`: testnet ATS exception** — App Transport Security was silently blocking the RPC/faucet
  over HTTPS (no forward secrecy); without this the send couldn't work on device. Testnet domains only.
- **Backup banner — one line (Andjroo):** the escalated tiers wrapped the Backup pill to a second row.
  Forced `flex-wrap: nowrap` + flexible text + pill pushed right (real-wallet layout) and trimmed the
  copy ("Back up your wallet" / "Back up now or lose your funds").

**Verified in the running simulator:** the mesh bar shows the live head (`head 4500627`) — proving the
app reaches testnet RPC end to end (the same path the send uses) — and the Send UI + one-line banner
render correctly. The full funded send composes proven pieces (sign: the C1b CryptoKit↔dalek interop
test; broadcast: the same RPC the G8 live tool used for block 4428402).

`nq lint` 0 errors; iOS `xcodebuild` BUILD SUCCEEDED.

## [0.23.0] — 2026-06-27

### Changed — C1c-1: the app shows YOUR real wallet address (testnet)

The wallet is no longer a demo placeholder. On device, the webui pulls the device's real testnet
address (from the C1b Keychain key, over the read-only `walletAddress` bridge — seed never crosses) and
renders it everywhere it belongs: the **home header** address + identicon, the **Receive** sheet's
3×3 address grid + identicon, and the **request QR**. In a plain browser (no bridge) it degrades to the
demo address. Marked the wallet's own elements (`data-wallet` / `data-wallet-address`) so tx-row
counterparties are left untouched; `RECEIVE_ADDR` is now a shared global the address update replaces.

**Also fixed the false-alarm "sim launch" issue (#56, closed):** the app launches fine — the earlier
failure was a wrong bundle id in my launch command (`com.nimmesh.NimmeshApp` vs the real
`com.nimmesh.app`). Confirmed in the running simulator: the C1b wallet self-test prints
`signedOk=true` and a real address, and the home now renders it.

`nq lint` 0 errors; iOS `xcodebuild` BUILD SUCCEEDED; real address verified in the running simulator.
Next (C1c-2): wire the Send screen to sign + broadcast a live testnet transaction from the app.

## [0.22.0] — 2026-06-27

### Added — C1b: native Keychain wallet key + the CryptoKit ↔ dalek interop proof (testnet)

The app now holds a real key: an iOS **Keychain-backed Ed25519** key that implements the Rust
`EnclaveKey` foreign trait. The seed lives in the Keychain (`ThisDeviceOnly`); only the public key
(32 B) and a detached signature (64 B) ever cross FFI.

- `apple/NimmeshApp/Sources/Wallet.swift` (new): `KeychainEnclaveKey` (CryptoKit `Curve25519.Signing`
  + Keychain persistence) + `Wallet` (load/create on first launch, derive address, sign). iOS app
  builds + links it (`xcodebuild` BUILD SUCCEEDED).
- `lib.rs`: `verify_signed_tx_hex(raw_hex) -> bool` FFI (the relay's content-blind check, exposed so
  the app can self-verify a signed blob).
- `WebHostView.swift`: read-only `walletAddress` bridge method + a launch self-test.
- **Critical interop proof** (`signer.rs` test): a real **CryptoKit Ed25519 signature passes our
  `ed25519-dalek verify_strict`** — so a Keychain-signed tx clears the G12 spam filter on every relay.
  (Notable finding: CryptoKit *randomizes* its Ed25519 signatures, so they're **not** byte-identical to
  dalek's deterministic ones — but they verify, which is all that matters. Reference captured from
  CryptoKit on macOS; key derivation is deterministic and matches dalek byte-for-byte.)

**Known issue (does not block — affects on-device, which is Andjroo's gate):** the app currently fails to
*launch* in the headless simulator (FBS code 4, likely an xcframework embed/launch-config issue, not the
wallet code). Filed for the device session; the wallet code itself builds, links, and its crypto is
proven headlessly. Next (C1c): wire the Send/Receive screens to the real address + sign, and fix the
sim launch so a live testnet send can be demoed end to end.

212 tests green, `cargo clippy --all-features -D warnings` + `cargo fmt --check` + size-guard clean,
iOS `xcodebuild` BUILD SUCCEEDED.

## [0.21.0] — 2026-06-27

### Added — C1a: the real-signed money path, proven end to end (testnet)

**Operating-model change (Andjroo):** testnet is now full-speed + auto-merge — keygen/signing/testnet
broadcast included. The only gates are **mainnet** and **real devices**. `docs/LOOP.md` operating model
updated; `MAINNET-GATING.md` stands.

- `crates/nimmesh-core/src/send_e2e_tests.rs` (new): the headless proof that the C1 send path is real.
  A genuine **Ed25519-signed** transfer (`AppSigner` over an `EnclaveKey` — the seed never leaves it)
  floods `origin → verifying relay → gateway`: the relay accepts it *because* the signature is valid
  (G12 verify-before-relay), the gateway broadcasts the **exact signed 139-byte bytes**, and the origin
  settles. Plus the negative: a **tampered** signed tx (one flipped value nibble) is dropped by the
  verifying relay — no broadcast, never settles. Ties G3+G4+G6+G8+G12+G17 together with real crypto
  (the prior e2e suites used opaque stand-in bytes).

The signing FFI itself (`AppSigner`, `submit_signed_transfer`, `anchored_intent`, `payment_status`)
already existed from G3/G8/G9/G17; C1a proves they compose into a working, spam-filtered, settling send.
Next (C1b/c): the native Keychain `EnclaveKey` + the webui Send screen wired to sign for real.

211 tests green, `cargo clippy --all-features -D warnings` + `cargo fmt --check` + size-guard clean.

## [0.20.0] — 2026-06-27

### Changed — project renamed bitmesh → **nimmesh** (it's Nimiq, not Bitcoin)

"bit" wrongly implied Bitcoin (a leftover from the Bitchat port). Renamed the whole project to
**nimmesh** before any public site / TestFlight exists. Pure mechanical token-swap, no logic change:

- `bitmesh` → `nimmesh`, `Bitmesh` → `Nimmesh`, `BITMESH` → `NIMMESH` across all code, docs, configs.
- Crate `bitmesh-core` → `nimmesh-core` (dir + package + lib); xcframework `BitmeshCore` → `NimmeshCore`;
  iOS app `BitmeshApp` → `NimmeshApp` (dir + `NimmeshApp.swift`); bundle prefix `com.nimmesh`.
- JS bridge `window.bitmesh` / channel `"bitmesh"` → `window.nimmesh` / `"nimmesh"`; webui title + wordmark.
- Repo `nimiq.bitmesh` → `nimiq.nimmesh`; public site target `nimmesh.nimiq.tech`.
- **Unchanged:** generic mesh terms (`MeshNode`, `BleRadio`, `MeshGateway`), the `nimiq:` payment-URI
  scheme, and the upstream Bitchat (`permissionlesstech/bitchat`) port attribution.

209 tests green, `cargo clippy --all-features -D warnings` + `cargo fmt --check` + size-guard + `nq lint`
clean, iOS `xcodebuild` BUILD SUCCEEDED as NimmeshApp, webui wordmark renders "nimmesh".

## [0.19.0] — 2026-06-27

### Added — G13: mainnet-gating doc + headless mesh demo — non-money-path

- `docs/MAINNET-GATING.md` (new): the safety contract for staying on testnet. Documents the six
  in-code invariants that keep every build/test/loop on testnet (default network, testnet-guarded RPC,
  feature-gated broadcast, seed-never-crosses-FFI, unconfirmed-until-inclusion, money-path-never-auto-merge),
  exactly what a future mainnet switch would require (and that only Andjroo does it), the pre-mainnet
  checklist (most boxes still open), and the irreversible/gated action list.
- `crates/nimmesh-core/examples/mesh_demo.rs` (new): the whole pay loop —
  `submit → flood → relay → dedup → gateway → receipt → settled` — runs **headless, no network** against
  a mock gateway (`cargo run -p nimmesh-core --example mesh_demo`). The no-network companion to the G8
  `live_testnet_broadcast` tool (which proves the same path on the real testnet, block 4428402).

This is the autonomous-safe half of G13: the real-testnet broadcast demo is already the G8 live tool
(money-path, Andjroo-authorized); the mainnet switch itself stays Andjroo-only. No sign/keys/broadcast here.

209 tests green, `cargo clippy -D warnings` (incl. the example) + `cargo fmt --check` + size-guard clean,
the demo runs end-to-end (SETTLED, 1 submission, bytes match).

## [0.18.0] — 2026-06-27

### Added — G12 (part 2): per-peer rate limits + stop-after-ACK — completes G12 — non-money-path

The rest of RISKS.md #4's anti-DoS hardening, on top of part 1's verify-before-relay.

- `crates/nimmesh-core/src/ratelimit.rs` (new): `PeerRateLimiter`, a per-peer **token bucket**
  (256 burst / 64 frames-per-sec steady). `process_inbound` drops frames from a peer exceeding its
  budget before any decode/relay airtime is spent (`rate_limited` counter). Clock-free (worker
  monotonic clock → deterministic), tracked-peer map bounded against peer-id churn. 5 unit tests.
- `engine.rs`: **stop-after-ACK** — once a gateway receipt for a txId has been seen, the node stops
  re-carrying copies of that (already-landed) tx (`stop_after_ack` counter). `handle_tx` now decodes the
  envelope once and reuses it for verify + ACK-check + the gateway submit.
- **NACK** is the existing G8 reject path: a gateway floods a `Failed`/`Expired` `nimiqTxReceipt`, which
  settles the sender (and, via G17, the receiver) to `Failed` — no new code needed.
- 2 new e2e tests (a flooding peer is throttled; a node drops an already-ACKed tx), split into
  `hardening_e2e_tests.rs` to keep test files under the 800-line guard.

**Refactor:** extracted the G9 head-beacon engine glue (`emit_head_beacon` / `handle_head_beacon` /
`tx_valid_until`) from `engine.rs` into `beacon.rs` (where its codec lives), restoring engine headroom
(`engine.rs` 825 → 772). Mirrors the `balance.rs` / `settlement.rs` glue pattern.

**Completes #12.** Non-money-path throughout (no sign/keys/broadcast). 209 tests green, `cargo clippy
-D warnings` + `cargo fmt --check` + size-guard clean.

## [0.17.0] — 2026-06-27

### Added — G12 (part 1): verify-before-relay — the free spam filter — non-money-path

RISKS.md #4's anti-spam defence: a node refuses to store or relay a `nimiqTx` whose opaque blob is not
a **well-formed, correctly-signed** Nimiq transfer. Forging mesh spam now costs a real signature.

- `crates/nimmesh-core/src/nimiq/tx.rs`: `decode_basic_wire` (inverse of `serialize_basic`) +
  `verify_basic_wire(wire) -> bool` — derives the sender from the embedded pubkey, rebuilds
  `serializeContent`, and checks the Ed25519 signature with `verify_strict`. **Content-blind**: it
  proves the blob is genuinely signed, but never inspects *who* it pays or *whether it can* (core
  value #3 — the relay stays trustless; balance is the gateway's/chain's job). Pure, panic-free, no keys.
- `engine.rs`: `handle_tx` drops an unverified tx (no store, no relay) when verify-before-relay is on,
  counting `verify_dropped`. Threaded a `verify_before_relay` flag: **on in production**
  (`MeshNode::new`), off in the headless harness (which floods opaque stand-in bytes).
- 4 new tests incl. an e2e: a verifying relay **drops a junk tx** (gateway never sees it) but **carries
  a real signed transfer** to settlement.

**Non-money-path:** verification reads a public signature; it signs nothing, handles no keys, broadcasts
nothing. (Reclassified from the issue's original `money-path` grouping — this slice is pure defensive
hardening. The keygen/sign path C1 remains money-path + Andjroo-gated.) Part 2 (per-peer rate limits +
stop-after-ACK) follows.

202 tests green (cargo), `cargo clippy -D warnings` + `cargo fmt --check` + size-guard clean.

## [0.16.1] — 2026-06-27

### Fixed — G18 Receive sheet polish (web UI, compared against the real wallet)

Refined the "pay me X NIM" Receive screen after a device screenshot, diffing against the real wallet's
Receive NIM modal:

- **Removed the default button border** on "Create request link" + the QR toggle — they were rendering
  the UA `2px outset` border (the dark ring visible on device). Now clean borderless grey pills like the
  real wallet, with a tap press-state and a keyboard-only focus ring (`:focus-visible`).
- **Enlarged the identicon** 116 → 136 px to match the real modal's prominent identicon-in-hex.
- **Cleaner QR-toggle glyph** (three rounded finder squares + a module block) and the QR bumped to the
  component's default 240 px; fill already matches the Nimiq `#265DD7 → #0582CA` radial exactly.
- **Polished the amount input** — muted placeholder + a Nimiq-blue caret.

`nq lint` 0 errors; base + request states screenshot-verified at 390px against
`references/screenshots/wallet-app/logged-in/receive-nim-address-mobile.png`.

## [0.16.0] — 2026-06-27

### Added — G18: contacts + "pay me X NIM" request links — non-money-path

The fat-finger fix for getting paid: the payee shows a request QR that carries the recipient address
**and** the exact amount, so the payer's Send screen is pre-filled. Pure local + QR — no mesh packet,
no keys.

- `crates/nimmesh-core/src/request_uri.rs` (new): the `nimiq:<address>?amount=<NIM>&message=<text>`
  URI codec — `build_request_uri(address, amount_luna, message?) -> String?` + `parse_request_uri(uri)
  -> PaymentRequest?`. Pure, key-free, **symmetric** (`parse(build(x)) == x`, property-tested): validates
  the address, formats/parses NIM⇄luna exactly (≤5 dp, integer math, no floats), percent-encodes the
  message. `PaymentRequest` is a new FFI record. 8 unit tests.
- `webui/index.html`: the Receive sheet gains a **"Request an amount (optional)"** field (borderless,
  like the real wallet) + "Create request link" → a Nimiq-blue **QR** encoding `nimiq:<addr>?amount=<NIM>`
  that live-updates with the amount, plus a "Requesting X NIM" caption.

**Honest scope:** recent recipients + named contacts are local UI state populated by real send history
(C1, money-path) — the Send sheet already carries the Contacts + recent affordance. G18 ships the one
cross-platform-exact piece (the request-link codec) + the request-QR UI, which work fully today.

198 tests green (cargo, 8 new), `cargo clippy -D warnings` + `cargo fmt --check` + size-guard clean,
`nq lint` 0 errors, iOS `xcodebuild` BUILD SUCCEEDED, Receive-sheet request QR screenshot-verified at
390px (`nimiq:…?amount=12.5`).

## [0.15.0] — 2026-06-27

### Added — G17: settlement closure for both parties — non-money-path

"Did it land?" is the offline-pay question for **both** sides. G8 already floods a `nimiqTxReceipt`
when a gateway accepts/rejects a tx (and G7 store-and-forward catches a rejoining node up on it);
G17 closes that loop for the **receiver** too, not just the sender.

- `crates/nimmesh-core/src/settlement.rs` (new): the per-node payment ledger, lifted out of
  `engine.rs` (size-guard) and generalised to two directions — `Outgoing` (recorded on submit) and
  `Incoming` (recorded by a payee via `watch_incoming`). The **same** flooded receipt settles whichever
  side is watching that txId: `Pending → ✓ Settled` / `✗ Failed`. `PaymentStatus` now lives here;
  `SettlementDirection` + `Settlement` are new FFI types. 5 unit tests.
- `engine.rs`: now delegates to the ledger (`record_pending`/`record_incoming`/`settle`/`status`/
  `settlement`); `handle_receipt` already settles any tracked txId, so the receiver path needed no new
  wire handling. The extraction shrank `engine.rs` **796 → 752 lines** (headroom restored).
- `node.rs` (FFI): `watch_incoming(tx_id)` (the payee registers an expected payment, learned via the
  request/confirmation flow) + `settlement(tx_id) -> Settlement?` (status + Outgoing/Incoming).
- `webui/index.html`: the tx-list now has a **pending** treatment — a static amber "🕐 Pending · via
  mesh" row that resolves to a settled row, the visible "did it land?" → ✓ closure.

**Trustless-relay note:** a node only matches receipts to txIds it itself registered — it never parses
a tx to guess who it's for, so the blind relay (core value #3) is preserved. The txId hand-off between
payer and payee rides the request/confirmation flow (G18); live in-app settlement needs a running node
(Phase D). The Rust ledger is the audited deliverable.

190 tests green (cargo, 7 new incl. 2 e2e: one receipt settles both sender + receiver; a NACK fails the
receiver too), clippy/fmt/size-guard clean, `nq lint` 0 errors, iOS `xcodebuild` BUILD SUCCEEDED,
tx-list pending row screenshot-verified at 390px.

## [0.14.0] — 2026-06-27

### Added — G16: "will it send?" reachability + validity-window countdown — non-money-path

The biggest anxiety of paying offline is not knowing whether the payment can get anywhere. G16
turns the node's existing mesh state into an honest answer, and tells the user how long a signed tx
stays spendable.

- `crates/nimmesh-core/src/reachability.rs` (new): pure, clock-free, key-free signals over public
  state. `assess_reachability(self_is_gateway, peer_count, heard_gateway) -> Reachability`
  (`Online` = a gateway is reachable; `Meshed` = peers but no gateway yet, relays + waits via G7
  store-and-forward; `Offline` = no peers). `blocks_until_expiry` / `secs_until_expiry` (~1 block/s)
  render the G9 validity-window countdown; `needs_resign` fires a re-sign nudge in the last ~600
  blocks. 7 unit tests.
- `node.rs` (FFI): `reachability()` (from the live peer count + a heard gateway beacon + whether this
  node is itself a gateway), `blocks_until_expiry(valid_until)`, `secs_until_expiry(valid_until)`.
- `apple/.../WebHostView.swift`: read-only bridge gains `reachability` (honest `offline` until the
  native radio runs — there is no mesh to reach yet).
- `webui/index.html`: the Send sheet now leads with the honest "will it send?" line — a static status
  dot (no pulse) + copy, driven live by the bridge (`Online` / `Meshed` / `Offline`) when a node runs.

**Honest scope:** the live reach + the validity countdown + the actual queue-and-auto-send of a
*signed* tx need a running in-app `MeshNode` (Phase D) and the signing seam (C1, money-path). G16 ships
the honest signal layer + FFI both of those read; the Rust assessment is the audited deliverable.

183 tests green (cargo), `cargo clippy -D warnings` + `cargo fmt --check` + size-guard clean, `nq lint`
0 errors, iOS `xcodebuild` BUILD SUCCEEDED, Send-sheet reachability line screenshot-verified at 390px.

## [0.13.0] — 2026-06-27

### Added — G20: good-citizen + battery-aware relay — non-money-path

The mesh *is* its users: a payment reaches the internet only because someone nearby relayed it.
G20 makes that participation visible and respectful of the user's battery.

- `crates/nimmesh-core/src/citizen.rs` (new): the battery-derived `RelayPosture`
  (`Full → Reduced → Frugal → Off`) and the good-citizen counter. `relay_posture(level, charging)`:
  **charging always means `Full`** (be the best citizen while plugged in); on battery it steps down —
  `Full` ≥ 50 %, `Reduced` ≥ 20 % (half fanout), `Frugal` ≥ 10 % (payment-critical traffic only),
  else `Off` (relay nothing for others — the user's own send/receive still work). `CitizenState` is
  lock-free atomics that **default to full participation** (100 %, charging) so a node that never
  reports a battery behaves exactly as pre-G20. `BatteryState` + `RelayStats` are FFI records.
- `relay.rs`: `should_relay_throttled(degree, factor)` scales the degree-adaptive probability by a
  battery factor; `should_relay` delegates with `factor = 1.0` (a sparse-mesh `bernoulli(1.0)`
  short-circuits without an RNG draw → existing deterministic behaviour byte-identical).
- `engine.rs`: `relay_onward` now passes through `citizen::relay_allowed` (posture carries this packet
  type **and** wins the battery-damped roll) and bumps the "payments helped" counter on a relayed `nimiqTx`.
- `node.rs` (FFI): `set_battery(level_pct, charging)` (the native shim reports `UIDevice`/`BatteryManager`)
  + `relay_stats() -> RelayStats` ("you helped N payments reach the network" + total + current posture).

**Honest scope:** the live "you helped N" UI line + the battery wiring need a running in-app `MeshNode`
+ the device battery API = the **native shim (Phase D)**; the FFI is ready. The Rust core is the audited
deliverable. Tests cover posture thresholds, type filtering, factor scaling, the helped-payment counter,
and two e2e mesh round-trips (a critical-battery node relays nothing; a charging node relays + counts it).

176 tests green (cargo), `cargo clippy -D warnings` + `cargo fmt --check` + size-guard clean, iOS
`xcodebuild` BUILD SUCCEEDED against the regenerated xcframework (new UniFFI records/enum compile).

## [0.12.0] — 2026-06-27

### Added — G19: backup nudge (self-custody protection) — non-money-path

A self-custody offline wallet has no "forgot password": lose the device without a backup and the
funds are gone forever. G19 makes the backup prompt **persistent and proportionate** — driven by a
pure, tested Rust policy, surfaced through the vendored `backup-banner` component's escalating
treatment.

- `crates/nimmesh-core/src/backup.rs` (new): `backup_urgency(BackupState) -> BackupUrgency`, a pure,
  **clock-free, key-free** policy. Inputs are public account facts only — `backed_up`, `balance_luna`,
  `days_since_first_funds`. Output escalates `None → Gentle → Important → Critical`, taking the higher
  of a balance-driven and an age-driven sub-score; returns `None` when backed up or there are no funds
  at stake. 8 unit tests (incl. monotonicity in both balance and age). FFI-exported (`#[uniffi::export]`).
- `apple/.../WebHostView.swift`: the read-only JS bridge gains `backupUrgency(state)` — the Rust policy
  decides; no key/seed crosses, only the public balance + backed-up flag.
- `webui/index.html`: the backup banner now escalates from the policy — hidden (`none`) → orange
  words-on-white card (`gentle`/`important`, escalating copy) → the component's solid-orange "file"
  treatment (`critical`, real `--nimiq-orange-bg` gradient + white inverse "Backup" pill). On device the
  bridge drives it; in a plain browser it degrades to the canonical "There is no 'forgot password'"
  words banner. No invented colors — both orange tokens come from `nimiq-style.min.css`.

**Honest scope:** the wallet has no keys/balance in-app yet (C1 keys + Phase D node), so the in-app
nudge reads the *displayed* balance for now; once real keys + a real balance land, the same call drives
it for real. The Rust policy is the audited deliverable; the UI is wired and verified at all four tiers.

165 tests green (cargo), `cargo clippy -D warnings` + `cargo fmt --check` + size-guard clean, `nq lint`
0 errors, iOS `xcodebuild` BUILD SUCCEEDED against the regenerated xcframework, banner tiers
screenshot-verified at 390px.

## [0.11.0] — 2026-06-27

### Added — G15 (Rust core): account balance over the mesh — non-money-path

Andjroo's feature: get an account's balance with no internet, by asking the mesh. A node floods a
**balance query**; any internet-bearing **gateway** answers with the balance it read at a head
height; every node caches it (unverified / last-known) and relays it onward. Read-only public
state — no keys, no signing. Mirrors the G9 head-beacon pattern end to end.

- `crates/nimmesh-core/src/balance.rs` (new): `nimiqBalanceQuery` (`0x33`, 20-byte address) +
  `nimiqBalanceResponse` (`0x34`, addr+balance+headHeight+networkId) payload codecs (length-exact,
  panic-free) + `BalanceCache` — clock-free, per-address, **monotonic by head height** (no stale
  rollback), `networkId`-guarded. `CachedBalance` is FFI-visible (`uniffi::Record`).
- `gateway.rs`: `MeshGateway::balance_of()` — `RpcGateway` reads it via the existing read-only
  `get_account` + `block_number` (no new capability, testnet-guarded); `MockGateway::set_balance`
  for tests. `BalanceAnswer { balance, head_height, network_id }`.
- `engine.rs`: `flood_local_balance_query` + `handle_balance_query` (a gateway answers + floods a
  response, dedups, relays) + `handle_balance_response` (cache + remember + relay) wired into the
  dispatch; a per-node `BalanceCache` + `cache_balance`/`cached_balance`.
- `node.rs` (FFI): `query_balance(address)` (floods a query; unparseable address = no-op) +
  `cached_balance(address) -> CachedBalance?` (last-known; `head_height` drives a future
  "synced X ago" stamp). New `Job::BalanceQuery`.
- Tests: 6 `balance` unit tests + 2 end-to-end mesh tests (query→gateway-answer→cache across
  origin↔relay↔gateway; gateway-with-no-balance answers nothing). **146 tests green**, clippy + fmt clean.
- **Honest scope:** the balance is **unverified / last-known** (a relay is untrusted) until a
  trustless **accounts-proof** binds it to the head-beacon hash (follow-up). The UI **"+ fiat +
  'synced X ago'"** binding lands when a `MeshNode` runs in the app (the native shim, Phase D) —
  the FFI (`query_balance`/`cached_balance`/`cached_head_height`) is ready for it.

## [Unreleased]

### Fixed — backup banner used a hand-written markup instead of the real component (web UI)

Andjroo caught it: the home's backup banner had the wrong color, container, and icon (navy text +
a hexagon icon + a gold "Back up" pill) because it was hand-written rather than the vendored
component. Replaced with the `backup-banner` component's **`words nq-orange`** variant verbatim —
amber warning-triangle + amber text + inset-bordered card + orange "Backup →" pill — now matching
the real wallet. Reinforces the rule: copy the real component, never hand-guess. `nq lint` 0 errors.

**Follow-up (Andjroo):** the grey container around the banner was rendering too faint/tight vs the
reference. Made the `.words` card explicit — white fill, a clearly visible light-grey border
(`rgba(31,35,72,.13)`), 12px radius, and 14px text so the "Backup →" pill stays on one row at
390px — now matching the reference container with breathing room.

## [0.10.0] — 2026-06-27

### Added — A1 (iOS app): WKWebView host + read-only JS↔Rust bridge — non-money-path

The pivot to the real wallet UI, made real on device. The iOS app stops hand-building UI in
SwiftUI (the rejected home is deleted) and instead **hosts the real `nimiq-ui` web layer
(`webui/`) in a `WKWebView`**, bridged to the Rust core. This is also the foundation merge of
the iOS shell + web UI onto `main`.

- `apple/NimmeshApp/Sources/WebHostView.swift` (new) — a `UIViewRepresentable` `WKWebView` that
  loads `webui/index.html` from the bundle (folder reference, so relative hrefs resolve) and a
  `Bridge` (`WKScriptMessageHandler`) exposing a promise-based `window.nimmesh` RPC. **Read-only**
  methods only — `version` (`coreVersion()`), `network` (`defaultNetwork()` + wireId + loopSafe),
  `meshStatus` (honest `offline` / `0 peers` until the native radio lands in Phase D). It signs
  nothing, broadcasts nothing, and never touches key/seed material → firmly non-money-path. The
  signing path (Send → enclave → queue) is deferred to the gated money slice (C1).
- `apple/NimmeshApp/Sources/NimmeshApp.swift` — mounts `WebHostView` (was `HomeView`).
- `apple/NimmeshApp/Sources/HomeView.swift`, `Theme.swift` (deleted) — the hand-built SwiftUI
  home Andjroo rejected; superseded by the web UI.
- `apple/project.yml` — `webui/` added as a folder-reference bundle resource.
- `webui/index.html` — safe-area insets for full-bleed hosting; a mesh status bar fed by the
  bridge (hidden in a plain browser, so screenshot verification is unaffected); the mobile
  account-header truncation fix.
- Gate: `cargo swift package` (xcframework) + `xcodegen` + `xcodebuild build`
  (`CODE_SIGNING_ALLOWED=NO`) — the app compiles against the freshly generated core and loads the
  web UI. Rust core unchanged (149 tests still green).

### Changed — A2 (web UI): mobile account-header polish — non-money-path (same 0.10.0 app milestone)

The account-header is the wallet's **1440px desktop** component (48px side padding, a 90px
identicon, 24px type); crammed into 390px it truncated the account name to "M.". A2 makes the
mobile home faithful without hand-inventing a layout:

- `webui/index.html` — re-scale the component's **own** layout vars for mobile (`--padding` 12px,
  48px identicon, `--h1-size`/`--body-size` 22/14px), fade the chunked address with the
  component's mask idiom (no hard ellipsis; `title` added for a11y/copy), and **stack the actions
  row** (full-width search + 50/50 Send/Receive) so every control stays legible and tappable.
- Verified on the iOS 26.5 simulator at device width: full "Mesh Wallet" label, legible
  balance/fiat, a faithful Nimiq mobile home. `nq lint` 0 errors. Core untouched (no version bump).
- Honest scope: real balance/address/tx data binds when there's a wallet (C1 keys) + mesh
  (Phase D); the displayed values are still demo until then.

### Added — A3 (web UI): Send + Receive screens — non-money-path (same 0.10.0 app milestone)

Built against authentic live testnet-wallet captures (a reusable Playwright capture pipeline +
logged-in references now live in the nimiq-branding-cli skill). Verifying our home against the
real wallet showed the account-header is faithful; the one divergence — Send/Receive placement —
is fixed here.

- `webui/index.html` — Send/Receive **moved to a bottom action bar** (Receive | Send | scan),
  matching the real mobile wallet, with nimmesh's **mesh status line directly above it** (the one
  honest divergence from the wallet). The header actions row is now just the full-width search.
- **Receive NIM** bottom sheet — identicon + the 3×3 Fira-Mono chunked address (`address-display`
  component) + "Create request link" + a real Nimiq-blue **QR** rendered on demand (`qr-creator`).
- **Send Transaction** bottom sheet — Contacts + recent-identicon row + "ENTER ADDRESS" 3×3
  auto-advancing input grid + "Create a Cashlink". **Compose-only**: the sign + queue action is a
  STUB with an honest note ("Signed offline, then relayed over the mesh. Signing arrives next.") —
  the real signing/broadcast is the gated money-path (C1), behind Andjroo. No keys here.
- Navy-overlay bottom sheets (rule 11), Nimiq easing, gold/blue per palette. Verified at 390px
  (playwright) + on the iOS 26.5 simulator; `nq lint` 0 errors. Core untouched (no version bump).

### Added — A4 (web UI): fleet chrome (language + connect-wallet) + mesh identity — non-money-path

Completes Phase A ("the app you can hold").

- `webui/index.html` — top-right **language pill** (`mountLanguagePill`) and **connect-wallet pill**
  (`mountWalletPill` over `createWallet` → Nimiq Hub delegate) from the shared **nimiq-app-shell**,
  loaded via a graceful dynamic `import()` from jsDelivr (the fleet-standard pattern). An offline
  import failure just hides the pills; the core mesh UX (home / send-compose / receive) keeps
  working. The connect-wallet pill is **selection only** (choose the delegate account) — no key/seed
  ever crosses to us; the real signing via that delegate is the gated money-path (C1). Non-money-path.
- **Mesh identity:** the mesh status line is now **always visible** with a mesh-nodes glyph —
  "Bluetooth mesh · offline-ready" by default, enriched by the bridge to "mesh <state> · N nearby ·
  <net> · core X" on device — the unmistakable "this is the offline mesh, not the regular network" cue.
- Verified the CDN import works in the real **WKWebView** (device screenshot: both pills loaded +
  the live mesh line). `nq lint` 0 errors, no horizontal overflow. Core untouched.

## [0.9.0] — 2026-06-26

### Added — G9 (Rust core): head beacon (`0x32`) + validity-window guard / packet GC — non-money-path

The relay budget made real (PROTOCOL.md "Validity window — the relay budget", RISKS.md #1 —
"the single constraint that most shapes the design"). An Albatross tx is valid only for
`[validityStartHeight, validityStartHeight + 7200)` (≈ 2 h); that window is the **entire mesh
relay budget**. G9 lets a deep-offline signer anchor to a fresh head, and GCs dead txs so the
mesh never carries them. New logic lives in dedicated modules to keep every file < 800 lines.
Still opaque-bytes only — reads a public head height, no signing/broadcast/keys (`// G3:` /
`// G8:` anchors preserved).

- `crates/nimmesh-core/src/beacon.rs` — the **clock-free building blocks** (new module):
  - **`HeadBeacon`** `{height u32 | blockHash 32 | networkId 1}` + `encode_beacon` /
    `decode_beacon` (panic-free, exact-length) — the `nimiqHeadBeacon` (`0x32`) payload.
  - **`HeadCache`** — every node caches the **latest** head it has heard (monotonic — an
    older/equal beacon is ignored; a `networkId` mismatch is rejected). The G3 signer anchors
    `validityStartHeight` to it.
  - **`BeaconScheduler`** — a rate-limiter (one emit per `BEACON_TICK_MS`), caller-driven like
    the G7 `SyncScheduler`.
  - **`is_expired(head, validUntil)`** — the guard: a tx is dead only when **both** a head and
    a window are known and `head >= validUntil` (no head ⇒ never GC blindly).
- `crates/nimmesh-core/src/gateway.rs` — `MeshGateway::head_beacon()` (new default seam,
  `None` for the chain-less `MockGateway`); `RpcGateway` sources the height from its existing
  read-only RPC `block_number` (**no new networking**) on its testnet `networkId`.
- `crates/nimmesh-core/src/engine.rs` — wires it into the worker: a **gateway** floods a
  beacon via `emit_head_beacon` (rate-limited, gateway-only); every node caches an inbound
  `0x32` (`handle_head_beacon`) then floods it onward; the **validity-window guard** drops an
  expired `nimiqTx` in `handle_tx` (neither relayed nor stored — packet GC); `flood_local_tx`
  stamps `validUntil = cachedHead + VALIDITY_WINDOW` when a head is known. `now`/`head` are
  injectable (worker clock + cached beacon) so tests are deterministic.
- `crates/nimmesh-core/src/node.rs` — FFI: **`poll_beacon()`** (gateway emits if the tick is
  due; non-blocking, ADR-0002), **`cached_head_height() -> Option<u32>`**, and
  **`anchored_intent(recipient, value)`** which builds a `TransferIntent` anchored to the
  freshest cached head — returning `None` (refusing to pre-date) when no beacon has been heard.
- Tests: `beacon.rs` unit tests (codec round-trip, monotonic cache, network-mismatch reject,
  `is_expired`, scheduler) + `beacon_e2e_tests.rs` end-to-end — (a) a gateway emits a beacon, a
  node caches it, and a signed intent's `validityStartHeight` equals the cached head; (b) a node
  GCs / refuses to relay a tx past its window (and still relays a live one); (c) monotonic
  beacon (older ignored); plus deep-offline refuses-to-anchor. All offline/deterministic; the
  shared wire-frame builders + spy radio moved to `test_support.rs` (keeps each test file
  < 800 lines).

## [0.8.0] — 2026-06-26

### Added — G8 (Rust core): gateway broadcast (TESTNET) — MONEY-PATH, Andjroo-authorized

The one online hop: a gateway node takes a signed-tx blob off the mesh and puts it on the
**Nimiq Albatross TESTNET** via plain JSON-RPC (`sendRawTransaction`), then floods a
`nimiqTxReceipt (0x31)` back. **TESTNET-only (`networkId = 5`)** — every constructor is
testnet-guarded and there is no path to a mainnet RPC. `cargo test` stays fully offline.

- `crates/nimmesh-core/src/rpc.rs` — the blocking Albatross **JSON-RPC client seam**
  (`GatewayRpc`: `block_number`, `get_account`, `send_raw_transaction`, `get_transaction`),
  modelled on the fleet's `sendhome`/`nimiq.sale` clients (JSON-RPC 2.0 over HTTP POST,
  `{ result: { data } }` unwrap, transient-vs-terminal error split — **never** the
  `@nimiq/core` consensus client). Ships `MockRpc` (deterministic, always compiled, keeps
  the test suite offline) + `HttpGatewayRpc` (real `ureq` blocking client) gated behind the
  new **`gateway-rpc`** cargo feature so the pure-protocol core stays dependency-light +
  WASM-friendly. `guard_testnet` refuses any non-testnet network or known mainnet host
  (`rpc.nimiqwatch.com`).
- `crates/nimmesh-core/src/gateway.rs` — the real **`RpcGateway`** (a `MeshGateway` over any
  `GatewayRpc`) + the `SubmitContext` the engine hands it. On a `nimiqTx` it **guards
  `networkId`** (testnet 5), **checks the validity window** against the live head (drops +
  `Expired` receipt if `head >= validUntil`, never broadcasting), then
  **`sendRawTransaction(rawHex)`** (terminal rejection → `Failed`; accept → `Accepted`); a
  transient RPC error yields no receipt so another gateway / a retry can still carry the tx.
  `MeshGateway::submit_validated` is a new default-method seam (`MockGateway` unchanged).
- `crates/nimmesh-core/src/engine.rs` — the gateway role now calls `submit_validated` with
  the decoded envelope (`networkId` + `validUntil` + `txId`); receipt keyed by the origin's
  `txId`, relay-anyway preserved, `on_packet_received` stays non-blocking (worker thread).
- `crates/nimmesh-core/examples/live_testnet_broadcast.rs` — the **live broadcast tool**
  (built with `--features gateway-rpc`): generates/loads a testnet keypair via the core,
  prints the `NQ…` address, optional faucet tap (`faucet.pos.nimiq-testnet.com/tapit`),
  fetches the head, **signs with the core's G3 signer**, broadcasts, and polls inclusion —
  printing the tx hash + block + explorer URL. Injectable RPC URL/seed; testnet-guarded.
- Tests: `RpcGateway` broadcast/expired-drop/wrong-network/terminal/transient against
  `MockRpc`, the `guard_testnet` refusals, and two end-to-end mesh tests (full pay loop
  settles through the real `RpcGateway`; an expired tx is never broadcast) — all offline.

## [0.7.0] — 2026-06-26

### Added — G3 (Rust core): offline Nimiq signing (TESTNET) — MONEY-PATH, Andjroo-gated

The money-critical layer: turn a payment *intent* into a self-contained, self-authenticating
**signed Albatross transaction blob** (GOAL.md north star), proven **byte-for-byte equal to
`@nimiq/core`** (v2.7.0). Pure offline crypto — **no network, no RPC, no broadcast** (that is
G8); the **seed never crosses the FFI boundary**. Testnet-only (`networkId = 5`) by default.

- `crates/nimmesh-core/src/nimiq/` — new module tree (each file < 800 lines):
  - **`tx.rs`** — byte-exact `serializeContent` (the **67-byte** signing payload), the full
    **139-byte** `Basic`-format wire blob (`format || proof_type || pubkey || recipient ||
    value || fee || vsh || network || signature`), the **Blake2b-256** tx hash, and the
    standalone **98-byte** single-sig `SignatureProof` (`type || pubkey || merkle_len ||
    sig`). Fee is always 0; data empty.
  - **`address.rs`** — the 20-byte address + its user-friendly **`NQ`-IBAN** codec (Nimiq
    base32 alphabet + mod-97 check digits), parse (checksum-verified) ⇄ format round-trip,
    and `Address::from_public_key` = `Blake2b-256(pubkey)[..20]`.
  - **`hex.rs`** — pure hex + big-endian byte helpers (ported from the fleet's `sendhome`).
  - **`signer.rs`** — the pluggable **`KeyOrigin`** seam with **both** origins Andjroo asked
    for: **`AppSigner`** (offline-first; signs locally via the **`EnclaveKey`** `with_foreign`
    trait so the Secure Enclave / Android Keystore holds the seed — only a public key + a
    64-byte signature ever cross FFI) and **`DelegatedSigner`** (a `with_foreign` seam the
    native layer implements by calling **Nimiq Pay / Hub**, returning a pre-signed blob).
    Both emit a `SignedTransfer { raw_hex, tx_hash, validity_start_height, valid_until_height }`
    (`valid_until = vsh + VALIDITY_WINDOW(7200)`, RISKS.md #1). `// NATIVE:` notes mark the
    on-device sign-but-DON'T-broadcast paths verified later.
- `crates/nimmesh-core/src/node.rs` — **`MeshNode::submit_signed_transfer(SignedTransfer)`**
  decodes `raw_hex` and floods it through the existing mesh path as **opaque bytes**
  (replacing the G3 opaque stub); `submit_local_tx(Vec<u8>)` stays for raw-bytes callers.
- `crates/nimmesh-core/src/lib.rs` — `pub mod nimiq`; the `G3:` anchor marked DONE.
- **Byte-exactness proof** — `scripts/fixtures/gen-fixtures.mjs` generates reference
  `{rawHex, txHash, serializeContent, proof, signature, addresses}` from `@nimiq/core` for
  4 known `(privKey, recipient, value, vsh, testnet)` inputs; committed at
  `crates/nimmesh-core/tests/fixtures/g3_signing_fixtures.json`. `tests/g3_signing_fixtures.rs`
  reproduces each from the same inputs and asserts equality **byte-for-byte** (the acceptance
  bar; a subtly-wrong serializer fails here). Confirms `ed25519-dalek` == `@nimiq/core`
  (deterministic RFC-8032).
- Deps: `ed25519-dalek`, `blake2` (runtime); `serde` + `serde_json` (dev, fixture load).
  Base32 IBAN codec + hex are implemented in-crate (no extra dep).

**Money-path safety:** seed never crosses FFI (only pubkey + signature do); testnet default;
**no broadcast / no RPC / no networking** in this PR. **DO NOT MERGE without Andjroo's review.**

## [0.6.0] — 2026-06-26

### Added — G11 (Rust core): optional encrypted memo / 1:1 chat (Noise XX) — PROTOCOL.md "Encryption"

A **pure transport-privacy** layer so an *optional* memo can ride a `nimiqTx` encrypted and
two peers can exchange an *optional* 1:1 encrypted chat. **No money-path**: no wallet seed,
no Nimiq tx signing, no broadcast; `txWire` stays opaque and the `G3:` / `G8:` anchors are
untouched. The Noise static key is a **transport** key, never a wallet key.

- `crates/nimmesh-core/src/noise.rs` — the Noise layer (new module, < 800 lines):
  - **`Noise_XX_25519_ChaChaPoly_SHA256`** mutual-auth, identity-hiding handshake via the
    `snow` crate (`Handshake` initiator/responder + the three XX messages; `handshake_xx`
    drives both ends). Identities are exchanged only *after* the ephemeral DH (identity
    hiding); each side captures the peer's static fingerprint at completion.
  - **Transport identity** — `StaticIdentity`, a long-term **Curve25519 static keypair**
    generated for this app's mesh session (`x25519-dalek`), **explicitly separate** from any
    Nimiq wallet seed (which does not exist here; G3/G10 own that, money-path). The
    **fingerprint** = **SHA-256** of the static public key (`fingerprint_of`), stable + 32 B,
    for out-of-band verification. `Debug` never prints the secret.
  - **Two ChaChaPoly cipher states** after the handshake (`Session`, `snow` stateless
    transport — one key per direction) with a **1024-message sliding-window replay guard**
    (`ReplayWindow`, RFC-6479 style, 1-based counters): a sealed blob is
    `nonce(8, BE) || ciphertext||tag`; `decrypt` **authenticates first, then** runs the
    replay filter so a forged/replayed nonce can't poison the window.
  - Memo + chat helpers: `seal_memo`/`open_memo` (the `encMemo` blob) and
    `seal_payload`/`open_payload` (inner `NoisePayloadType`: `chat = 0x01`,
    `nimiqTx = 0x04`, `nimiqTxReceipt = 0x05` — inner bytes stay **opaque**).
- `crates/nimmesh-core/src/packet.rs` + `codec.rs` — new **`noiseEncrypted = 0x11`**
  `MessageType` for optional 1:1 encrypted chat; round-trips through the wire codec.
- `crates/nimmesh-core/src/envelope.rs` — the `encMemo` TLV (`0x05`) carries a Noise-sealed
  blob (the field already existed; now wired end-to-end with a round-trip test).
- `crates/nimmesh-core/src/engine.rs` — a `noiseEncrypted` packet is relayed **opaque**
  (blind dedup + store-and-forward + adaptive TTL relay); only its two endpoints decrypt it.
  `on_packet_received` stays **non-blocking** (enqueue-only, ADR-0002).
- Tests (in `noise.rs`): XX handshake completes between two parties (+ mutual fingerprint
  match); memo + chat + targeted-nimiqTx encrypt→decrypt round-trips (memo through a real
  envelope); the replay window **rejects** a replayed ciphertext, accepts out-of-order,
  rejects too-old counters; a **mismatched static key fails to decrypt** (AEAD); tampered
  ciphertext fails; fingerprint is **stable + 32 bytes**; deterministic via fixed transport
  secrets (handshake ephemerals use `snow`'s builder — behavior asserted, not exact bytes).

## [0.5.0] — 2026-06-26

### Added — G7 (Rust core): store-and-forward via GCS gossip-sync (PROTOCOL.md "Store-and-forward = GCS gossip-sync")

The key offline-origination piece: a node that was **out of range** when packets flooded
the mesh catches up on what it missed when it rejoins — within Nimiq's ~2 h validity
window. New logic lives in dedicated modules to keep every file under the 800-line ceiling.

- `crates/nimmesh-core/src/gcs.rs` — a **Golomb-Coded-Set membership filter**:
  - Hashes each member id uniformly into `[0, N·M)` (`M = 100`), sorts, delta-encodes, and
    **Golomb-Rice codes** the deltas (parameter `P = 6`, the near-optimal Rice modulus for
    the gap distribution). **No false negatives**; false-positive rate **1/M = 0.01**.
  - `~8.6` bits/id → a `360`-id filter is a deterministic `≤ ~388 B`, under the **400 B**
    budget (`GCS_MAX_BYTES` / `GCS_MAX_ITEMS`). Panic-free `from_bytes`/`contains` over
    hostile peer bytes (a corrupt filter degrades to "absent", never a crash).
  - Tests: membership (no false negatives across 1–500 ids), an **empirical fp-rate check
    near 0.01** (100k disjoint queries), wire round-trip, byte-budget, hostile-input.
- `crates/nimmesh-core/src/store_forward.rs` — the **recent-packet cache + sync cadence**:
  - `RecentCache` — a bounded, **clock-free** store (≤ **1000** entries, **900 s** (15 min)
    retention, **15 s** active window; oldest evicted) keyed by a header-derived
    `packet_id` (type + sender + timestamp; TTL/payload excluded so a relayed copy keeps a
    stable id). Like the G6 reassembler, the caller passes a monotonic `now_ms`, so tests
    are deterministic. `build_filter` / `missing` drive the sync exchange.
  - `SyncScheduler` — a clock-free rate-limiter firing at most once per **30 s** tick.
- `crates/nimmesh-core/src/engine.rs` — wires it into the worker:
  - every accepted packet (origin/relay/gateway/fragment) is **remembered** in the cache;
  - **`requestSync` (`type 0x21`, ttl 0, local-only)** — `emit_request_sync` floods this
    node's GCS "have" filter to direct peers (never relayed); a peer that receives one
    unicasts back each cached packet **not** in the filter, flagged **`isRSR` (`0x10`)**;
  - an inbound `isRSR` reply is delivered **locally only** (TTL zeroed → never re-flooded)
    through the normal handlers, so it settles / submits / caches like any other packet;
  - `maintenance_tick` issues a `requestSync` only when the 30 s tick is due.
- `crates/nimmesh-core/src/node.rs` — FFI: **`request_sync()`** (force a catch-up on
  rejoin) and **`poll_sync()`** (the periodic maintenance poll). Both **non-blocking**
  (enqueue only, ADR-0002 gotcha a); the worker does the GCS work off the callback thread.
- Tests (`e2e_tests.rs`): a **simulated rejoin** — A floods 12 packets while B is
  partitioned/offline; B rejoins, `request_sync`s with its stale (empty) filter, and
  receives exactly the missed packets via `isRSR`, ending with the **full set and no
  duplicates**; a **gap-only** sync (B already holds some); and **tick rate-limiting**.

`txWire` stays **opaque** end to end — no signing, no broadcast, no key material (`// G3:`
/ `// G8:` anchors preserved; money-path Andjroo-gated).

## [0.4.0] — 2026-06-26

### Added — G6 (Rust core): relay-engine refinements (PROTOCOL.md "TTL / hop cap & relay")

Builds the PROTOCOL.md relay sophistication on top of the G5 basic relay (which already
did blind LRU dedup → TTL-decrement → flood). New logic is split into dedicated modules
to keep every file well under the 800-line ceiling.

- `crates/nimmesh-core/src/relay.rs` — the **G6 relay policy**:
  - **Degree-adaptive probabilistic relay.** In a sparse mesh (peer-degree below the
    high-degree threshold **6**) every flooded packet is always relayed; in a dense mesh
    each is relayed only with probability **0.5**, damping broadcast storms. The decision
    rides an **injectable, seeded `RelayRng`** (`XorShiftRng`) so tests are deterministic.
  - **Relay jitter 10–220 ms** before a rebroadcast, via an **injectable `RelayDelay`
    trait** — `RealDelay` (sleeps the worker thread) in production, `NoDelay` (zero-cost)
    in tests, so the suite never actually sleeps.
  - **`relayed_ttl`** — the loop-free TTL hop cap (`min(ttl, 7)` then decrement, drop at
    the floor); capping a hostile over-large TTL is what makes the flood provably
    loop-free.
  - A `RelayPolicy` bundles the RNG + delay + tunables; `production()` (real jitter, time
    seed) vs `deterministic()` (zero sleep, fixed seed) — the harness/tests use the latter.
- `crates/nimmesh-core/src/fragment.rs` — the **`fragment = 0x20`** split/reassemble path
  (defined-but-unused for today's ~205-B `nimiqTx`, implemented for larger/future
  payloads). Fragment header **8 B fragmentID + 2 B index + 2 B total + 1 B originalType**;
  `fragment_message` splits a payload at the BLE chunk (~469 B), and a bounded
  **`Reassembler`** (≤ **128** in-flight, oldest evicted; **30 s** lifetime via a
  caller-supplied logical clock) rebuilds it. Reassembled messages are dispatched with
  **TTL zeroed** (delivered locally, never re-flooded).
- `crates/nimmesh-core/src/engine.rs` — wires the above into the worker:
  - relays now run the **degree-adaptive decision → jitter → TTL hop cap → flood**;
  - **source-link exclusion** — a relay never echoes a packet back out the peer it
    arrived on (new `flood_excluding`; the inbound source peer is threaded from the radio
    callback through `process_inbound`);
  - the `fragment` type feeds the reassembler and dispatches the rebuilt message locally;
  - `WorkerState` now carries the `RelayPolicy` + `Reassembler` (worker-thread-local, no
    locks on the hot path). `txWire` stays **opaque** — no signing/broadcast (`// G3:` /
    `// G8:` anchors kept).
- `crates/nimmesh-core/src/node.rs` — new **`on_packet_received_from(peer, bytes)`** FFI
  method so the shim can attribute the source link (the source-unaware
  `on_packet_received` still works); the relay policy is injected at construction
  (production default; harness injects deterministic).
- `crates/nimmesh-core/src/mock_radio.rs` — the harness delivers with the source peer
  attributed and builds nodes with the **deterministic** (zero-sleep) policy.
- `crates/nimmesh-core/tests/relay_proptests.rs` — **property tests** (proptest):
  **loop-freedom** (TTL relay strictly terminates within the hop cap from any start),
  **dedup correctness** (a key is reported "fresh" at most once), **fragment round-trip**
  (any payload reassembles byte-for-byte, in any order), and **adaptive relay reaches the
  gateway** across a random connected sparse tree. Plus engine-level e2e tests for
  source-link exclusion and fragmented-receipt reassembly-then-settle.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (67 unit + 4 relay proptests + 5 wire proptests),
  and `scripts/size-guard.sh`. Non-money-path; opaque bytes only.

## [0.3.0] — 2026-06-26

### Added — G5 (Rust core): BLE mesh node + `BleRadio` seam (ADR-0002)

Builds the **architecture ADR-0002 fixes**: the BLE radio stays native and the Rust core
owns everything above the byte-stream seam, wired with UniFFI foreign traits as two
objects pointing at each other. Native iOS/Android shim is deferred (needs Xcode +
Andjroo's Apple ID); this is the **Rust-core part of #5**.

- `crates/nimmesh-core/src/radio.rs` — the **`BleRadio` foreign trait**
  (`#[uniffi::export(with_foreign)]`) the native shim implements: `start_advertising`,
  `start_scanning`, `send(peer_id, bytes)` (**fire-and-forget**), `disconnect(peer_id)`,
  `stop`. Rust holds `Arc<dyn BleRadio>` and only ever calls **out** to it. A "peer" is an
  opaque BLE connection identity — the radio never sees a TTL or a packet.
- `crates/nimmesh-core/src/node.rs` — **`MeshNode`** (`#[derive(uniffi::Object)]`), the
  object the shim calls **in** to on every BLE event: `on_peer_connected`,
  `on_peer_disconnected`, `on_packet_received(bytes)`, `on_send_result(peer, ok)`,
  `submit_local_tx(tx_wire)`. `on_packet_received` is **NON-BLOCKING** — it only enqueues
  to an internal channel and returns; a dedicated worker thread drains the queue and runs
  decode → dedup → TTL-relay, calling `radio.send` **off** the callback thread.
- `crates/nimmesh-core/src/engine.rs` — the real-packet **relay / gateway / origin**
  logic. Wires the **G4 codec** into the mesh: the temporary `MeshFrame` framing is gone,
  replaced by real nimmesh packets (`codec::encode`/`decode`, `MessageType::NimiqTx 0x30`
  + the TLV envelope, `nimiqTxReceipt 0x31`). Relays operate on **real packet headers**
  (TTL-decrement, blind LRU dedup on the `(type, senderID, timestamp)` header identity);
  `txWire` stays **opaque** (no signing/broadcast — `// G3:` / `// G8:` anchors kept).
- `crates/nimmesh-core/src/dedup.rs` — a bounded O(1) **LRU** "have I seen this?" set
  (not a bloom filter; capped against hostile-flood DoS, RISKS.md #4).
- `crates/nimmesh-core/src/mock_radio.rs` — **`MockRadio`** (a pure-Rust `BleRadio`) + a
  **`MockEther`** virtual topology with controllable **latency / loss / partition**, and a
  **`MeshHarness`** that wires N `MeshNode`s into a mesh. The headless `kind: mock` test
  substrate (RISKS.md Part A) — the whole demo loop runs under `cargo test`, no phone.
- `crates/nimmesh-core/src/e2e_tests.rs` — the **full headless end-to-end test**:
  `submit_local_tx(opaque_bytes)` on an offline origin → real-packet flood at TTL=7 → a
  blind relay (TTL-decrement + LRU dedup) → a mock gateway records the bytes + emits a
  `nimiqTxReceipt 0x31` → receipt propagates back → origin observes **Settled**. Plus the
  diamond-path single-submit, the rejected→Failed, latency, total-loss→Pending, and
  partition cases.
- **The four ADR-0002 callback gotchas, each engineered + tested:** (a) `on_packet_received`
  is non-blocking — a test asserts `radio.send` never re-enters synchronously on the
  callback thread (it only ever runs on the worker thread); (b) `send` is fire-and-forget,
  outcomes arrive via `on_send_result` — tested for both delivered + dropped hops; (c) the
  worker wraps each job in `catch_unwind` so a **panicking handler can't abort** the worker
  (tested with a panicking gateway) — the hot path is infallible; (d) the node↔radio
  refcount cycle is broken by a **weak edge** (node holds the radio strongly, the radio
  holds the node weakly) — a teardown/leak test proves `shutdown` releases the radio and
  the node is reclaimed with no leak.
- **Replaced** the G2 temporary `MeshFrame` framing + the `MeshTransport`/`MockMesh`
  broadcast substrate (and the `payment.rs` orchestrator) with the canonical ADR-0002
  radio model. `transport.rs` is slimmed to the shared value types (`TxId`, `mock_tx_id`,
  `MeshError`); `provider.rs` now bundles a `BleRadio` + `MeshGateway` behind
  `kind: mock | real`.
- Generated Swift + Kotlin bindings verified (`BleRadio` foreign protocol + `MeshNode`
  Rust-backed class) without Xcode / Android SDK.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (50 unit + 5 proptests), and `scripts/size-guard.sh`.

## [0.2.0] — 2026-06-26

### Added — G4: nimmesh wire protocol + packet codec (pure Rust)

- `crates/nimmesh-core/src/packet.rs` — the in-memory **packet model** and on-wire
  constants: the 14-byte big-endian header (`version=1`, `MessageType`, `ttl=7`,
  `timestamp`, `flags`, `payloadLength`), the five flag bits (`0x01` hasRecipient ·
  `0x02` hasSignature · `0x04` isCompressed · `0x08` hasRoute · `0x10` isRSR), and the
  `MessageType` enum (`fragment=0x20`, `requestSync=0x21`, `nimiqTx=0x30`,
  `nimiqTxReceipt=0x31`, `nimiqHeadBeacon=0x32`). `hasRecipient`/`hasSignature` are
  derived from field presence so the model can never disagree with the bytes.
- `crates/nimmesh-core/src/codec.rs` — byte-level `encode()` / `decode()` with strict,
  panic-free bounds checking, plus **PKCS#7-style block padding** up to the smallest of
  `[256, 512, 1024, 2048]`. Decode recomputes the exact packet length from the header
  and ignores trailing padding (no PKCS#7 unpad oracle); a real ~205-B `nimiqTx` pads
  cleanly into the 256 block. A typed `CodecError` rejects unknown version / type /
  flag bits and truncated frames.
- `crates/nimmesh-core/src/envelope.rs` — the **Nimiq TLV envelope** (`1B type | 1B len
  | value`): `0x01` txWire (required, **opaque** bytes), `0x02` networkId (required, 1B,
  default **testnet = 5**), `0x03` validUntil (u32 BE), `0x04` txId (32B), `0x05`
  encMemo, `0x06` wantReceipt. Unknown TLV types are skipped for forward-compat; fixed-
  width fields must carry their exact length; the two required fields must be present.
- `crates/nimmesh-core/tests/wire_proptests.rs` — `proptest` property/fuzz tests: `decode`
  and `decode_envelope` **never panic** on arbitrary/malformed input, and any valid
  packet / envelope (incl. an envelope nested inside a `nimiqTx` packet) round-trips
  byte-for-byte. Plus per-message-type round-trip, padding-block, and rejection unit
  tests.
- `txWire` is carried as **opaque bytes** end to end — no signing, no broadcast, no key
  material (that is G3/G8, money-path and Andjroo-gated). Non-money-path; auto-merge.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (28 tests), and `scripts/size-guard.sh`.
### Added — G2: provider seam + `MockMeshTransport` (mock pay-loop, no radio)

- **`transport.rs`** — the frozen `MeshTransport` seam (start/stop/`broadcast`/
  `set_receiver` + a `PacketHandler` deliver-via-callback), plus an in-memory,
  channel-based `MockMeshTransport` and a `MockMesh` graph that wires several virtual
  nodes into a relay topology — **no Bluetooth, no `tokio`, no external deps**. Carries
  **opaque `Vec<u8>`** payloads end to end. Includes a temporary `MeshFrame` mock
  framing (TTL + a `(type, txId)` dedup key around an opaque payload) with `// G4:`
  anchors marking where the real nimmesh packet codec replaces it.
- **`gateway.rs`** — the `MeshGateway` seam (`submit(txWire) -> Receipt`) + a
  record-only `MockGateway`/mock-RPC that stores submissions and emits a `Receipt`
  (`Accepted`/`Expired`/`Failed`), **no real network**. `// G8:` anchor marks where the
  real `sendRawTransaction` lands (money-path, gated).
- **`provider.rs`** — a `MeshProvider { kind: Mock | Real }` factory mirroring the
  fleet `ChainProvider kind:mock|real` pattern; `Mock` is fully wired, `Real` is a
  documented `// G5:` / `// G8:` seam stub.
- **`payment.rs`** — the `MeshPayment` orchestrator tying **origin → relay → gateway →
  receipt** (`OriginNode`/`RelayNode`/`GatewayNode`, `PaymentStatus`). Blind relays
  dedup + TTL-decrement + re-flood and **never inspect the opaque payload**.
  `// G3:` anchor marks where the real signed-tx bytes from `sign_offline()` ride the
  same `Vec<u8>` path.
- **End-to-end mock pay-loop test**: an opaque payload floods from an offline origin
  through ≥1 relay to a gateway, which records the submission and emits a receipt that
  propagates back; the origin observes `Settled`. Plus dedup-across-two-paths (one
  submission), reject→`Failed` (unconfirmed-until-inclusion honesty), and
  unreachable-gateway→`Pending` cases. 19 unit tests green locally
  (`fmt`/`clippy -D warnings`/`test --all`/size-guard). No version bump (loop tags at
  merge); non-money-path.

## [0.1.0] — 2026-06-26

### Added — G1: Rust core scaffold + UniFFI + CI

- Cargo **workspace** (`Cargo.toml`) + `crates/nimmesh-core/` — the shared, headless
  Rust core crate, built with `crate-type = ["cdylib", "staticlib", "lib"]` so it can
  back an Android `.so`, an iOS `.xcframework`, and the local Rust unit tests.
- UniFFI proc-macro surface (`uniffi::setup_scaffolding!()`) exposing a small but
  **real, unit-tested** API that proves the FFI boundary: `core_version()`,
  `default_network()`, a `NetworkId` enum (Testnet/Mainnet) with exported
  `network_wire_id()` / `network_is_loop_safe()` helpers, and a `echo_bytes()` binary
  round-trip. `G3:`/`G4:`/`G8:` TODO anchors mark where signing, the packet codec, and
  gateway broadcast land — none implemented (no money path in G1).
- `uniffi-bindgen` binary (`src/bin/uniffi-bindgen.rs`) so Swift + Kotlin bindings
  generate **without** Xcode or the Android SDK/NDK. Confirmed both languages emit
  cleanly into the git-ignored `bindings/generated/`.
- `scripts/size-guard.sh` — fails if any tracked `*.rs|*.swift|*.kt` file exceeds 800 lines.
- `.github/workflows/ci.yml` — a `core` job on `ubuntu-latest` mirroring the local
  gate (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all`, size-guard).
- `rust-toolchain.toml` pinning stable (+ clippy/rustfmt).
- Local gate green on the Mini: `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, and `cargo test --all` (5 unit tests) all pass.

## [0.0.1] — 2026-06-26

### Added
- Project bootstrap: `docs/GOAL.md` (north star, demo loop, core values),
  `docs/LOOP.md` (autonomous build contract, goals G1–G13, money-path gating),
  `docs/adr/0001` (native Swift + Kotlin + shared Rust core via UniFFI),
  `docs/PROTOCOL.md` (nimmesh wire format), `docs/RISKS.md` (offline-payment hazards),
  `nimiq-stack.json` (fleet manifest, marked exempt — native, not a web PWA), and the
  CI plan in `docs/ci/`.
- Outcome of the `nimmesh-design-spike` dynamic workflow (4 research agents + synthesis):
  empirically confirmed 139-byte signed transfer, RFC-8032 Ed25519 signing, Bitchat =
  Unlicense (portable), ~2 h validity window.
