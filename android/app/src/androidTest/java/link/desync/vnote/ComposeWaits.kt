package link.desync.vnote

import androidx.compose.ui.test.ComposeTimeoutException
import androidx.compose.ui.test.junit4.ComposeTestRule
import java.util.concurrent.BlockingQueue
import java.util.concurrent.CountDownLatch

/**
 * Waits for [latch] while letting Compose run (#465).
 *
 * Under the v2 test rule a composition-scoped coroutine — the page session
 * subscribing, a batch being sent — only runs when the test synchronises with
 * Compose. A bare `latch.await` blocks the test thread without doing that, so
 * it can time out waiting on work that is queued behind it. `waitUntil` runs
 * the due work and advances the test clock while it waits.
 */
internal fun ComposeTestRule.awaitLatch(
    latch: CountDownLatch,
    timeoutMillis: Long,
): Boolean =
    try {
        waitUntil(timeoutMillis) { latch.count == 0L }
        true
    } catch (_: ComposeTimeoutException) {
        false
    }

/**
 * Takes messages off [queue] until one satisfies [matches], while letting
 * Compose run (see [awaitLatch]); null if none arrives within [timeoutMillis].
 * Messages that do not match are dropped, as a polling loop would.
 */
internal fun <T : Any> ComposeTestRule.awaitQueued(
    queue: BlockingQueue<T>,
    timeoutMillis: Long,
    matches: (T) -> Boolean,
): T? {
    var found: T? = null
    try {
        waitUntil(timeoutMillis) {
            while (found == null) {
                val next = queue.poll() ?: break
                if (matches(next)) found = next
            }
            found != null
        }
    } catch (_: ComposeTimeoutException) {
        return null
    }
    return found
}
