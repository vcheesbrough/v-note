package link.desync.vnote.telemetry

import android.util.Log

// Levelled logging for the app (#406): every line goes to logcat, as before, and
// to OTLP as a log record correlated with [context] — the span the line is
// about, or the current screen.
//
// Levels, as the server and the SPA use them (#354):
// - `e` — the app is broken for the user and will not recover by itself;
// - `w` — something failed that the user may notice, or that should not happen;
// - `i` — a state change worth seeing in a timeline;
// - `d` — detail for diagnosing one of the above.
//
// **Never log user content** — see [Telemetry.log]. A throwable contributes its
// type and message only; the stack trace is reserved for crashes.
object AppLog {
    fun d(
        tag: String,
        message: String,
        context: SpanContext? = null,
        attributes: List<Attribute> = emptyList(),
    ) {
        Log.d(tag, message)
        export(Severity.Debug, message, context, tagged(tag, null, attributes))
    }

    fun i(
        tag: String,
        message: String,
        context: SpanContext? = null,
        attributes: List<Attribute> = emptyList(),
    ) {
        Log.i(tag, message)
        export(Severity.Info, message, context, tagged(tag, null, attributes))
    }

    fun w(
        tag: String,
        message: String,
        throwable: Throwable? = null,
        context: SpanContext? = null,
        attributes: List<Attribute> = emptyList(),
    ) {
        Log.w(tag, message, throwable)
        export(Severity.Warn, message, context, tagged(tag, throwable, attributes))
    }

    fun e(
        tag: String,
        message: String,
        throwable: Throwable? = null,
        context: SpanContext? = null,
        attributes: List<Attribute> = emptyList(),
    ) {
        Log.e(tag, message, throwable)
        export(Severity.Error, message, context, tagged(tag, throwable, attributes))
    }

    private fun tagged(
        tag: String,
        throwable: Throwable?,
        attributes: List<Attribute>,
    ): List<Attribute> = listOf(Attribute("android.log.tag", tag)) + throwableAttributes(throwable) + attributes

    private fun export(
        severity: Severity,
        message: String,
        context: SpanContext?,
        attributes: List<Attribute>,
    ) = Telemetry.log(severity, message, context ?: Telemetry.screen(), attributes)
}

// OpenTelemetry's `exception.*` attributes. The stack trace only when asked
// for: a crash, where it is the whole point.
internal fun throwableAttributes(
    throwable: Throwable?,
    withStackTrace: Boolean = false,
): List<Attribute> {
    throwable ?: return emptyList()
    return buildList {
        add(Attribute("exception.type", throwable.javaClass.name))
        throwable.message?.let { add(Attribute("exception.message", it)) }
        if (withStackTrace) add(Attribute("exception.stacktrace", throwable.stackTraceToString()))
    }
}
