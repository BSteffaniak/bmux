plugins {
    id("com.android.application") version "9.1.0"
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.0"
}

android {
    namespace = "io.bmux.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.bmux.android"
        minSdk = 29
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0-alpha"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        debug {
            buildConfigField("boolean", "ALPHA_TELEMETRY_ENABLED", "true")
        }

        create("alpha") {
            initWith(getByName("debug"))
            applicationIdSuffix = ".alpha"
            versionNameSuffix = "-internal"
            matchingFallbacks += listOf("debug")
            buildConfigField("boolean", "ALPHA_TELEMETRY_ENABLED", "true")
        }

        release {
            isMinifyEnabled = false
            buildConfigField("boolean", "ALPHA_TELEMETRY_ENABLED", "false")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    sourceSets {
        getByName("main").jniLibs.srcDirs(file("../generated/jniLibs"))
    }

}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.03.01"))

    implementation("androidx.activity:activity-compose:1.11.0")
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.9.4")
    implementation("androidx.lifecycle:lifecycle-viewmodel-ktx:2.9.4")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.9.4")
    implementation("androidx.security:security-crypto:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("com.google.android.material:material:1.13.0")
    implementation("org.connectbot:termlib:0.0.27")
    debugImplementation("androidx.compose.ui:ui-tooling")

    implementation("net.java.dev.jna:jna:5.17.0@aar")

    androidTestImplementation("androidx.test:core-ktx:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test:rules:1.7.0")
}
