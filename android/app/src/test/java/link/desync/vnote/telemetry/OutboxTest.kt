package link.desync.vnote.telemetry

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class OutboxTest {
    @Test
    fun dropsTheOldestWhenFullAndCountsIt() {
        val outbox = Outbox<Int>(capacity = 3)
        (1..5).forEach(outbox::push)
        assertEquals(listOf(3, 4, 5), outbox.takeBatch(10))
        assertEquals(2L, outbox.dropped)
    }

    @Test
    fun zeroCapacityDropsEverything() {
        val outbox = Outbox<Int>(capacity = 0)
        outbox.push(1)
        assertEquals(0, outbox.size)
        assertEquals(1L, outbox.dropped)
    }

    @Test
    fun takeBatchTakesTheOldestUpToMax() {
        val outbox = Outbox<Int>()
        (1..5).forEach(outbox::push)
        assertEquals(listOf(1, 2), outbox.takeBatch(2))
        assertEquals(3, outbox.size)
    }

    @Test
    fun outcomesFollowTheIngressStatuses() {
        assertEquals(ExportOutcome.Accepted, ExportOutcome.fromStatus(200))
        assertEquals(ExportOutcome.Accepted, ExportOutcome.fromStatus(204))
        assertEquals(ExportOutcome.SwitchedOff, ExportOutcome.fromStatus(404))
        for (status in listOf(400, 413, 415)) {
            assertEquals(ExportOutcome.BatchRefused, ExportOutcome.fromStatus(status))
        }
        for (status in listOf(null, 401, 403, 429, 500, 502, 503)) {
            assertEquals(ExportOutcome.Unavailable, ExportOutcome.fromStatus(status))
        }
    }

    @Test
    fun failuresBackOffExponentiallyToACap() {
        val policy = ExportPolicy()
        val waits =
            (1..6).map {
                policy.record(ExportOutcome.Unavailable)
                var skipped = 0
                while (!policy.shouldExport()) skipped += 1
                skipped
            }
        assertEquals(listOf(1, 2, 4, 8, 12, 12), waits)
    }

    @Test
    fun successResetsTheBackoff() {
        val policy = ExportPolicy()
        policy.record(ExportOutcome.Unavailable)
        policy.record(ExportOutcome.Unavailable)
        policy.record(ExportOutcome.Accepted)
        assertTrue(policy.shouldExport())
        policy.record(ExportOutcome.Unavailable)
        assertFalse(policy.shouldExport())
        assertTrue(policy.shouldExport())
    }

    @Test
    fun switchedOffIsFinal() {
        val policy = ExportPolicy()
        policy.record(ExportOutcome.SwitchedOff)
        policy.record(ExportOutcome.Accepted)
        assertTrue(policy.isSwitchedOff)
        assertFalse(policy.shouldExport())
    }
}
