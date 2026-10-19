import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.kiln.mobile"
    compileSdk = 36

    defaultConfig {
        // UnifiedPush connector 3.x needs 21+; the app sets 26 (tauri.conf.json).
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    // UnifiedPush (distributor e.g. ntfy). Maven Central, Apache-2.0.
    implementation("org.unifiedpush.android:connector:3.3.5")
    implementation(project(":tauri-android"))
}
