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
        versionCode = 1
        versionName = "0.89.7"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
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
