import java.io.File

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jlleitschuh.gradle.ktlint")
}

val repoRoot = rootProject.projectDir.parentFile
val versionFile = File(repoRoot, "version.txt")
// CI injects the computed release tag (e.g. 0.4.1) via V_NOTE_RELEASE so versionName
// matches the deployed image; local builds fall back to the cargo version in version.txt.
val injectedRelease = System.getenv("V_NOTE_RELEASE")?.trim()?.takeIf { it.isNotEmpty() }
val appVersionName =
    injectedRelease
        ?: if (versionFile.exists()) {
            versionFile.readText().trim()
        } else {
            "0.1.0"
        }

// Monotonic versionCode from major.minor.patch so in-place upgrades are accepted
// (e.g. 0.4.1 -> 4001). Pre-release suffixes are ignored for the code.
val appVersionCode =
    appVersionName.substringBefore('-').split('.').let { parts ->
        val major = parts.getOrNull(0)?.toIntOrNull() ?: 0
        val minor = parts.getOrNull(1)?.toIntOrNull() ?: 0
        val patch = parts.getOrNull(2)?.toIntOrNull() ?: 0
        (major * 1_000_000 + minor * 1_000 + patch).coerceAtLeast(1)
    }

android {
    namespace = "link.desync.vnote"
    compileSdk = 35

    defaultConfig {
        applicationId = "link.desync.vnote"
        minSdk = 26
        targetSdk = 35
        versionCode = appVersionCode
        versionName = appVersionName
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // Required by net.openid.appauth manifest merger (HTTPS App Links use a separate intent filter).
        manifestPlaceholders["appAuthRedirectScheme"] = "link.desync.vnote"
    }

    flavorDimensions += "env"
    productFlavors {
        create("dev") {
            dimension = "env"
            applicationIdSuffix = ".dev"
            manifestPlaceholders["appLinkHost"] = "v-notes-dev.desync.link"
            buildConfigField("String", "BASE_URL", "\"https://v-notes-dev.desync.link\"")
            buildConfigField(
                "String",
                "OIDC_ISSUER_URL",
                "\"https://auth.desync.link/application/o/v-note-android-dev/\"",
            )
            buildConfigField("String", "OIDC_CLIENT_ID", "\"v-note-android-dev\"")
            buildConfigField(
                "String",
                "OIDC_REDIRECT_URI",
                "\"https://v-notes-dev.desync.link/auth/mobile/callback\"",
            )
            buildConfigField(
                "String",
                "OIDC_END_SESSION_URL",
                "\"https://auth.desync.link/application/o/v-note-android-dev/end-session/\"",
            )
            buildConfigField(
                "String",
                "OIDC_SCOPES",
                "\"openid profile email offline_access v-note:dev:access\"",
            )
        }
        create("devLocal") {
            dimension = "env"
            applicationIdSuffix = ".dev"
            manifestPlaceholders["appLinkHost"] = "v-notes-dev.desync.link"
            // Loopback + `adb reverse tcp:8080 tcp:8080` — for laptop dev without a deployed stack.
            buildConfigField("String", "BASE_URL", "\"http://127.0.0.1:8080\"")
            buildConfigField(
                "String",
                "OIDC_ISSUER_URL",
                "\"https://auth.desync.link/application/o/v-note-android-dev/\"",
            )
            buildConfigField("String", "OIDC_CLIENT_ID", "\"v-note-android-dev\"")
            buildConfigField(
                "String",
                "OIDC_REDIRECT_URI",
                "\"https://v-notes-dev.desync.link/auth/mobile/callback\"",
            )
            buildConfigField(
                "String",
                "OIDC_END_SESSION_URL",
                "\"https://auth.desync.link/application/o/v-note-android-dev/end-session/\"",
            )
            buildConfigField(
                "String",
                "OIDC_SCOPES",
                "\"openid profile email offline_access v-note:dev:access\"",
            )
        }
        create("prod") {
            dimension = "env"
            manifestPlaceholders["appLinkHost"] = "v-notes.desync.link"
            buildConfigField("String", "BASE_URL", "\"https://v-notes.desync.link\"")
            buildConfigField(
                "String",
                "OIDC_ISSUER_URL",
                "\"https://auth.desync.link/application/o/v-note-android-prod/\"",
            )
            buildConfigField("String", "OIDC_CLIENT_ID", "\"v-note-android-prod\"")
            buildConfigField(
                "String",
                "OIDC_REDIRECT_URI",
                "\"https://v-notes.desync.link/auth/mobile/callback\"",
            )
            buildConfigField(
                "String",
                "OIDC_END_SESSION_URL",
                "\"https://auth.desync.link/application/o/v-note-android-prod/end-session/\"",
            )
            buildConfigField(
                "String",
                "OIDC_SCOPES",
                "\"openid profile email offline_access v-note:prod:access\"",
            )
        }
    }

    signingConfigs {
        // Shared, non-secret debug keystore committed at android/app/debug.keystore so every
        // build (CI, docker, Android Studio) signs with the same certificate. Required for
        // in-place upgrades and a stable App Links fingerprint. A secret release keystore
        // replaces this before any prod release (see backlog).
        getByName("debug") {
            storeFile = file("debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
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
    implementation("androidx.compose.material:material-icons-extended")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("net.openid:appauth:0.11.1")
    implementation("androidx.browser:browser:1.8.0")
    implementation("androidx.security:security-crypto:1.1.0-alpha06")

    testImplementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    testImplementation("org.json:json:20240303")

    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.6.1")
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    androidTestImplementation("com.squareup.okhttp3:mockwebserver:4.12.0")

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
