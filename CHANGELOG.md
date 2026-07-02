# Changelog

All notable changes to nimiq.nimmesh. Each PR bumps the version and adds an entry.

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
