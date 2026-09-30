package link.desync.vnote.telemetry

import java.time.Instant
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

// Client telemetry for the Android app (#406): spans and levelled logs, exported
// as OTLP/JSON to this environment's `otlp-collector-oidc` ingest with the app's
// bearer token (#439). Deliberately the same shape as the SPA's
// `frontend/src/telemetry.rs`.
//
// **No configuration, no telemetry.** The endpoint is not compiled in: the app
// asks its server (`GET /api/telemetry/config`) once it has a session, and with
// no answer — ingest off, a failed fetch, an older server — OTLP is never
// initialised and whatever waited in the small pre-config buffer is discarded.
//
// Three rules hold everywhere in this package, because telemetry that costs the
// product anything is worse than no telemetry:
//
// 1. **Nothing here can fail loudly.** Every entry point swallows its errors,
//    and failures are said once, to Logcat, on a change of state only.
// 2. **Nothing here blocks the caller.** Exports run on one background thread;
//    the crash handler is the only caller that waits, and only briefly.
// 3. **Nothing here is unbounded.** Queues are capped, retries are capped, and
//    repeated failure stops telemetry for the process.
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

// The network side. [OtlpHttpTransport] in the app; a fake in tests.
internal interface Transport {
    // Whether a request could be authorised now: a token that is present and
    // not about to expire. A signed-out app can only collect 401s.
    fun isReady(): Boolean

    // Identifies the token a request would carry now, without exposing it, so
    // a refresh after a 401 can be recognised. Null when there is none.
    fun credentialId(): Int?

    // `GET {server}/api/telemetry/config` with the bearer.
    fun fetchConfig(): ConfigFetch

    // `POST {endpoint}/v1/{signal}` with the bearer, the token read now.
    fun send(
        endpoint: String,
        signal: Signal,
        body: String,
    ): ExportOutcome
}

