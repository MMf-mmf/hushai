plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("com.squareup.wire")
}

android {
    namespace = "com.hushai.android"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.hushai.android"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
            // Cleartext-to-LAN network_security_config + headless Intent-extra
            // config live in src/debug only (see src/debug/).
        }
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
}

// Compile the IDENTICAL hushai.v1.SegmentManifest the backend uses (prost) from the
// single shared .proto on disk — the contract §8 anti-drift guarantee. Square Wire
// emits Kotlin; `bytes` -> okio.ByteString (so segment_id/session_id are 16 raw bytes),
// uint64/fixed64 -> Long.
wire {
    kotlin {}
    sourcePath {
        srcDir("../../hushai-backend/proto")
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.09.03"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.6")
    implementation("androidx.lifecycle:lifecycle-service:2.8.6")
    implementation("androidx.datastore:datastore-preferences:1.1.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("com.squareup.wire:wire-runtime:4.9.9")
    // Offline on-device speech: wake-word + question STT + speaker x-vectors (spk model).
    // Ships native .so (arm64/armv7/x86) + JNA; no cloud, consistent with the local stack.
    implementation("com.alphacephei:vosk-android:0.3.47")

    debugImplementation("androidx.compose.ui:ui-tooling")

    testImplementation("junit:junit:4.13.2")
    testImplementation("com.squareup.okhttp3:mockwebserver:4.12.0")
    // Real org.json impl for JVM unit tests (Android's bundled org.json is a
    // throwing stub under unit tests — "Method ... not mocked").
    testImplementation("org.json:json:20240303")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test:rules:1.6.1")
}
