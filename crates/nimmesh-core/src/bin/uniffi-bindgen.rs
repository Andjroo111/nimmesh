//! Standalone `uniffi-bindgen` entry point for nimmesh-core.
//!
//! Lets us generate the Swift and Kotlin foreign-language bindings from the compiled
//! `nimmesh_core` library **without** Xcode or the Android SDK/NDK — which is exactly
//! what G1 needs (native toolchains land at G5). Once the `cdylib` is built, run e.g.:
//!
//! ```text
//! cargo run --bin uniffi-bindgen -- generate \
//!     --library target/debug/libnimmesh_core.dylib \
//!     --language swift --language kotlin \
//!     --out-dir bindings/generated
//! ```
//!
//! The generated bindings are build artifacts and are git-ignored, never committed.
fn main() {
    uniffi::uniffi_bindgen_main()
}