// One process's telemetry: the current screen, span and log collection, and
// the export loop. [transport] null means export is off (every unit test that
// does not care): spans and logs are accepted and discarded.
class TelemetryRuntime internal constructor(
    transport: Transport?,
    serviceVersion: String = "unknown",
    // Where the state-change lines go: Logcat in the app, a list in tests.
    localLog: (String) -> Unit = {},
    jitter: () -> Double = { kotlin.random.Random.nextDouble() },
    // Last, so a test's trailing lambda is the clock.
    private val clock: () -> Long = ::nowUnixNanos,
) {
    private val queue = transport?.let { ExportQueue(it, clock, serviceVersion, localLog, jitter) }
    private var scheduler: ScheduledExecutorService? = null

    // The span enclosing everything the current screen does. Read only by
    // [screen]; nothing else consults "the current" anything.
    @Volatile
    private var screenRoot = SpanContext(TraceId.random(), SpanId.random())

    val isExporting: Boolean get() = queue?.isExporting == true

    fun now(): Long = clock()

    // Starts the export loop, fetching configuration first. Idempotent.
    @Synchronized
    fun start(intervalMs: Long = EXPORT_INTERVAL_MS) {
        val queue = queue ?: return
        if (scheduler != null) return
        scheduler =
            Executors
                .newSingleThreadScheduledExecutor { runnable ->
                    Thread(runnable, "vnote-telemetry").apply { isDaemon = true }
                }.also { executor ->
                    executor.execute(queue::tick)
                    executor.scheduleWithFixedDelay(queue::tick, intervalMs, intervalMs, TimeUnit.MILLISECONDS)
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
        record(FinishedSpan(root, null, name, SpanKind.Internal, start, start, attributes + located))
        return root
    }

    fun span(
        name: String,
        parent: SpanContext,
        kind: SpanKind = SpanKind.Internal,
        startUnixNanos: Long = now(),
    ): OpenSpan {
        val span = OpenSpan(this, SpanContext(parent.traceId, SpanId.random()), parent.spanId, name, kind, startUnixNanos)
        // First, so a caller's own attributes follow.
        located.forEach { span.attr(it.key, it.value) }
        return span
    }

    // **Never pass user content.** Page titles and stroke data are the user's;
    // ids, counts, statuses and our own error strings are not. This goes to a
    // server the user does not control.
    fun log(
        severity: Severity,
        message: String,
        context: SpanContext? = screen(),
        attributes: List<Attribute> = emptyList(),
    ) {
        if (queue == null) return
        queue.push(LogRecord(now(), severity, message, attributes + located, context))
    }

    // The caller's source location and thread (#453), taken — a getter, read
    // afresh each time — where a span opens or a line is written. Skipped with
    // export off, so the no-op runtime every unit test and pre-install call
    // uses stays free.
    private val located: List<Attribute>
        get() = if (queue == null) emptyList() else callSite()

    internal fun record(span: FinishedSpan) {
        queue?.push(span)
    }

    // Exports now, on the export thread. For the app going to the background:
    // the radio is likely still up from whatever the user just did.
    fun flush() {
        val queue = queue ?: return
        runCatching { scheduler?.execute(queue::tick) }
    }

    // Sends [crash] now, by itself, and waits up to [timeoutMs]. Only for the
    // crash handler: the process is about to die, so the record goes straight
    // out rather than joining the queue — see [ExportQueue.exportCrash]. On its
    // own thread because the crashing thread may be the main thread, where
    // Android refuses network I/O outright.
    fun reportCrash(
        crash: LogRecord,
        timeoutMs: Long = CRASH_FLUSH_TIMEOUT_MS,
    ) {
        val queue = queue ?: return
        runCatching {
            val thread = Thread({ queue.exportCrash(crash) }, "vnote-telemetry-crash").apply { isDaemon = true }
            thread.start()
            thread.join(timeoutMs)
        }
    }

    // One tick — configure if needed, then export — on the calling thread. For tests.
    internal fun exportNow() {
        queue?.tick()
    }
}

// A batch that failed retryably, kept to be sent again.
private class Pending(
    val body: String,
    val items: Int,
    val attempts: Int,
)

// The two outboxes, configuration, the send policy, and one export at a time.
private class ExportQueue(
    private val transport: Transport,
    private val clock: () -> Long,
    private val serviceVersion: String,
    private val localLog: (String) -> Unit,
    private val jitter: () -> Double,
) {
    private val spans = Outbox<FinishedSpan>(PRE_CONFIG_CAPACITY)
    private val logs = Outbox<LogRecord>(PRE_CONFIG_CAPACITY)
    private val policy = ExportPolicy()
    private val startedAtMs = nowMs

    // The ingest origin once configured; null while waiting.
    @Volatile
    private var endpoint: String? = null

    @Volatile
    private var off: OffReason? = null

    private var refusedCredential: Int? = null
    private val retries = HashMap<Signal, Pending>()
    private var droppedAfterSend = 0L
    private var overflowReported = false

    val isExporting: Boolean get() = endpoint != null && off == null

    private val nowMs: Long get() = clock() / NANOS_PER_MILLI

    private val dropped: Long get() = spans.dropped + logs.dropped + droppedAfterSend

    fun push(span: FinishedSpan) {
        if (off == null) spans.push(span)
    }

    fun push(record: LogRecord) {
        if (off == null) logs.push(record)
    }

    // One tick: configure if not yet configured, wait out a refresh, then send
    // traces and logs. Never throws.
    @Synchronized
    fun tick() {
        runCatching {
            if (off != null) return
            reportOverflow()
            if (endpoint == null && !configure()) return
            if (policy.needsRefresh) {
                val current = transport.credentialId() ?: return
                if (current == refusedCredential) return
                policy.refreshed()
            }
            if (!transport.isReady() || !policy.shouldExport(nowMs)) return
            for (signal in Signal.entries) {
                if (!exportSignal(signal)) break
            }
            policy.stopped?.let(::turnOff)
        }
    }

    // Fetches configuration once a session exists. `true` when configured.
    private fun configure(): Boolean {
        if (nowMs - startedAtMs >= PRE_CONFIG_MAX_MS) {
            turnOff(OffReason.ConfigTimedOut)
            return false
        }
        // No session yet: keep waiting (and buffering) until the deadline.
        if (!transport.isReady()) return false
        when (val fetch = transport.fetchConfig()) {
            is ConfigFetch.Configured -> {
                endpoint = fetch.endpoint.trimEnd('/')
                spans.setCapacity(OUTBOX_CAPACITY)
                logs.setCapacity(OUTBOX_CAPACITY)
                localLog("client telemetry: exporting to ${fetch.endpoint}")
                return true
            }
            ConfigFetch.Absent -> {
                turnOff(OffReason.NotConfigured)
                return false
            }
            // Asked again next tick, until the pre-config deadline above.
            ConfigFetch.NotYet -> return false
        }
    }

    // Sends one signal's batch — a retry first, else fresh items. `false` when
    // the answer was not "accepted": the other signal waits for the policy.
    private fun exportSignal(signal: Signal): Boolean {
        val endpoint = endpoint ?: return false
        val retry = retries.remove(signal)
        val (body, items, attempts) =
            if (retry != null) {
                Triple(retry.body, retry.items, retry.attempts + 1)
            } else {
                val encoded = takeBody(signal) ?: return true
                Triple(encoded.first, encoded.second, 1)
            }
        val credential = transport.credentialId()
        val outcome = transport.send(endpoint, signal, body)
        val decision = policy.record(outcome, attempts, nowMs, jitter())
        if (decision.retryBatch) {
            retries[signal] = Pending(body, items, attempts)
        } else if (outcome != ExportOutcome.Accepted) {
            droppedAfterSend += items
        }
        if (decision.refreshToken) refusedCredential = credential
        report(decision.transition, outcome, "$endpoint/v1/${signal.path}")
        return outcome == ExportOutcome.Accepted
    }

    // The crash record, in a logs request of its own. Not the ordinary export:
    // backoff protects an ingest from a process that will keep sending, and
    // this one is about to stop; and in the queue the crash would be the newest
    // log, behind a traces request and up to a full batch of older lines. No
    // configuration, a stopped policy, or no session still wins. Not under the
    // queue's lock, so an export already in flight cannot hold it up.
    fun exportCrash(crash: LogRecord) {
        runCatching {
            val endpoint = endpoint ?: return
            if (off != null || policy.stopped != null || !transport.isReady()) return
            transport.send(endpoint, Signal.Logs, logsRequest(listOf(crash), serviceVersion))
        }
    }

    private fun takeBody(signal: Signal): Pair<String, Int>? =
        when (signal) {
            Signal.Traces ->
                spans.takeBatch().takeIf { it.isNotEmpty() }?.let { tracesRequest(it, serviceVersion) to it.size }
            Signal.Logs ->
                logs.takeBatch().takeIf { it.isNotEmpty() }?.let { logsRequest(it, serviceVersion) to it.size }
        }

    // The first time a queue overflows, say so locally. Not into the telemetry
    // queue: that is the channel that is failing.
    private fun reportOverflow() {
        if (overflowReported || spans.dropped + logs.dropped == 0L) return
        overflowReported = true
        localLog("client telemetry queue full; $dropped item(s) dropped so far")
    }

    private fun report(
        transition: Transition?,
        outcome: ExportOutcome,
        url: String,
    ) {
        when (transition) {
            Transition.StartedFailing ->
                localLog("client telemetry export failing: ${outcome.describe()} from $url; $dropped item(s) dropped so far")
            Transition.Recovered ->
                localLog("client telemetry export recovered at $url; $dropped item(s) dropped while failing")
            is Transition.Stopped, null -> Unit
        }
    }

    // Off for the rest of the process: discard everything and say why, once.
    private fun turnOff(reason: OffReason) {
        if (off != null) return
        off = reason
        policy.stop(reason)
        val lost = dropped + spans.size + logs.size + retries.values.sumOf { it.items }
        spans.clear()
        logs.clear()
        retries.clear()
        localLog("client telemetry off for this process: ${reason.description}; $lost item(s) dropped")
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
