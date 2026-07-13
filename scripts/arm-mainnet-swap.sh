#!/usr/bin/env bash
# arm-mainnet-swap.sh — PREPARE (not merge) the PR that ARMS the mainnet swap.
#
#   scripts/arm-mainnet-swap.sh <deployed-htlc-address> [--dry-run]
#
# The §8.3 first-mainnet-swap wiring is INERT on a merged branch: the master switch
# `mainnet_swap::MAINNET_SWAP_ENABLED` is `false` and `MAINNET_HTLC_ADDRESS` is empty, so every
# mainnet swap constructor refuses. This script performs the ONE deliberate arming act — AFTER
# Andjroo has deployed + source-verified the `NimmeshForwarder`/`NimmeshHtlc` on Polygon mainnet:
#
#   1. records the deployed HTLC in `MAINNET_HTLC_ADDRESS`,
#   2. flips `MAINNET_SWAP_ENABLED` to `true`,
#   3. bumps the workspace version,
#   4. writes the CHANGELOG arming entry,
#   5. creates a branch + opens a PR labelled `needs:owner` + `money-path`.
#
# It ONLY PREPARES the PR. Merging the armed change stays Andjroo's explicit click. The agent /
# autonomous loop NEVER runs this — arming real funds is a human act.
#
# --dry-run: do everything locally (edit + commit on a scratch branch) but DO NOT push or open a
# PR — for verifying the produced diff without arming anything. Clean up the scratch branch after.
set -euo pipefail

# --- locate the repo + the files we edit ------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SWAP_RS="$REPO_ROOT/crates/nimmesh-core/src/mainnet_swap.rs"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"

die() { echo "arm-mainnet-swap: $*" >&2; exit 1; }

# --- parse args -------------------------------------------------------------------------------
HTLC=""
DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        0x*) HTLC="$arg" ;;
        *) die "unexpected argument '$arg' (expected a 0x… HTLC address or --dry-run)" ;;
    esac
done

[ -n "$HTLC" ] || die "usage: scripts/arm-mainnet-swap.sh <deployed-htlc-address> [--dry-run]"
# A deployed HTLC address is a 20-byte 0x-hex — refuse anything else so we never arm a typo.
[[ "$HTLC" =~ ^0x[0-9a-fA-F]{40}$ ]] || die "HTLC address must be a 20-byte 0x-hex (got: $HTLC)"

[ -f "$SWAP_RS" ] || die "cannot find $SWAP_RS"
[ -f "$CARGO_TOML" ] || die "cannot find $CARGO_TOML"
[ -f "$CHANGELOG" ] || die "cannot find $CHANGELOG"

command -v git >/dev/null || die "git not found"
[ "$DRY_RUN" -eq 1 ] || command -v gh >/dev/null || die "gh CLI not found (needed to open the PR)"

# --- clean tree + fresh branch off origin/main ------------------------------------------------
cd "$REPO_ROOT"
[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first"

echo "==> git fetch origin"
git fetch origin --quiet
git rev-parse --verify origin/main >/dev/null 2>&1 || die "origin/main not found after fetch"

# Read the version + double-arm state from ORIGIN/MAIN (not the working tree) — the arm branch
# is cut from there, so this is what the checkout will actually contain.
CUR_VERSION="$(git show origin/main:Cargo.toml | grep -m1 '^version = "' | sed -E 's/^version = "([^"]+)".*/\1/')"
[ -n "$CUR_VERSION" ] || die "could not read the workspace version from origin/main:Cargo.toml"
IFS='.' read -r MAJ MIN PAT <<<"$CUR_VERSION"
[ -n "$MAJ" ] && [ -n "$MIN" ] && [ -n "$PAT" ] || die "unexpected version format '$CUR_VERSION'"
NEXT_VERSION="${MAJ}.$((MIN + 1)).0"
BRANCH="arm/mainnet-swap-v${NEXT_VERSION}"
DATE="$(date +%Y-%m-%d)"

