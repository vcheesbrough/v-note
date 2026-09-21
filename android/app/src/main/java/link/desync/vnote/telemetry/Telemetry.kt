package link.desync.vnote.telemetry

import java.time.Instant
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

// Client telemetry for the Android app (#406): spans and levelled logs, exported
// as OTLP/JSON to `{BASE_URL}/otlp/android` with the app's bearer token. The
// Android half of #354, over the same ingress and sidecar, and deliberately the
// same shape as the SPA's `frontend/src/telemetry.rs`.
//
// Three rules hold everywhere in this package, because telemetry that costs the
// product anything is worse than no telemetry:
//
// 1. **Nothing here can fail loudly.** Every entry point swallows its errors.
// 2. **Nothing here blocks the caller.** Exports run on one background thread;
//    the crash handler is the only caller that waits, and only briefly.
// 3. **Nothing here is unbounded.** Queues are capped and the send rate is
//    capped, whatever the app does.
//
// No Android API is used in this file, so screens and sessions that call it stay
// unit-testable on the JVM: before [Telemetry.install] every call is a cheap
// no-op that still hands out valid span contexts.

// How long finished telemetry waits before it is sent (#406 battery budget).
// Longer than the SPA's 5 s on purpose: a cellular radio stays in its
// high-power state for several seconds after each transfer, so a 5 s cadence
// would keep it there for as long as the user is inking. At 30 s an active
// session makes at most two small requests per window, and an idle one none.
internal const val EXPORT_INTERVAL_MS = 30_000L

// How long the crash handler waits for its last export before letting the
// process die. Long enough for one request on a working network; short enough
// that a crash with no network is not noticeably slower to report to the user.
internal const val CRASH_FLUSH_TIMEOUT_MS = 2_000L

private const val NANOS_PER_SECOND = 1_000_000_000L
private const val NANOS_PER_MILLI = 1_000_000L

// Which OTLP signal a request carries; the last segment of its path.
enum class Signal(
    val path: String,
) {
    Traces("traces"),
    Logs("logs"),
}

// Where a batch goes. [OtlpHttpTransport] in the app; a fake in tests.
internal interface Transport {
    // Whether an export could be authorised now. The ingress needs the bearer
    // token, so a signed-out app can only get a 401 — cheaper to know first.
    fun isReady(): Boolean

    fun send(
        signal: Signal,
        body: String,
    ): ExportOutcome
}

// One process's telemetry: the current screen, span and log collection, and
// the export loop. [transport] null means export is off (the `devLocal`
// flavor, and every unit test): spans and logs are accepted and discarded.
class TelemetryRuntime internal constructor(
    transport: Transport?,
    private val clock: () -> Long = ::nowUnixNanos,
) {
    private val queue = transport?.let { ExportQueue(it, clock) }
    private var scheduler: ScheduledExecutorService? = null

    // The span enclosing everything the current screen does. Read only by
    // [screen]; nothing else consults "the current" anything.
    @Volatile
    private var screenRoot = SpanContext(TraceId.random(), SpanId.random())

    val isExporting: Boolean get() = queue?.isSwitchedOff == false

    fun now(): Long = clock()

    // Starts the export loop. Idempotent.
    @Synchronized
    fun start(intervalMs: Long = EXPORT_INTERVAL_MS) {
        val queue = queue ?: return
        if (scheduler != null) return
        scheduler =
            Executors
                .newSingleThreadScheduledExecutor { runnable ->
                    Thread(runnable, "vnote-telemetry").apply { isDaemon = true }
                }.also { executor ->
                    executor.scheduleWithFixedDelay(queue::exportOnce, intervalMs, intervalMs, TimeUnit.MILLISECONDS)
                }
    }

    fun screen(): SpanContext = screenRoot

    // Starts a new trace rooted at a zero-length span named [name], and makes
    // it the current screen. One trace per screen rather than per process: an
    // app left open all day would otherwise build one unboundedly deep trace.
    fun startScreen(
        name: String,
        attributes: List<Attribute> = emptyList(),
    ): SpanContext {
        val root = SpanContext(TraceId.random(), SpanId.random())
        val start = now()
        screenRoot = root
        record(FinishedSpan(root, null, name, SpanKind.Internal, start, start, attributes))
        return root
    }

    fun span(
        name: String,
        parent: SpanContext,
        kind: SpanKind = SpanKind.Internal,
        startUnixNanos: Long = now(),
    ): OpenSpan = OpenSpan(this, SpanContext(parent.traceId, SpanId.random()), parent.spanId, name, kind, startUnixNanos)

    // **Never pass user content.** Page titles and stroke data are the user's;
    // ids, counts, statuses and our own error strings are not. This goes to a
    // server the user does not control.
    fun log(
        severity: Severity,
        message: String,
        context: SpanContext? = screen(),
        attributes: List<Attribute> = emptyList(),
    ) {
        queue?.push(LogRecord(now(), severity, message, attributes, context))
    }

    internal fun record(span: FinishedSpan) {
        queue?.push(span)
    }

    // Exports now, on the export thread. For the app going to the background:
    // the radio is likely still up from whatever the user just did.
    fun flush() {
        val queue = queue ?: return
        runCatching { scheduler?.execute(queue::exportOnce) }
    }

    // Exports now and waits up to [timeoutMs]. Only for the crash handler — the
    // process is about to die, and anything still queued dies with it. On its
    // own thread because the crashing thread may be the main thread, where
    // Android refuses network I/O outright.
    fun flushBlocking(timeoutMs: Long = CRASH_FLUSH_TIMEOUT_MS) {
        val queue = queue ?: return
        runCatching {
            val thread = Thread(queue::exportOnce, "vnote-telemetry-flush").apply { isDaemon = true }
            thread.start()
            thread.join(timeoutMs)
        }
    }

    // One export attempt, on the calling thread. For tests.
    internal fun exportNow() {
        queue?.exportOnce()
    }
}

