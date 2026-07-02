# Discovery-layer privacy (G45)

The swap **settlement** protocol is blind-relayed: a relay forwards swap packets as opaque bytes and
never parses terms, addresses, or signed blobs (core value #3). The **discovery** layer (G34–G44) is
different: a `SwapIntent` is *meant* to be read by strangers, so it floods mesh-wide in cleartext.
This document audits exactly what that leaks, scores it against the privacy / non-custodial core
values, and specifies the mitigation shipped in G45.

## What an intent carries

A flooded `SwapIntent` (see `swap_intent.rs`) is, in cleartext:

| Field | Purpose | Leaks |
|---|---|---|
| `gives`, `nim_amount`, `btc_amount` | the trade + rate | the **terms** (not identity) |
| `min_nim` / `max_nim` | size band (G40) | the size range the advertiser will do |
| `expiry_height` | freshness (G35) | roughly when the ad was made |
| `nim_pubkey` + `nim_address` | authenticate the intent (G41) + receive NIM | the advertiser's **NIM identity** |
| `btc_pubkey` + `btc_address` | the BTC HTLC keys | the advertiser's **BTC identity** |
| `signature` | authenticity (G41) | nothing beyond the key it's under |

## Threat model — a passive mesh observer

An observer in radio range (or any relay) that simply records floods learns, **per intent**:

1. **Identity ↔ trade linkage.** `nim_address` + `btc_pubkey`/`btc_address` tie a specific NIM (and
   BTC) identity to "wants to trade X NIM for Y BTC". If that NIM address is the advertiser's main
   wallet, the observer now knows that wallet is doing cross-chain swaps and for how much.
2. **Same-advertiser correlation.** If a node makes several advertisements **reusing the same keys**,
   every ad shares `nim_pubkey`/`nim_address`/`btc_pubkey`, so they all link to one entity — building
   a profile of its trading.
3. **Coarse timing/locality.** A flood is heard near its origin; `expiry_height` dates it. (Inherent
   to a broadcast mesh; out of scope here.)

Scored against the core values: settlement stays **non-custodial** (keys never leave the host; the
intent carries only public material). But discovery, unmitigated, is **poor on unlinkability** — it
publishes a stable identity↔trade map, the opposite of the privacy posture the relay layer holds.

## Mitigation shipped in G45 — ephemeral per-advertisement keys

**Advertise under fresh, throwaway keys, never your main wallet's.** The intent only needs *some* key
that (a) it can sign under (G41 authenticity, so it can't be forged in transit) and (b) it can later
claim the swap proceeds under. Nothing requires that key to be the advertiser's long-term identity.

So an advertiser generates, **per advertisement**:
- a fresh Ed25519 NIM keypair → `nim_pubkey`/`nim_address` + the `signature`, via
  [`sign_intent_ephemeral`](../../crates/nimmesh-core/src/swap_intent.rs);
- a fresh BTC keypair → `btc_pubkey`/`btc_address` (rotated by the app's BTC layer before signing).

After the swap settles, it **sweeps** the received NIM (and any BTC change) to its main wallet in a
separate, unlinked transaction.

This kills both **(1)** (the flooded identity is a throwaway, not the main wallet) and **(2)** (two
advertisements share no key, so they don't correlate). What still leaks is only the **terms** — "*some*
node wants this trade" — which is the irreducible minimum for a discovery layer to function, and which
names nobody. The mechanism is mechanically identical to G41 signing; `sign_intent_ephemeral` exists so
the privacy-preserving path is the obvious, named one. `swap_privacy_tests.rs` proves two ephemeral
advertisements share no identity field yet each still `verify_authentic`s, and that an ephemeral-keyed
intent still discovers + settles end to end.

## Residual leaks & deeper mitigations (not in G45)

- **BTC-pubkey linkage to the HTLC.** The BTC pubkey in the intent is the one the on-chain HTLC uses,
  so an observer who later watches the BTC chain can link the intent to that HTLC. Ephemeral BTC keys
  (above) break the *cross-advertisement* link but not the *intent ↔ its own HTLC* link.
- **Amount fingerprinting.** Identical, unusual amounts across ads weakly suggest one origin even with
  rotated keys. Padding/bucketing amounts would help.
- **Commit-then-reveal addresses.** Flooding only a *commitment* to the addresses and revealing them
  only on `Propose` (to the matched counterparty) would hide addresses from non-matching observers
  entirely — but it changes the match → propose handshake and the money-path address wiring.

**BLOCKED (needs a human/architecture decision):** commit-reveal addressing and any on-chain mixing
touch the gated money-path and the match handshake; they are recorded here for a future, owner-gated
goal rather than implemented autonomously.