# --- refuse to double-arm (checked against origin/main's source) ------------------------------
SWAP_RS_REL="crates/nimmesh-core/src/mainnet_swap.rs"
git show "origin/main:$SWAP_RS_REL" | grep -q 'pub const MAINNET_SWAP_ENABLED: bool = false;' \
    || die "origin/main's MAINNET_SWAP_ENABLED is not 'false' — already armed, or the source moved. Refusing."
git show "origin/main:$SWAP_RS_REL" | grep -q 'pub const MAINNET_HTLC_ADDRESS: &str = "";' \
    || die "origin/main's MAINNET_HTLC_ADDRESS is not empty — already armed, or the source moved. Refusing."

echo "==> arming plan:"
echo "      HTLC (escrow contract) : $HTLC"
echo "      version                : $CUR_VERSION -> $NEXT_VERSION"
echo "      branch                 : $BRANCH (off origin/main)"
echo "      dry-run                : $([ "$DRY_RUN" -eq 1 ] && echo yes || echo NO — will push + open PR)"

git rev-parse --verify "$BRANCH" >/dev/null 2>&1 && die "branch $BRANCH already exists — delete it or bump the version first"
git checkout -q -b "$BRANCH" origin/main

# --- 1 + 2: flip the flag + record the deployed HTLC ------------------------------------------
perl -0pi -e 's/pub const MAINNET_SWAP_ENABLED: bool = false;/pub const MAINNET_SWAP_ENABLED: bool = true;/' "$SWAP_RS"
perl -0pi -e "s/pub const MAINNET_HTLC_ADDRESS: &str = \"\";/pub const MAINNET_HTLC_ADDRESS: &str = \"$HTLC\";/" "$SWAP_RS"

grep -q "pub const MAINNET_SWAP_ENABLED: bool = true;" "$SWAP_RS" || die "failed to flip MAINNET_SWAP_ENABLED"
grep -q "pub const MAINNET_HTLC_ADDRESS: &str = \"$HTLC\";" "$SWAP_RS" || die "failed to record MAINNET_HTLC_ADDRESS"

# --- 3: bump the workspace version ------------------------------------------------------------
perl -0pi -e "s/^version = \"$CUR_VERSION\"/version = \"$NEXT_VERSION\"/m" "$CARGO_TOML"
grep -q "^version = \"$NEXT_VERSION\"" "$CARGO_TOML" || die "failed to bump the version"

# --- 4: prepend the CHANGELOG arming entry ----------------------------------------------------
ENTRY_FILE="$(mktemp)"
cat >"$ENTRY_FILE" <<EOF
## [$NEXT_VERSION] — $DATE — ARM MAINNET SWAP (needs:owner, money-path)

### The deliberate arming act — real mainnet funds

