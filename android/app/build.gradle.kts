import java.io.File
import javax.inject.Inject

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jlleitschuh.gradle.ktlint")
    id("io.gitlab.arturbosch.detekt")
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
    compileSdk = 36

    defaultConfig {
        applicationId = "link.desync.vnote"
        minSdk = 26
        targetSdk = 36
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

    buildFeatures {
        compose = true
        buildConfig = true
    }

    lint {
        // Gate, not a report: CI runs `:app:lintDevDebug` in the Dockerfile.android
        // builder stage (#337). Existing findings are frozen in the baseline; anything
        // new — warning or error — fails the build.
        abortOnError = true
        warningsAsErrors = true
        baseline = file("lint-baseline.xml")
        // Time-dependent checks: they start firing when a newer dependency, AGP or API
        // level is published, which would turn an unrelated push red. Dependency
        // upgrades are their own iterations (#330-style), not a lint outcome.
        disable +=
            listOf(
                "GradleDependency",
                "NewerVersionAvailable",
                "AndroidGradlePluginVersion",
                "OldTargetApi",
            )
    }
}

detekt {
    // Complexity ratchet (LongMethod, LargeClass, ComplexCondition, TooManyFunctions).
    // Defaults plus `../detekt.yml`; existing findings are frozen in the baseline so
    // only new ones fail. #337 slice 9 shrinks the baseline as functions are split.
    buildUponDefaultConfig = true
    config.setFrom(rootProject.file("detekt.yml"))
    baseline = file("detekt-baseline.xml")
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2026.06.01")
    implementation(composeBom)
    androidTestImplementation(composeBom)

    implementation("androidx.core:core-ktx:1.18.0")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("com.squareup.okhttp3:okhttp:5.4.0")
    implementation("net.openid:appauth:0.11.1")
    implementation("androidx.browser:browser:1.10.0")
    implementation("androidx.security:security-crypto:1.1.0")
    // Named trace sections around the ink draw layers, so a Perfetto capture on
    // a real tablet shows which layer a frame was spent in. See card #312.
    implementation("androidx.tracing:tracing-ktx:2.0.2")

    testImplementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.11.0")
    testImplementation("org.json:json:20260814")

    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.7.0")
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    androidTestImplementation("com.squareup.okhttp3:mockwebserver3:5.4.0")

    debugImplementation("androidx.compose.ui:ui-tooling")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}

// Gradle 9 removed Project.exec, and this task sits on the path of every build
// (every pre*Build task depends on it). ExecOperations is the supported
// replacement: it is injected into a real task class, runs at execution time
// like the old doLast did, and is configuration-cache clean.
abstract class SyncVersionFromWorkspace : DefaultTask() {
    @get:Inject
    abstract val execOps: ExecOperations

    @get:Internal
    abstract val workspaceRoot: DirectoryProperty

    @TaskAction
    fun sync() {
        val root = workspaceRoot.get().asFile
        val script = File(root, "scripts/sync-version.sh")
        if (!script.exists()) {
            throw GradleException("Missing ${script.absolutePath}")
        }
        execOps.exec {
            workingDir = root
            commandLine("bash", script.absolutePath)
        }
    }
}

tasks.register<SyncVersionFromWorkspace>("syncVersionFromWorkspace") {
    workspaceRoot.set(repoRoot)
}

tasks.matching { it.name.startsWith("pre") && it.name.contains("Build") }.configureEach {
    dependsOn("syncVersionFromWorkspace")
}
