package link.desync.vnote

import android.app.Activity
import android.app.Application
import android.os.Bundle
import android.os.Process
import android.os.SystemClock
import android.util.Log
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.telemetry.CrashHandler
import link.desync.vnote.telemetry.OpenSpan
import link.desync.vnote.telemetry.OtlpHttpTransport
import link.desync.vnote.telemetry.Telemetry
import link.desync.vnote.telemetry.TelemetryRuntime
import link.desync.vnote.telemetry.millisToNanos

// Process-wide setup (#406): client telemetry and the crash handler, installed
// before any activity exists so the first screen is covered.
//
// Whether telemetry is exported is not decided here (#439): the runtime asks the
// server it talks to (`GET /api/telemetry/config`) once there is a session, and
// a server with no ingest configured — a laptop behind `devLocal`, or an
// environment that has switched it off — answers "off".
class VNoteApplication : Application() {
    // Process start → first activity resumed: cold start as the user sees it.
    private var launch: OpenSpan? = null

    override fun onCreate() {
        super.onCreate()
        // First, so a failure in anything below is itself reported.
        CrashHandler.install()
        // Built on first use — on the export thread, not here: it is Keystore
        // work that would otherwise sit on the main thread inside the
        // `app.start` span. A keystore that cannot be opened means "not signed
        // in" to the exporter, never a crash at startup.
        val tokenStore by lazy { runCatching { TokenStore(this) }.getOrNull() }
        Telemetry.install(
            TelemetryRuntime(
                OtlpHttpTransport(
                    BuildConfig.BASE_URL,
                    accessToken = { tokenStore?.accessToken() },
                    accessTokenExpiry = { tokenStore?.accessTokenExpiryEpochSeconds() },
                ),
                serviceVersion = BuildConfig.VERSION_NAME,
                // State changes only — first failure, recovery, giving up —
                // never a line per batch, and never the token.
                localLog = { message -> Log.w(TELEMETRY_TAG, message) },
            ),
        )
        val root = Telemetry.startScreen("app.launch")
        launch =
            Telemetry
                .span("app.start", root, startUnixNanos = processStartUnixNanos())
                .attr("app.start.type", "cold")
        registerActivityLifecycleCallbacks(LifecycleTelemetry())
    }

    private companion object {
        const val TELEMETRY_TAG = "VNoteTelemetry"
    }

    private fun processStartUnixNanos(): Long {
        val sinceStartMillis = SystemClock.elapsedRealtime() - Process.getStartElapsedRealtime()
        return Telemetry.now() - millisToNanos(sinceStartMillis)
    }

    private inner class LifecycleTelemetry : ActivityLifecycleCallbacks {
        override fun onActivityResumed(activity: Activity) {
            launch?.end()
            launch = null
        }

        // Leaving the foreground: send what is queued while the radio is still
        // up from whatever the user just did, rather than waking it later — or
        // losing the batch if the process is reclaimed in the background.
        override fun onActivityStopped(activity: Activity) = Telemetry.flush()

        override fun onActivityCreated(
            activity: Activity,
            savedInstanceState: Bundle?,
        ) = Unit

        override fun onActivityStarted(activity: Activity) = Unit

        override fun onActivityPaused(activity: Activity) = Unit

        override fun onActivitySaveInstanceState(
            activity: Activity,
            outState: Bundle,
        ) = Unit

        override fun onActivityDestroyed(activity: Activity) = Unit
    }
}
