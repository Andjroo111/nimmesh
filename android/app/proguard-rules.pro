# R8 keep rules for the release build.
#
# The danger here is specific: a mangled FFI surface fails at RUNTIME on a user's phone, not
# at build time, and this app has no crash reporting behind it. So every rule below is
# verified by installing the minified APK and running the wallet self-test on device
# (BouncyCastle Ed25519 signing, accepted by the Rust ed25519-dalek verifier). If that reports
# signedOk=true on a minified build, the FFI survived R8.

# ---- UniFFI + JNA -------------------------------------------------------------------------
# JNA maps Kotlin interfaces and Structures onto native symbols BY NAME through reflection.
# R8 cannot see those uses, so renaming or removing any of it breaks the binding at runtime.
-keep class com.sun.jna.** { *; }
-keep interface com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { *; }
-keep class * implements com.sun.jna.Library { *; }
-keep class * implements com.sun.jna.Callback { *; }

# The generated bindings are the FFI surface itself: callback interfaces the Rust core invokes
# by name, and the structs that cross the boundary.
-keep class uniffi.nimmesh_core.** { *; }
-keep interface uniffi.nimmesh_core.** { *; }

# Our own implementations of the Rust foreign traits. Rust calls INTO these.
-keep class com.nimmesh.app.ble.** { *; }
-keep class com.nimmesh.app.wallet.Ed25519Key { *; }

# The @JavascriptInterface bridge is reached from JavaScript by method name.
-keepclassmembers class com.nimmesh.app.Bridge {
    @android.webkit.JavascriptInterface <methods>;
}

# ---- BouncyCastle -------------------------------------------------------------------------
# Only the Ed25519 signer and the PBKDF2 generator are used, so the rest can go: that is where
# most of the size saving is. These are the entry points reached directly.
-keep class org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters { *; }
-keep class org.bouncycastle.crypto.params.Ed25519PublicKeyParameters { *; }
-keep class org.bouncycastle.crypto.signers.Ed25519Signer { *; }
-keep class org.bouncycastle.crypto.generators.PKCS5S2ParametersGenerator { *; }
-keep class org.bouncycastle.crypto.digests.SHA512Digest { *; }
-keep class org.bouncycastle.crypto.params.KeyParameter { *; }
-dontwarn org.bouncycastle.**
-dontwarn javax.naming.**

# Keep the line numbers, or a stack trace from a user is unreadable and there is no mapping
# file shipped with a direct-download APK.
-keepattributes SourceFile,LineNumberTable,Signature,*Annotation*

# JNA is a desktop-first library and its Native class references java.awt for window handles.
# Android has no java.awt at all, and that code path is unreachable here. Warn-only, so R8
# still strips it rather than failing the build over classes that will never be called.
-dontwarn java.awt.**
-dontwarn java.beans.**