// The two outboxes, the send policy, and one export attempt at a time.
private class ExportQueue(
    private val transport: Transport,
    private val clock: () -> Long,
) {
    private val spans = Outbox<FinishedSpan>()
    private val logs = Outbox<LogRecord>()
    private val policy = ExportPolicy()
    private var reportedDrops = 0L

    val isSwitchedOff: Boolean get() = policy.isSwitchedOff

    fun push(span: FinishedSpan) {
        if (!isSwitchedOff) spans.push(span)
    }

    fun push(record: LogRecord) {
        if (!isSwitchedOff) logs.push(record)
    }

    // One export attempt: traces, then logs. Never throws.
    @Synchronized
    fun exportOnce() {
        runCatching {
            if (!policy.shouldExport() || !transport.isReady()) return
            reportDrops()
            for (signal in Signal.entries) {
                val body = takeBody(signal) ?: continue
                val outcome = transport.send(signal, body)
                policy.record(outcome)
                if (outcome == ExportOutcome.SwitchedOff) {
                    spans.clear()
                    logs.clear()
                }
                if (outcome == ExportOutcome.SwitchedOff || outcome == ExportOutcome.Unavailable) return
            }
        }
    }

    private fun takeBody(signal: Signal): String? =
        when (signal) {
            Signal.Traces -> spans.takeBatch().takeIf { it.isNotEmpty() }?.let(::tracesRequest)
            Signal.Logs -> logs.takeBatch().takeIf { it.isNotEmpty() }?.let(::logsRequest)
        }

    // Items dropped for lack of room are reported once per new loss, as a log
    // line of their own — the queue says what it lost rather than losing it
    // silently.
    private fun reportDrops() {
        val dropped = spans.dropped + logs.dropped
        if (dropped > reportedDrops) {
            logs.push(
                LogRecord(
                    clock(),
                    Severity.Warn,
                    "telemetry outbox full; items dropped",
                    listOf(Attribute("vnote.telemetry.dropped", dropped - reportedDrops)),
                ),
            )
            reportedDrops = dropped
        }
    }
}

// An open span. Finishing it is [end] or [fail]; an abandoned span records
// nothing, which is deliberate — work that never finished never happened as far
// as the trace is concerned. Its trace and parent are fixed when it opens.
class OpenSpan internal constructor(
    private val runtime: TelemetryRuntime,
    val context: SpanContext,
    private val parentSpanId: SpanId,
    private val name: String,
    private val kind: SpanKind,
    private val startUnixNanos: Long,
) {
    private val attributes = mutableListOf<Attribute>()
    private val finished = AtomicBoolean(false)

    fun attr(
        key: String,
        value: Any,
    ): OpenSpan {
        synchronized(attributes) { attributes.add(Attribute(key, value)) }
        return this
    }

    fun end(endUnixNanos: Long = runtime.now()) = finish(endUnixNanos, null)

    fun fail(
        message: String,
        endUnixNanos: Long = runtime.now(),
    ) = finish(endUnixNanos, message)

    // Idempotent: the first finish wins, so an echo racing a disconnect cannot
    // record the same span twice.
    private fun finish(
        endUnixNanos: Long,
        error: String?,
    ) {
        if (!finished.compareAndSet(false, true)) return
        val snapshot = synchronized(attributes) { attributes.toList() }
        runtime.record(
            FinishedSpan(context, parentSpanId, name, kind, startUnixNanos, endUnixNanos, snapshot, error),
        )
    }
}

// The process-wide runtime the app's code calls. Replaced once, at startup, by
// [install]; until then (and in unit tests) it collects nothing.
object Telemetry {
    @Volatile
    var runtime: TelemetryRuntime = TelemetryRuntime(transport = null)
        private set

    fun install(installed: TelemetryRuntime) {
        runtime = installed
        installed.start()
    }

    val isExporting: Boolean get() = runtime.isExporting

    fun now(): Long = runtime.now()

    fun screen(): SpanContext = runtime.screen()

    fun startScreen(
        name: String,
        attributes: List<Attribute> = emptyList(),
    ): SpanContext = runtime.startScreen(name, attributes)

    fun span(
        name: String,
        parent: SpanContext = screen(),
        kind: SpanKind = SpanKind.Internal,
        startUnixNanos: Long = now(),
    ): OpenSpan = runtime.span(name, parent, kind, startUnixNanos)

    fun log(
        severity: Severity,
        message: String,
        context: SpanContext? = screen(),
        attributes: List<Attribute> = emptyList(),
    ) = runtime.log(severity, message, context, attributes)

    fun flush() = runtime.flush()

    fun flushBlocking(timeoutMs: Long = CRASH_FLUSH_TIMEOUT_MS) = runtime.flushBlocking(timeoutMs)
}

// Parses a `traceparent` header back into the span it names, so a log about a
// request can be correlated with that request's span. Null for anything that is
// not a well-formed version-00 header.
fun parseTraceparent(header: String?): SpanContext? {
    val match = header?.let(TRACEPARENT::matchEntire) ?: return null
    val (trace, span) = match.destructured
    if (trace.all { it == '0' } || span.all { it == '0' }) return null
    return SpanContext(TraceId(trace), SpanId(span))
}

private val TRACEPARENT = Regex("00-([0-9a-f]{32})-([0-9a-f]{16})-[0-9a-f]{2}")

internal fun nowUnixNanos(): Long {
    val now = Instant.now()
    return now.epochSecond * NANOS_PER_SECOND + now.nano
}

internal fun millisToNanos(millis: Long): Long = millis * NANOS_PER_MILLI
