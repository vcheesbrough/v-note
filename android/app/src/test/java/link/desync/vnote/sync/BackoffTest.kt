package link.desync.vnote.sync

import org.junit.Assert.assertEquals
import org.junit.Test

// The one retry schedule (#279, #651). That the channels follow it is covered
// in LibraryStateHolderTest and against real sockets in the instrumented
// suite.
class BackoffTest {
    @Test
    fun firstRetryIsPromptSoALaggedCloseRecoversQuickly() {
        assertEquals(1_000L, Backoff().next())
    }

    @Test
    fun backoffDoublesWhileTheServerStaysAway() {
        val backoff = Backoff()
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 8_000L, 16_000L), List(5) { backoff.next() })
    }

    @Test
    fun delayIsCappedAndNeverOverflows() {
        assertEquals(30_000L, Backoff.delayMs(5))
        assertEquals(30_000L, Backoff.delayMs(64))
        assertEquals(30_000L, Backoff.delayMs(Int.MAX_VALUE))
    }

    /** There is no last attempt: past the ceiling it is 30 s, every time, for good. */
    @Test
    fun retriesNeverStopAndSettleAtThirtySeconds() {
        val backoff = Backoff()
        repeat(5) { backoff.next() }

        val later = List(10_000) { backoff.next() }

        assertEquals(setOf(30_000L), later.toSet())
    }

    @Test
    fun aSuccessStartsTheScheduleOver() {
        val backoff = Backoff()
        repeat(9) { backoff.next() }

        backoff.reset()

        assertEquals(listOf(1_000L, 2_000L), List(2) { backoff.next() })
    }
}
