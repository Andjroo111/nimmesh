#!/bin/bash
# Run the nimmesh Mac mesh node with its logs in your terminal. Executes the binary INSIDE
# the signed .app bundle, so macOS attributes Bluetooth to the signed node (not the terminal)
# while you still see the output. First run: a Bluetooth permission dialog appears on the
# Mac's screen — click Allow (once). Ctrl-C to stop.
set -euo pipefail
cd "$(dirname "$0")/.."
APP="$(pwd)/mac-node/nimmesh-node.app"
[ -x "$APP/Contents/MacOS/nimmesh-node" ] || { echo "Not built yet — run ./mac-node/build.sh first."; exit 1; }
exec "$APP/Contents/MacOS/nimmesh-node"
