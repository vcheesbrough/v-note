package link.desync.vnote.telemetry

// Routes an uncaught exception to OTLP as an `error` log, correlated with the
// screen it happened on, then hands it to whatever handler was installed before
// — the platform's, which writes logcat and shows the crash dialog. The process
// is dying, so the record is sent at once and by itself, bounded by
// [CRASH_FLUSH_TIMEOUT_MS] — see [TelemetryRuntime.reportCrash].
//
// The full exception message and stack trace are recorded. That is the one
// deliberate exception to "never user content", as with the SPA's panic hook
// (#354): a crash is the rarest and most diagnostic event the app reports.
internal class CrashHandler(
    private val runtime: () -> TelemetryRuntime,
    private val previous: Thread.UncaughtExceptionHandler?,
) : Thread.UncaughtExceptionHandler {
    override fun uncaughtException(
        thread: Thread,
        throwable: Throwable,
    ) {
        runCatching {
            val telemetry = runtime()
            telemetry.reportCrash(
                LogRecord(
                    telemetry.now(),
                    Severity.Error,
                    "uncaught exception",
                    listOf(Attribute("thread.name", thread.name)) +
                        throwableAttributes(throwable, withStackTrace = true),
                    telemetry.screen(),
                ),
            )
        }
        previous?.uncaughtException(thread, throwable)
    }

    companion object {
        fun install() {
            val previous = Thread.getDefaultUncaughtExceptionHandler()
            if (previous is CrashHandler) return
            Thread.setDefaultUncaughtExceptionHandler(CrashHandler({ Telemetry.runtime }, previous))
        }
    }
}
