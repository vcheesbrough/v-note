package link.desync.vnote

import android.app.Activity
import android.app.Application
import android.os.Bundle
import android.os.Process
import android.os.SystemClock
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
// Export is per flavor (`TELEMETRY_EXPORT`): on for `dev`, which exports to the
// dev server it talks to; off for `devLocal`, whose server is a laptop and must
// never feed the dev environment's Tempo and Loki.
class VNoteApplication : Application() {
    // Process start → first activity resumed: cold start as the user sees it.
    private var launch: OpenSpan? = null

    override fun onCreate() {
        super.onCreate()
        // First, so a failure in anything below is itself reported.
        CrashHandler.install()
        if (BuildConfig.TELEMETRY_EXPORT) {
            // Built on first use — on the export thread, not here: it is
            // Keystore work that would otherwise sit on the main thread inside
            // the `app.start` span. A keystore that cannot be opened means "not
            // signed in" to the exporter, never a crash at startup.
            val tokenStore by lazy { runCatching { TokenStore(this) }.getOrNull() }
            Telemetry.install(
                TelemetryRuntime(
                    OtlpHttpTransport(
                        BuildConfig.BASE_URL,
                        accessToken = { tokenStore?.accessToken() },
                        accessTokenExpiry = { tokenStore?.accessTokenExpiryEpochSeconds() },
                    ),
                ),
            )
        }
        val root = Telemetry.startScreen("app.launch")
        launch =
            Telemetry
                .span("app.start", root, startUnixNanos = processStartUnixNanos())
                .attr("app.start.type", "cold")
        registerActivityLifecycleCallbacks(LifecycleTelemetry())
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
