package link.desync.vnote.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

// The one retry schedule (#651): 1 s, doubling to a 30 s ceiling, and then
// every 30 s for as long as it takes. There is no last attempt — whatever
// failed is tried again until it works — and any success starts it over.
// The library channel, an open page, and the sync engine each keep their own.
internal class Backoff(
    private val delayFor: (step: Int) -> Long = ::delayMs,
) {
    // Where the schedule is, not how often it ran: it stops counting at the
    // ceiling, so it can be asked for the next delay forever.
    private var step = 0

    // How long to wait before the next attempt.
    fun next(): Long {
        val delay = delayFor(step)
        if (step < MAX_DOUBLINGS) step += 1
        return delay
    }

    // Something worked, or the network came back: the next wait is the first.
    fun reset() {
        step = 0
    }

    companion object {
        fun delayMs(step: Int): Long = (BASE_MS shl step.coerceIn(0, MAX_DOUBLINGS)).coerceAtMost(MAX_MS)

        private const val BASE_MS = 1_000L
        private const val MAX_MS = 30_000L
        private const val MAX_DOUBLINGS = 5
    }
}

// One pending retry on a [Backoff] (#651): however many things are waiting
// for it, there is one timer, and it is set again for as long as anything
// still fails. Main thread only.
internal class RetryTimer(
    private val scope: CoroutineScope,
    private val backoff: Backoff = Backoff(),
) {
    private var job: Job? = null

    // Runs [retry] after the next backoff step, unless a retry is already
    // on its way.
    fun schedule(retry: () -> Unit) {
        if (job?.isActive == true) return
        val delayMs = backoff.next()
        job =
            scope.launch {
                delay(delayMs)
                job = null
                retry()
            }
    }

    fun cancel() {
        job?.cancel()
        job = null
    }

    // Something worked, or the network returned: no retry is pending, and
    // the next one waits the first step again.
    fun startOver() {
        cancel()
        backoff.reset()
    }
}
