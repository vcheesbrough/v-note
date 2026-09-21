package link.desync.vnote.telemetry

// What the app does with telemetry between producing it and the sidecar
// accepting it (#406): a bounded queue, and the rules for when to send. Pure
// state — no Android API — so it is unit-tested on the JVM. A port of the SPA's
// `telemetry/outbox.rs` (#354); the two clients behave the same towards the
// ingress.
//
// The rule everything here serves: **telemetry is best effort and must never
// cost the product anything**. So nothing is retried, nothing grows without
// bound, and a collector that is down, off or unhappy is met with *fewer*
// requests, not more.

// How many finished items are held waiting for the next export, per signal.
// Sized for the unhealthy session, where exports are failing and items pile up.
internal const val OUTBOX_CAPACITY = 512

// The most items sent in one request. 256 items at a generous 2 KiB each is half
// the ingress's 1 MiB cap, before gzip — a batch cannot be refused for size.
internal const val MAX_BATCH = 256

// The longest the exporter waits between attempts while they keep failing, in
// ticks. At a 30 s tick, a dead sidecar costs one request every six minutes.
private const val MAX_BACKOFF_TICKS = 12

// A FIFO that discards its **oldest** entry when full: when exports are failing
// the interesting telemetry is what is happening *now*, and the newest items are
// the ones a recovery will actually deliver. Thread-safe — items arrive from the
// UI thread, OkHttp's threads and the exporter's.
internal class Outbox<T>(
    private val capacity: Int = OUTBOX_CAPACITY,
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

// What an export attempt came to, reduced to what the exporter does about it.
internal enum class ExportOutcome {
    // 2xx.
    Accepted,

    // 404: the server has client telemetry switched off (or predates it). Final
    // for this process.
    SwitchedOff,

    // The batch itself was refused — malformed, or over the size cap. Sending it
    // again cannot help, and nothing suggests the *next* batch is doomed.
    BatchRefused,

    // Anything else: a stale token, the sidecar down or shedding load, no
    // network. Worth trying again, later and less often.
    Unavailable,
    ;

    companion object {
        // `null` is a transport failure — no response at all.
        @Suppress("MagicNumber")
        fun fromStatus(status: Int?): ExportOutcome =
            when (status) {
                in 200..299 -> Accepted
                404 -> SwitchedOff
                400, 413, 415 -> BatchRefused
                else -> Unavailable
            }
    }
}

// Whether to export on a given tick.
//
// Note what is *not* here: a retry queue. A batch that fails is gone. Holding it
// would mean re-sending spans the sidecar may already have accepted before the
// response was lost, and would let one poisoned batch block the queue.
internal class ExportPolicy {
    // Final: once the server has said telemetry is off, nothing is sent again
    // until the process restarts. Read without the lock by the collecting side.
    @Volatile
    var isSwitchedOff = false
        private set

    // Ticks still to sit out before the next attempt.
    private var wait = 0

    // What `wait` is reset to on the next failure; doubles each time.
    private var backoff = 0

    // Called once per tick. `true` means "send now"; a `false` during backoff
    // consumes one tick of the wait.
    @Synchronized
    fun shouldExport(): Boolean {
        if (isSwitchedOff) return false
        if (wait > 0) {
            wait -= 1
            return false
        }
        return true
    }

    @Synchronized
    fun record(outcome: ExportOutcome) {
        when (outcome) {
            ExportOutcome.Accepted, ExportOutcome.BatchRefused -> {
                wait = 0
                backoff = 0
            }
            ExportOutcome.SwitchedOff -> isSwitchedOff = true
            ExportOutcome.Unavailable -> {
                backoff = (backoff * 2).coerceIn(1, MAX_BACKOFF_TICKS)
                wait = backoff
            }
        }
    }
}
