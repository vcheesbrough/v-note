package link.desync.vnote.telemetry

// What the app does with telemetry between producing it and the ingest
// accepting it (#406, reworked for `otlp-collector-oidc` in #439): a bounded
// queue, whether telemetry is configured at all, and the rules for when to send
// and what to do with each answer. Pure state — no Android API — so it is
// unit-tested on the JVM. The same policy as the SPA's
// `frontend/src/telemetry/outbox.rs`; both follow the estate's client contract
// (`observability` skill, `references/client-export.md`):
//
// - **No configuration, no telemetry.** Until the server has said where to send
//   (`GET /api/telemetry/config`), items wait in a small buffer for a short
//   time; if it says nothing, they are discarded and OTLP is never initialised.
// - **Retry only what is retryable**: `429`, `502`, `503`, `504` and transport
//   failures, with jittered exponential backoff that honours `Retry-After`.
//   Everything else drops the batch.
// - **`401` gets one refresh** — a new token from the app's own OIDC stack —
//   and a second `401` stops telemetry for the process.
// - **Give up for the process** after repeated failure.
// - **Say so locally, on transitions only**, never into the telemetry queue.

// How many finished items are held waiting for the next export, per signal,
// once telemetry is configured.
internal const val OUTBOX_CAPACITY = 512

// How many items wait for configuration to arrive, per signal.
internal const val PRE_CONFIG_CAPACITY = 128

// How long items wait for configuration before they are discarded and telemetry
// is given up for the process. Covers a user who signs in shortly after launch;
// never the whole session in hope.
internal const val PRE_CONFIG_MAX_MS = 120_000L

// The most items sent in one request. 256 items at a generous 2 KiB each is an
// eighth of the ingest's 4 MiB decompressed-body cap.
internal const val MAX_BATCH = 256

internal const val BACKOFF_BASE_MS = 30_000L
internal const val BACKOFF_MAX_MS = 600_000L
internal const val RETRY_AFTER_MAX_MS = 900_000L
internal const val MAX_BATCH_ATTEMPTS = 3
internal const val MAX_CONSECUTIVE_FAILURES = 6

// A FIFO that discards its **oldest** entry when full: when exports are failing
// the interesting telemetry is what is happening *now*, and the newest items are
// the ones a recovery will actually deliver. Thread-safe — items arrive from the
// UI thread, OkHttp's threads and the exporter's.
internal class Outbox<T>(
    private var capacity: Int = OUTBOX_CAPACITY,
) {
    private val items = ArrayDeque<T>()
    private var droppedCount = 0L

    @Synchronized
    fun push(item: T) {
        if (capacity == 0) {
            droppedCount += 1
            return
        }
        if (items.size == capacity) {
            items.removeFirst()
            droppedCount += 1
        }
        items.addLast(item)
    }

    // Raises (or lowers) the cap. Lowering drops the oldest to fit.
    @Synchronized
    fun setCapacity(newCapacity: Int) {
        capacity = newCapacity
        while (items.size > capacity) {
            items.removeFirst()
            droppedCount += 1
        }
    }

    // Removes and returns up to [max] of the oldest items.
    @Synchronized
    fun takeBatch(max: Int = MAX_BATCH): List<T> {
        val batch = ArrayList<T>(minOf(max, items.size))
        while (batch.size < max && items.isNotEmpty()) {
            batch.add(items.removeFirst())
        }
        return batch
    }

    @Synchronized
    fun clear() = items.clear()

    @get:Synchronized
    val size: Int get() = items.size

    // How many items were discarded for lack of room, ever.
    @get:Synchronized
    val dropped: Long get() = droppedCount
}

// What `GET /api/telemetry/config` came to.
internal sealed class ConfigFetch {
    // `200`: the ingest's bare origin.
    data class Configured(
        val endpoint: String,
    ) : ConfigFetch()

    // `204` (ingest off here) or `404` (a server that predates the route): the
    // server's answer is "no configuration".
    data object Absent : ConfigFetch()

    // No response, a `5xx`, a `401`, any other status or an unparseable body:
    // not answered yet. Asked again on a later tick, within the pre-config
    // window — a phone's radio is at its flakiest right at launch, when this
    // is first asked.
    data object NotYet : ConfigFetch()
}

// Why telemetry is off. Each is said once, locally.
enum class OffReason(
    val description: String,
) {
    NotConfigured("the server gave no telemetry configuration"),
    ConfigTimedOut("no telemetry configuration arrived in time"),
    Unauthorized("the ingest refused a refreshed token (401)"),
    GaveUp("exports kept failing"),
}

// What an export attempt came to, reduced to what the exporter does about it.
internal sealed class ExportOutcome {
    // 2xx.
    data object Accepted : ExportOutcome()

    // `401`: every refusal of the token. One refresh, then stop.
    data object Unauthorized : ExportOutcome()

    // Permanent for this payload: `400`, `403`, `413`, `500` and every status
    // OTLP does not name retryable. The batch is dropped.
    data class Rejected(
        val status: Int,
    ) : ExportOutcome()

