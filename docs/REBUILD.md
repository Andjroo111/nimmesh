# The nimmesh rebuild — the wallet's mesh edition (contract)

> Andjroo, 2026-07-02: rebuild the app to match **wallet.nimiq.com** — "dig through every
> piece and component to make sure that it's pixel perfect to the actual wallet" — and make
> it the mesh version. This file is the contract: the reference for every screen, what is
> cloned, what is adapted, and what is mesh-native.

## The reference library (complete, 2026-07-02)

`~/Projects/nimiq/nimiq-branding-cli/references/screenshots/wallet-app/logged-in/`
(+ `sweep/`, see its README). Key screens:

| Screen | Reference |
| --- | --- |
| Home = portfolio overview (empty assets) | `sweep/01-home-overview.png` (testnet: TOTAL BALANCE, NIM card w/ address row, Bitcoin "Activate" card, Help pill, bottom bar) |
| Home with activated BTC + USDC cards & swap arrows | `sweep/andjroo-home-activated-assets.png` |
| Navy side menu (charts, Buy/Sell, swap donut, account row, Network, Settings) | `sweep/andjroo-side-menu.png` |
| Account modal (Login File / Create Backup / Rename / Change password / Export History / Logout / Add account) | `sweep/andjroo-account-modal.png` |
| Address detail (search, header, staking banner, contact chip, THIS MONTH tx groups) | `sweep/andjroo-address-detail-staked.png`, `sweep/07-address-detail.png` |
| Network screen (stat stack + hex map + explainer card) | `sweep/06-network.png` |
| Loading page (hexagon spinner) | `sweep/andjroo-hub-loading.png` — DONE in v0.48.5 |
| Send / Receive flows | `logged-in/send-*`, `receive-*` — largely DONE (v0.37) |
| Backup flows (hub, words+validate, codes) | Andjroo's keyguard captures — DONE (v0.38–0.41) |
| Scanner | Andjroo's captures — DONE (v0.48.4) |

Pixel-verified components: the `nq` registry (40). Upstream truth: `nimiq/wallet` source.

## The phases

1. **Home → the wallet's portfolio overview.** Hamburger (menu) top-left + globe (language)
   top-right; TOTAL BALANCE eyebrow + big fiat; the NIM asset card (identicon address row:
   name, NIM, fiat, chevron); Bitcoin + USDC cards (Activate state until the swap engine's
   legs land real balances; swap arrows between cards open the swap sheet); Help pill;
   bottom bar unchanged (Receive | Swap | Send | scan). The current home content becomes…
2. **Address detail (drill-in).** Back + "Search transactions" + ⋮; identicon + name + full
   copyable address + NIM/fiat; the MESH STATUS banner where the wallet shows the staking
   banner (green, mesh glyph: "meshed · N nearby" — the mesh identity, same slot); THIS
   MONTH/date-grouped tx list (existing rows).
3. **The navy side menu.** NIM/BTC 24h price charts (CoinGecko, already fetched), the
   portfolio donut + Swap pill, the account row (identicon + name), Network, Settings
   (language + log out relocate here). Buy/Sell OMITTED (no fiat ramps in nimmesh).
4. **Account modal on the wallet's layout.** Create Backup (→ our hub), Rename (new,
   local label), Export History (CSV of the tx list), Logout (existing). Save Login File /
   Change password OMITTED (nimmesh = Face ID + backup codes). Add account LATER.
5. **The Network screen, mesh-native.** The wallet's stat-stack layout with OUR numbers:
   MESH (peers nearby, relay posture), CONSENSUS (RPC head), FEE $0/tx, TX TIME 1-2 sec,
   "you helped N payments" (G20 relay stats over FFI), the explainer card rewritten for
   the mesh ("With nimmesh, you ARE the network").

## Andjroo's keeps (2026-07-02)

- **The mesh status line stays** — `mesh offline · 0 nearby · core X · head N` — it is the
  product's identity. It lives above the bottom action bar on every main screen (home +
  address detail), where the wallet has nothing comparable.
- **The language selector stays and stays visible** — the flag pill goes top-right on the
  home, in exactly the slot where the real wallet shows its globe/language icon. Same
  offline flag-hex pill, same language sheet.
- **The account/profile pill at the top is PROVISIONAL** — keep it until the side menu
  (Phase 3) + account modal (Phase 4) provide account access the wallet's way, then remove
  it from the home chrome.

## The loop

One phase-slice per cycle, autonomous, merge-on-green (Andjroo's standing grant):

1. Branch from fresh `origin/main` in the `nimiq.nimmesh-ui` worktree.
2. Build the slice; verify = Playwright 390px screenshot-diff vs the mapped reference +
   `nq lint` 0 errors + `node --check` on the module script.
3. `build-adhoc.sh` (full, so the core version string matches), refresh `ota/`
   (align the version to main+1 RIGHT before commit — the swap track ships constantly).
4. PR → CI green (rerun once if the flaky swap stress test trips) → squash-merge →
   verify the DEPLOYED manifest by reading the bundle-version line.
5. Tick the phase below, reschedule the next cycle.

Progress: [x] P1 home overview + address drill-in (v-P1, 2026-07-03) · [x] P2 side menu (2026-07-03) · [x] P3 account
modal (2026-07-03) · [x] P4 mesh Network screen (2026-07-03) · [ ] P5 final diff-everything polish pass.

## Rules

- Every screen ships only after a screenshot-diff against its reference (390px, the
  Playwright harness with the mocked bridge) + `nq lint` 0 errors.
- Components come from the registry (`nq add`) or the wallet source — never hand-drawn.
- Mesh-native additions (mesh bar, discovery, sim labels) stay loud — this is the mesh
  edition, not a clone with the mesh hidden.
- Work happens in the `nimiq.nimmesh-ui` worktree; branch from fresh origin/main; align
  the version right before commit (the swap track ships constantly).
