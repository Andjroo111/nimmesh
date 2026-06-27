# CI plan — nimiq.nimmesh

**Rust is installed on the Mini**, so the **Rust core is gated LOCALLY** (`cargo test`
+ `cargo clippy` + `cargo fmt`). **GitHub Actions is a backstop + the cross-platform
check** (Linux re-run of the core gate, plus the iOS/Android jobs once those targets
exist). The gh token has `workflow` scope, so the loop arms `.github/workflows/` as part
of **G1**. Native toolchains (Xcode + Android SDK/NDK) are added at G5. Keep this file in
sync with the workflow.

## Jobs

### `core` (ubuntu-latest) — the headless gate, runs on every PR
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all` (unit + property + the full **mock pay-loop end-to-end**:
  sign → relay → gateway → receipt against `MockMeshTransport` + mock RPC)
- file-size guard: fail if any tracked source file > 800 lines

### `ios` (macos-latest) — added when the iOS target exists (G1/G5)
- install the Rust aarch64-apple-ios* targets; `cargo-swift` build the `.xcframework`
- `xcodebuild build` the app target (and `test` where simulator-runnable)

### `android` (ubuntu-latest) — added when the Android target exists (G1/G5)
- install Android SDK/NDK; `cargo-ndk` build the `.so` + Kotlin bindings
- `./gradlew assembleDebug` + `./gradlew testDebugUnitTest`

## Merge policy (enforced by the loop, not CI)
- **Non-money-path + all required jobs green → squash-merge.** New repo is unprotected,
  so do **not** rely on `gh pr merge --auto`: `gh pr checks <pr> --watch`, then
  `gh pr merge <pr> --squash --delete-branch` once green.
- **Money-path PRs** (sign / keys / broadcast) → never auto-merge; label `needs:owner`,
  stop, and report. (See `docs/LOOP.md`.)

## Not covered by CI (Andjroo's Mac + real phones)
On-device BLE mesh interop (iOS↔iOS, iOS↔Android, Android↔Android), background-relay
survival, TestFlight / Play internal builds, and look-and-feel.
