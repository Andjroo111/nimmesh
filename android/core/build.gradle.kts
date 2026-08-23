plugins {
    alias(libs.plugins.android.library)
}

android {
    namespace = "com.nimmesh.core"
    compileSdk = 36

    defaultConfig {
        // 31, so BLUETOOTH_SCAN can declare `neverForLocation` and the app never asks
        // for location. Below API 31 a BLE scan is impossible without ACCESS_FINE_LOCATION.
        minSdk = 31
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // x86 is dead at minSdk 31; x86_64 is here for the emulator.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }

    // src/main/kotlin and src/main/jniLibs are AGP's defaults, so nothing needs
    // declaring here. Both are produced by android/scripts/build-core.sh and are
    // gitignored: the UniFFI bindings are generated FROM the built .so, so the two
    // can never disagree about the FFI contract version.

    packaging {
        // The .so files are already built for release by cargo-ndk. Let Gradle ship them
        // verbatim rather than running its own strip over a Rust artifact.
        jniLibs.keepDebugSymbols += "**/libnimmesh_core.so"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    api(libs.jna) { artifact { type = "aar" } }
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.runner)
}
