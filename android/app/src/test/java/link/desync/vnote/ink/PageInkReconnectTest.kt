package link.desync.vnote.ink

import org.junit.Assert.assertEquals
import org.junit.Test

// The page channel's reconnect backoff (#279). The reconnect itself is covered
// against a real socket in PageInkInstrumentedTest.
class PageInkReconnectTest {
    @Test
    fun firstRetryIsPromptSoALaggedCloseRecoversQuickly() {
        assertEquals(1_000L, PageInkSession.reconnectDelayMs(0))
    }

    @Test
    fun backoffDoublesWhileTheServerStaysAway() {
        assertEquals(
            listOf(1_000L, 2_000L, 4_000L, 8_000L, 16_000L),
            (0..4).map(PageInkSession::reconnectDelayMs),
        )
    }

    @Test
    fun backoffIsCappedAndNeverOverflows() {
        assertEquals(30_000L, PageInkSession.reconnectDelayMs(5))
        assertEquals(30_000L, PageInkSession.reconnectDelayMs(64))
        assertEquals(30_000L, PageInkSession.reconnectDelayMs(Int.MAX_VALUE))
    }
}