This flips the single master switch from OFF to ON. Until this entry, the mainnet swap path was
inert: \`mainnet_swap::MAINNET_SWAP_ENABLED = false\` and \`MAINNET_HTLC_ADDRESS\` was empty, so every
mainnet swap constructor refused. This change:

- records the deployed, source-verified \`NimmeshHtlc\` escrow on Polygon mainnet:
  \`$HTLC\`;
- sets \`mainnet_swap::MAINNET_SWAP_ENABLED = true\`.

With both set, \`mainnet_swap_armed()\` is now \`true\` and the mainnet live-swap constructors
(\`MeshNode::new_live_swap_initiator_mainnet\` / \`…responder_mainnet\`) assemble the mainnet money path
(native USDC, chain id 137, mainnet confirmation depths, the hard \`SwapCaps::mainnet_first_swap\`
caps). The first swap is a ≤ \$5 self-swap between Andjroo's own wallets, watched, timelock-refunded.

**This is a \`money-path\` change — Andjroo-merge only. The C1 live-safety gate + SwapCaps are
unchanged; only the master switch + the escrow address moved.**

EOF

# Insert the entry right after the CHANGELOG intro line (keep the "# Changelog" header + blurb).
HEADER_LINE="$(grep -n '^All notable changes' "$CHANGELOG" | head -1 | cut -d: -f1)"
[ -n "$HEADER_LINE" ] || die "could not find the CHANGELOG intro line"
INSERT_AT=$((HEADER_LINE + 1))
{
    head -n "$INSERT_AT" "$CHANGELOG"
    echo ""
    cat "$ENTRY_FILE"
    tail -n +"$((INSERT_AT + 1))" "$CHANGELOG"
} >"$CHANGELOG.tmp"
mv "$CHANGELOG.tmp" "$CHANGELOG"
rm -f "$ENTRY_FILE"

# --- commit on the arm branch (NEVER main) ----------------------------------------------------
CUR_BRANCH="$(git branch --show-current)"
[ "$CUR_BRANCH" = "$BRANCH" ] || die "expected to be on $BRANCH but on '$CUR_BRANCH' — refusing to commit"

git add "$SWAP_RS" "$CARGO_TOML" "$CHANGELOG"
COMMIT_MSG="ARM mainnet swap: enable master switch + record HTLC $HTLC (v$NEXT_VERSION) [needs:owner, money-path]"
git commit -q -m "$COMMIT_MSG" \
    -m "Flips mainnet_swap::MAINNET_SWAP_ENABLED to true and records the deployed NimmeshHtlc escrow. money-path — Andjroo-merge only." \
    -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"

echo "==> committed the armed change on $BRANCH:"
git --no-pager show --stat --oneline HEAD | head -20

PR_BODY_FILE="$(mktemp)"
cat >"$PR_BODY_FILE" <<EOF
> **money-path + needs:owner — Andjroo-merge only. Do NOT auto-merge.**

This PR is the **deliberate arming act** for the first mainnet swap. It flips the single master
switch and records the deployed escrow contract — nothing else about the money path changes.

## What this arms

- \`mainnet_swap::MAINNET_HTLC_ADDRESS\` = \`$HTLC\` (the deployed, source-verified \`NimmeshHtlc\` on
  Polygon mainnet).
- \`mainnet_swap::MAINNET_SWAP_ENABLED\` = \`true\`.

With both set, \`mainnet_swap_armed()\` is \`true\`, so the mainnet live-swap constructors stop refusing
and assemble the code-pinned mainnet money path (native USDC \`0x3c49…3359\`, chain id 137, mainnet
confirmation depths NIM 10 / USDC 64, and the hard \`SwapCaps::mainnet_first_swap\` ≤ 50 NIM / ≤ 5
USDC caps). The app's Swap sheet then shows the loud "REAL MAINNET FUNDS" labels.

## Before you merge

- [ ] The HTLC above is the address you deployed + source-verified on polygonscan.
- [ ] \`trustedForwarder()\` and \`token()\` (native USDC) check out on that contract.
- [ ] You have funded the responder's derived MAINNET EVM address with real USDC + POL (shown in the
      app's responder panel, tap-to-copy — same address as testnet, EVM addresses are chain-agnostic).
- [ ] This is a ≤ \$5 self-swap between your own wallets; you press every button and watch each leg.

## Safety unchanged

The C1 live-safety gate, the chain-backed verifiers, and the SwapCaps are **not touched** — only the
master switch + the escrow address moved. Full green gate (fmt + clippy + \`cargo test\`
\`--all\`/\`--all-features\` + size-guard) still required.

🤖 Prepared by scripts/arm-mainnet-swap.sh — merging stays your click.
EOF

if [ "$DRY_RUN" -eq 1 ]; then
    echo ""
    echo "==> DRY RUN — NOT pushing and NOT opening a PR."
    echo "    PR body prepared at: $PR_BODY_FILE"
    echo "    Inspect the diff on branch '$BRANCH', then clean up with:"
    echo "        git checkout - && git branch -D $BRANCH"
    echo ""
    echo "    (For the real arming, re-run WITHOUT --dry-run.)"
    exit 0
fi

echo "==> git push -u origin $BRANCH"
git push -q -u origin "$BRANCH"

echo "==> gh pr create (labels: needs:owner, money-path)"
gh pr create \
    --title "ARM mainnet swap: enable + record HTLC (v$NEXT_VERSION) — Andjroo-merge only" \
    --body-file "$PR_BODY_FILE" \
    --label "needs:owner" \
    --label "money-path" \
    --head "$BRANCH"
rm -f "$PR_BODY_FILE"

echo ""
echo "==> Done. The arming PR is OPEN and waiting for YOUR review + merge."
echo "    Nothing is armed until you merge it."
