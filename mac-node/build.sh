#!/bin/bash
# Build the nimmesh Mac mesh node as a signed .app bundle so macOS will grant it Bluetooth.
# Produces mac-node/nimmesh-node.app; run it (stdout visible) with mac-node/run.sh.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root (worktree)

ROOT="$(pwd)"
GEN="$ROOT/crates/nimmesh-core/generated"
LIB="$ROOT/target/aarch64-apple-darwin/release/libnimmesh_core.a"
APP="$ROOT/mac-node/nimmesh-node.app"
BIN="$APP/Contents/MacOS/nimmesh-node"

# Ad-hoc sign (identity "-"): this headless Mac's Keychain/securityd blocks real-cert signing
# (errSecInternalComponent — same securityd XPC issue that blocks 1Password `op` here). Ad-hoc
# carries the Bluetooth usage string + entitlement, which is what the TCC prompt needs; the
# only cost is the grant resets on each rebuild (re-approve once after building).
IDENTITY="-"

echo "==> building the Rust core for macOS (release, staticlib)…"
cargo build -p nimmesh-core --release --target aarch64-apple-darwin --lib

echo "==> compiling the Swift node…"
mkdir -p "$APP/Contents/MacOS"
swiftc \
  -O \
  -target arm64-apple-macos12 \
  -o "$BIN" \
  -I "$GEN" \
  -Xcc -fmodule-map-file="$GEN/nimmesh_coreFFI.modulemap" \
  "$GEN/nimmesh_core.swift" \
  "$ROOT/mac-node/BleMeshRadio.swift" \
  "$ROOT/mac-node/main.swift" \
  -L "$(dirname "$LIB")" -lnimmesh_core \
  -framework CoreBluetooth -framework Foundation -framework Security -framework SystemConfiguration

echo "==> assembling + signing the .app (identity: $IDENTITY)…"
cp "$ROOT/mac-node/Info.plist" "$APP/Contents/Info.plist"
codesign --force --sign "$IDENTITY" \
  --entitlements "$ROOT/mac-node/nimmesh-node.entitlements" \
  "$APP"

echo "==> built + signed: $APP"
echo "    run it with:  ./mac-node/run.sh   (first run pops a Bluetooth permission prompt — click Allow)"
