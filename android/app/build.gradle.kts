plugins {
    alias(libs.plugins.android.application)
}

android {
    namespace = "com.nimmesh.app"
    compileSdk = 36

    defaultConfig {
        // Same identity as the iOS build. A bundle/application id is an identity, not a
        // label: changing it orphans every existing install and its stored wallet.
        applicationId = "com.nimmesh.app"
        minSdk = 31
        targetSdk = 36
        // Both are driven from the workspace Cargo.toml by android/scripts/build-apk.sh, so
        // the APK and the Rust core it embeds always report the same version. These are the
        // fallbacks for a plain `./gradlew assembleDebug`.
        //
        // ⚠ The fallback code is 1 on purpose, so a dev build is obviously a dev build. The
        // cost is that a release build (minutes since 2020) cannot be replaced by a debug
        // one: the install fails with INSTALL_FAILED_VERSION_DOWNGRADE. Uninstall first.
        // build-apk.sh --verify uninstalls when it finishes for exactly this reason.
        versionCode = (project.findProperty("nimmeshVersionCode") as String?)?.toInt() ?: 1
        versionName = (project.findProperty("nimmeshVersionName") as String?) ?: "0.0.0-dev"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    signingConfigs {
        // Release signing is owner-gated (ADR-0002). The keystore never enters the repo and
        // is read from the environment, so an unsigned release build is the normal outcome
        // on any machine that does not hold it. See android/scripts/build-apk.sh.
        create("release") {
            val storePath = System.getenv("NIMMESH_KEYSTORE")
            if (storePath != null && file(storePath).exists()) {
                storeFile = file(storePath)
                storePassword = System.getenv("NIMMESH_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("NIMMESH_KEY_ALIAS") ?: "nimmesh"
                keyPassword = System.getenv("NIMMESH_KEY_PASSWORD")
                    ?: System.getenv("NIMMESH_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // ON, but only because it is VERIFIED rather than assumed. A mangled FFI
            // surface fails at RUNTIME on a user's phone, not at build time, and this app
            // ships with no crash reporting behind it. proguard-rules.pro keeps the UniFFI
            // bindings, JNA's reflective access and the JavaScript bridge, and the minified
            // APK is installed and run through the wallet self-test before release.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName("release")
                .takeIf { it.storeFile != null }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

// webui/ is the SAME web layer the iOS app bundles, and it contains no platform code:
// zero references to window.webkit, and the page reaches native only through
// window.nimmesh. Sync it into the default assets dir (gitignored) rather than pointing
// an assets.srcDir outside the module, which would make the build non-relocatable.
val syncWebui by tasks.registering(Sync::class) {
    from(rootProject.layout.projectDirectory.dir("../webui"))
    into(layout.projectDirectory.dir("src/main/assets/webui"))
}

// The official BIP39 English wordlist, taken from the iOS app's resources rather than
// copied. Two copies of a wordlist is a wallet that derives a DIFFERENT ADDRESS on one
// platform if a single word ever diverges, and nothing would flag it until someone's
// funds landed somewhere they could not reach.
val syncWordlist by tasks.registering(Copy::class) {
    from(rootProject.layout.projectDirectory.file("../apple/NimmeshApp/Resources/bip39-english.txt"))
    into(layout.projectDirectory.dir("src/main/assets"))
}

tasks.named("preBuild") { dependsOn(syncWebui, syncWordlist) }

dependencies {
    implementation(project(":core"))
    implementation(libs.bouncycastle)
    implementation(libs.camera.core)
    implementation(libs.camera.camera2)
    implementation(libs.camera.lifecycle)
    implementation(libs.camera.view)
    implementation(libs.zxing.core)
    testImplementation(libs.junit)
    // A REAL org.json for JVM tests. Android stubs org.json in unit tests and every method
    // throws "not mocked", so without this the JSON-shaping tests would not run at all.
    testImplementation(libs.json)
    implementation(libs.androidx.webkit)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.runner)
}
