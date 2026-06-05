import java.io.File

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jlleitschuh.gradle.ktlint")
}

val repoRoot = rootProject.projectDir.parentFile
val versionFile = File(repoRoot, "version.txt")
val appVersionName =
    if (versionFile.exists()) {
        versionFile.readText().trim()
    } else {
        "0.1.0"
    }

android {
    namespace = "link.desync.vnote"
    compileSdk = 35

    defaultConfig {
        applicationId = "link.desync.vnote"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = appVersionName
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    flavorDimensions += "env"
    productFlavors {
        create("dev") {
            dimension = "env"
            applicationIdSuffix = ".dev"
            // Loopback + `adb reverse tcp:8080 tcp:8080` — works on emulator and USB devices.
            buildConfigField("String", "BASE_URL", "\"http://127.0.0.1:8080\"")
        }
        create("prod") {
            dimension = "env"
            buildConfigField("String", "BASE_URL", "\"https://v-notes.desync.link\"")
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
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

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    androidTestImplementation(composeBom)

    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")

    testImplementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    testImplementation("org.json:json:20240303")

    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.6.1")
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")

    debugImplementation("androidx.compose.ui:ui-tooling")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}

tasks.register("syncVersionFromWorkspace") {
    doLast {
        val script = File(repoRoot, "scripts/sync-version.sh")
        if (!script.exists()) {
            throw GradleException("Missing ${script.absolutePath}")
        }
        project.exec {
            workingDir = repoRoot
            commandLine("bash", script.absolutePath)
        }
    }
}

tasks.matching { it.name.startsWith("pre") && it.name.contains("Build") }.configureEach {
    dependsOn("syncVersionFromWorkspace")
}