    // `429`, `502`, `503`, `504`, or no response (`status` null).
    data class Retryable(
        val status: Int?,
        val retryAfterMs: Long? = null,
    ) : ExportOutcome()

    fun describe(): String =
        when (this) {
            Accepted -> "accepted"
            Unauthorized -> "status 401"
            is Rejected -> "status $status"
            is Retryable -> status?.let { "status $it" } ?: "transport error"
        }

    companion object {
        // `status` null is a transport failure; `retryAfter` the raw header.
        @Suppress("MagicNumber")
        fun fromResponse(
            status: Int?,
            retryAfter: String? = null,
        ): ExportOutcome =
            when (status) {
                null -> Retryable(null)
                in 200..299 -> Accepted
                401 -> Unauthorized
                429, 502, 503, 504 -> Retryable(status, parseRetryAfterMs(retryAfter))
                else -> Rejected(status)
            }
    }
}

// `Retry-After` as delay-seconds. The HTTP-date form is ignored: a device clock
// is not trusted enough to subtract from, and the backoff alone is a safe answer.
@Suppress("MagicNumber")
internal fun parseRetryAfterMs(value: String?): Long? =
    value
        ?.trim()
        ?.toLongOrNull()
        ?.takeIf { it >= 0 }
        ?.let { it * 1000 }

// A change of state worth one local log line. Nothing else is reported.
internal sealed class Transition {
    data object StartedFailing : Transition()

    data object Recovered : Transition()

    data class Stopped(
        val reason: OffReason,
    ) : Transition()
}

internal data class Decision(
    // Keep the batch and send it again later. `false` means it is gone.
    val retryBatch: Boolean = false,
    // Wait for a fresh token before the next attempt.
    val refreshToken: Boolean = false,
    val transition: Transition? = null,
)

// Exponential backoff with "equal jitter": the `n`th consecutive failure waits
// between half and all of `BASE * 2^(n-1)`, capped. Half fixed so a crowd still
// spreads out; half random so it does not come back in lockstep.
@Suppress("MagicNumber")
internal fun backoffMs(
    failures: Int,
    jitter: Double,
): Long {
    val exponent = (failures - 1).coerceIn(0, 16)
    val ceiling = minOf(BACKOFF_BASE_MS shl exponent, BACKOFF_MAX_MS)
    val half = ceiling / 2
    return half + (half * jitter.coerceIn(0.0, 1.0)).toLong()
}

// When to send, and what each answer means for the next attempt. Time is
// milliseconds and `jitter` a uniform draw in [0, 1), both passed in, so the
// policy is deterministic under test.
internal class ExportPolicy {
    @Volatile
    var stopped: OffReason? = null
        private set

    private var failures = 0
    private var failing = false
    private var refreshedAfter401 = false
    private var refreshPending = false
    private var nextAttemptAtMs = Long.MIN_VALUE

    @Synchronized
    fun shouldExport(nowMs: Long): Boolean = stopped == null && !refreshPending && nowMs >= nextAttemptAtMs

    @get:Synchronized
    val needsRefresh: Boolean get() = stopped == null && refreshPending

    @Synchronized
    fun refreshed() {
        refreshPending = false
    }

    @Synchronized
    fun stop(reason: OffReason): Transition? {
        if (stopped != null) return null
        stopped = reason
        return Transition.Stopped(reason)
    }

    // Records one attempt's answer. [attempts] is how many times this batch has
    // now been sent, this attempt included.
    @Synchronized
    fun record(
        outcome: ExportOutcome,
        attempts: Int,
        nowMs: Long,
        jitter: Double,
    ): Decision {
        if (stopped != null) return Decision()
        return when (outcome) {
            ExportOutcome.Accepted -> {
                failures = 0
                refreshedAfter401 = false
                nextAttemptAtMs = nowMs
                val recovered = failing
                failing = false
                Decision(transition = if (recovered) Transition.Recovered else null)
            }
            ExportOutcome.Unauthorized ->
                if (refreshedAfter401) {
                    Decision(transition = stop(OffReason.Unauthorized))
                } else {
                    refreshedAfter401 = true
                    refreshPending = true
                    Decision(refreshToken = true, transition = startFailing())
                }
            is ExportOutcome.Rejected -> Decision(transition = startFailing())
            is ExportOutcome.Retryable -> {
                failures += 1
                if (failures >= MAX_CONSECUTIVE_FAILURES) {
                    Decision(transition = stop(OffReason.GaveUp))
                } else {
                    val retryAfter = minOf(outcome.retryAfterMs ?: 0L, RETRY_AFTER_MAX_MS)
                    nextAttemptAtMs = nowMs + maxOf(backoffMs(failures, jitter), retryAfter)
                    Decision(retryBatch = attempts < MAX_BATCH_ATTEMPTS, transition = startFailing())
                }
            }
        }
    }

    private fun startFailing(): Transition? {
        if (failing) return null
        failing = true
        return Transition.StartedFailing
    }
}
