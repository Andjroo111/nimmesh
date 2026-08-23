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
tasks.named("preBuild") { dependsOn(syncWebui) }

dependencies {
    implementation(project(":core"))
    testImplementation(libs.junit)
    implementation(libs.androidx.webkit)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.runner)
}
