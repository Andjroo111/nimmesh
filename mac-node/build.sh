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

# Sign with the Apple Development cert when the login keychain is reachable (i.e. when a HUMAN
# runs this from a GUI Terminal). A real code identity is REQUIRED for the macOS Bluetooth TCC
# prompt to appear at all — ad-hoc silently suppresses it (verified 2026-07-05). Falls back to
# ad-hoc for headless/automated builds (which can't reach securityd here — errSecInternalComponent),
# but an ad-hoc build will NOT get the Bluetooth prompt, so build.sh must be run by Andjroo.
DEV_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | grep -m1 'Apple Development' | awk '{print $2}')"
IDENTITY="${DEV_IDENTITY:--}"

echo "==> building the Rust core for macOS (release, staticlib, gateway-rpc)…"
# gateway-rpc compiles the real HTTP broadcast client in — the Mac node is the mesh's
# internet exit (testnet-guarded in the core; a mainnet host is refused at construction).
cargo build -p nimmesh-core --release --target aarch64-apple-darwin --lib --features gateway-rpc

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
if ! codesign --force --sign "$IDENTITY" \
  --entitlements "$ROOT/mac-node/nimmesh-node.entitlements" \
  "$APP" 2>/dev/null; then
  echo "!! real-cert signing failed (headless keychain?) — falling back to ad-hoc."
  echo "!! An ad-hoc build will NOT get the Bluetooth prompt. Run build.sh from a GUI Terminal."
  codesign --force --sign - --entitlements "$ROOT/mac-node/nimmesh-node.entitlements" "$APP"
fi

echo "==> built + signed: $APP"
echo "    run it with:  ./mac-node/run.sh   (first run pops a Bluetooth permission prompt — click Allow)"
