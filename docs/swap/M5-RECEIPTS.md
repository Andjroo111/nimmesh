# M5 receipts — RPC-trust hardening (G8 M5, testnet-only)

Proof for the G8 **M5** hardening (ADR-0011): the verifier cross-read, the NIM content-hash bind,
the independent reveal confirmation, and the `word_u64` overflow guard. **Testnet/Amoy only — no
funds moved, no key used** (the cross-read is proven read-only against real infra; the settlement
and lying-RPC paths are proven by offline unit tests against mock RPC seams).

## 1. Live cross-read on real infra (read-only, no funds, no key)

`examples/live_rpc_cross_read.rs` performs the EXACT head cross-read the funding verifiers do:
trust a depth only when an independent second endpoint agrees within `HEAD_CROSS_TOLERANCE_BLOCKS`
(12), using the conservative (lower) head.

```
$ cargo run --example live_rpc_cross_read --features polygon-gateway,gateway-rpc

live_rpc_cross_read — M5 head cross-read vs REAL testnet infra (read-only, no funds)

Amoy primary    https://rpc-amoy.polygon.technology
     head = 41841154
Amoy secondary  https://polygon-amoy-bor-rpc.publicnode.com
     head = 41841154
  |Δ head| = 0  (tolerance 12 blocks) · conservative head = 41841154
  OK — Amoy cross-read AGREES; the verifier trusts the conservative depth.

NIM testnet     https://rpc.testnet.nimiqwatch.com
     head = 5606264
  (single public endpoint — a second INDEPENDENT testnet RPC is a self-hosted node,
   an M5/mainnet-gating item; wire it via NimHtlcVerifier::with_secondary when available.)

M5 live cross-read proof COMPLETE — honest infra passes the head-agreement gate.
```

Run 2026-07-09. **Amoy** uses two GENUINELY independent public endpoints (different operators);
they agreed exactly (`Δ = 0`), so the verifier trusts the conservative depth — the honest-infra
path passes. **NIM testnet** has one public endpoint (`rpc.testnet.nimiqwatch.com`); a second
INDEPENDENT testnet RPC is a self-hosted node (an M5/mainnet-gating item, ADR-0011 §4), so the NIM
cross-read is *seam-ready* (`NimHtlcVerifier::with_secondary`) but not exercised against two public
NIM endpoints today.

### 1b. The funds-moving verifier is wired to cross-read too (Andjroo-gated run)

`examples/live_amoy_verifier.rs` (the #72 verifier vs a REAL 1-USDC self-escrow) now constructs
its `PolygonHtlcVerifier` with `.with_secondary(HttpPolygonRpc::new(AMOY_RPC_URL_2))`, so the
verifier's actual `observe()` path exercises the head cross-read against two genuinely independent
Amoy providers on the money path. It moves real testnet USDC + POL, so the RUN is human-gated
(`AMOY_TEST_KEY` + a funded key + POL — not available to the loop); the wiring compiles in CI and
runs whenever Andjroo supplies the key. The read-only `live_rpc_cross_read` above proves the same
head-agreement gate without funds.

## 2. Lying-RPC unit tests (offline, deterministic mock seams)

Each fails **closed** when the RPC lies, and **passes** when the sources agree:

| Test | Lie modeled | Asserted |
|---|---|---|
| `nim_verifier::a_returned_tx_whose_hash_is_not_the_content_digest_is_refused` | node echoes a foreign hash + real-looking height | `Absent` (bind); honest hash → `Found` |
| `nim_verifier::a_secondary_that_disagrees_on_inclusion_fails_closed_and_agreement_passes` | secondary unseen / disagrees on inclusion block | `Absent`; agreement → `Found` |
| `nim_verifier::a_shallower_secondary_head_defeats_a_primary_that_inflates_depth` | primary inflates `head` to fake depth | conservative depth → gate `TooShallow` |
| `polygon_verifier::a_secondary_head_disagreeing_beyond_tolerance_fails_closed` | secondary head far off | `Absent` |
| `polygon_verifier::an_agreeing_secondary_within_tolerance_uses_the_conservative_head` | primary inflates within tolerance | depth uses min head |
| `polygon_verifier::a_secondary_whose_head_read_errors_fails_closed` | secondary transport error | `Absent` |
| `amoy_verifier_cross_read_requires_an_agreeing_secondary` | head disagree / secondary escrow not `Live` | `Absent`, never recorded; agreement → `Found` |
| `polygon_verifier::word_u64_refuses_over_64_bit_words_instead_of_truncating` (+ amoy twin) | `> 2^64` ABI word | `None`, log skipped |
| `a_withdraw_receipt_not_backed_by_a_claimed_escrow_withholds_the_reveal` | successful+buried withdraw receipt, escrow NOT `CLAIMED` | `S` withheld until `CLAIMED` re-read |

Reproduce: `cargo test -p nimmesh-core --features polygon-gateway,gateway-rpc --lib`.

## 3. What remains mainnet-gated (needs:owner)

Per ADR-0011 §4 and `docs/MAINNET-GATING.md` §8.2/§8.4: wiring a **trusted / self-hosted**
secondary endpoint on the live path, **M6** (mainnet confirmation-depth retune), and the guard-lift
review. Those are deliberately NOT done here.
