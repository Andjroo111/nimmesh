#!/usr/bin/env bash
# android-env.sh: the ONE place the Android toolchain paths live.
#
# Source this from every Android build script:
#   . "$(dirname "$0")/android-env.sh"
#
# Nothing here may rely on a login shell. launchd, and any agent-run job, starts with a
# bare environment, and this repo has been bitten by that before (ADR-0002 names it).
# Every path is absolute and every variable is exported explicitly.
#
# An ALREADY-SET variable always wins, so the same script serves two very different
# hosts: the Mac Mini, where the toolchain lives at the pinned paths below, and GitHub
# Actions, where setup-java and setup-android have already exported their own.
#
# Installed 2026-08-23 on the Mac Mini (arm64), all PINNED, not "latest":
#
#   JDK 17.0.20.1 (Temurin)     ~/tools/jdk-17
#   cmdline-tools 21.0          ~/Library/Android/sdk/cmdline-tools/latest
#   platform-tools, android-36, build-tools 36.0.0
#   NDK r27.3.13750724 (LTS)
#   Gradle 9.7.1                ~/tools/gradle-9.7.1 (only for `gradle wrapper`; builds use ./gradlew)
#   cargo-ndk 4.1.2             ~/.cargo/bin
#
# ⚠ cmdline-tools is pinned at 21.0 ON PURPOSE. From 22.0 Google replaced the pure-Java
# `sdkmanager` with a NATIVE `android` binary published for macOS as x86_64 ONLY. On this
# arm64 Mini that fails with "Bad CPU type in executable" and would need Rosetta. 21.0 is
# the last revision that runs on the JDK alone. Do not bump it without first checking for
# an arm64 build of the `android` binary.
set -euo pipefail

ANDROID_NDK_VERSION="27.3.13750724"

export JAVA_HOME="${JAVA_HOME:-$HOME/tools/jdk-17/Contents/Home}"
export ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$ANDROID_NDK_VERSION}"

PATH="$JAVA_HOME/bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$HOME/.cargo/bin:$PATH"
# Only on the Mini: the standalone Gradle exists to run `gradle wrapper`. Every build
# goes through android/gradlew, so its absence is not an error.
[ -d "$HOME/tools/gradle-9.7.1/bin" ] && PATH="$HOME/tools/gradle-9.7.1/bin:$PATH"
export PATH

# Fail loudly and specifically, rather than letting a build die three steps later with an
# unrelated message.
for p in "$JAVA_HOME/bin/java" "$ANDROID_NDK_HOME/source.properties"; do
    [ -e "$p" ] || { echo "android-env: MISSING $p (see docs/ANDROID.md, section A0)"; exit 1; }
done
command -v cargo-ndk >/dev/null || { echo "android-env: cargo-ndk not on PATH (cargo install cargo-ndk)"; exit 1; }
