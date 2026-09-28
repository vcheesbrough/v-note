package link.desync.vnote.telemetry

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
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
    fun capacityCanBeRaisedAndLowered() {
        val outbox = Outbox<Int>(capacity = 2)
        (1..2).forEach(outbox::push)
        outbox.setCapacity(4)
        outbox.push(3)
        assertEquals(3, outbox.size)
        outbox.setCapacity(1)
        assertEquals(listOf(3), outbox.takeBatch(10))
        assertEquals(2L, outbox.dropped)
    }

    // OTLP names the retryable answers, and they are the only ones.
    @Test
    fun statusesAreClassifiedPerTheOtlpContract() {
        assertEquals(ExportOutcome.Accepted, ExportOutcome.fromResponse(200))
        assertEquals(ExportOutcome.Accepted, ExportOutcome.fromResponse(204))
        assertEquals(ExportOutcome.Unauthorized, ExportOutcome.fromResponse(401))
        for (status in listOf(400, 403, 404, 413, 415, 500, 501, 307)) {
            assertEquals(ExportOutcome.Rejected(status), ExportOutcome.fromResponse(status))
        }
        for (status in listOf(429, 502, 503, 504)) {
            assertEquals(ExportOutcome.Retryable(status), ExportOutcome.fromResponse(status))
        }
        assertEquals(ExportOutcome.Retryable(null), ExportOutcome.fromResponse(null))
    }

    @Test
    fun retryAfterIsReadAsSecondsOnRetryableAnswersOnly() {
        assertEquals(ExportOutcome.Retryable(429, 7_000), ExportOutcome.fromResponse(429, "7"))
        assertEquals(ExportOutcome.Retryable(503, 2_000), ExportOutcome.fromResponse(503, " 2 "))
        assertEquals(
            ExportOutcome.Retryable(503, null),
            ExportOutcome.fromResponse(503, "Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        assertEquals(ExportOutcome.Rejected(400), ExportOutcome.fromResponse(400, "5"))
        assertNull(parseRetryAfterMs("-1"))
    }

    @Test
    fun aHealthyExporterExportsWheneverAsked() {
        val policy = ExportPolicy()
        for (tick in 0L..4L) {
            assertTrue(policy.shouldExport(tick * 30_000))
            assertNull(policy.record(ExportOutcome.Accepted, 1, tick * 30_000, MID).transition)
        }
    }

    @Test
    fun aRejectedBatchIsDroppedWithoutBackoff() {
        val policy = ExportPolicy()
        val decision = policy.record(ExportOutcome.Rejected(400), 1, 0, MID)
        assertFalse(decision.retryBatch)
        assertTrue(policy.shouldExport(0))
    }

    // 401 → one refresh → a second 401 stops telemetry for the process.
    @Test
    fun unauthorizedRefreshesOnceThenStops() {
        val policy = ExportPolicy()
        val first = policy.record(ExportOutcome.Unauthorized, 1, 0, MID)
        assertTrue(first.refreshToken)
        assertFalse("the 401'd batch is dropped", first.retryBatch)
        assertTrue(policy.needsRefresh)
        assertFalse(policy.shouldExport(0))

        policy.refreshed()
        assertTrue(policy.shouldExport(0))

        val second = policy.record(ExportOutcome.Unauthorized, 1, 0, MID)
        assertEquals(Transition.Stopped(OffReason.Unauthorized), second.transition)
        assertFalse(second.refreshToken)
        assertEquals(OffReason.Unauthorized, policy.stopped)
        assertFalse(policy.shouldExport(Long.MAX_VALUE))
    }

    @Test
    fun aSuccessResetsTheRefreshBudget() {
        val policy = ExportPolicy()
        policy.record(ExportOutcome.Unauthorized, 1, 0, MID)
        policy.refreshed()
        policy.record(ExportOutcome.Accepted, 1, 0, MID)
        assertTrue(policy.record(ExportOutcome.Unauthorized, 1, 0, MID).refreshToken)
        assertNull(policy.stopped)
    }

    @Test
    fun retryableFailuresBackOffAndRetryTheBatch() {
        val policy = ExportPolicy()
        val decision = policy.record(ExportOutcome.Retryable(503), 1, 0, MID)
        assertTrue(decision.retryBatch)
        assertEquals(Transition.StartedFailing, decision.transition)
        assertFalse(policy.shouldExport(1))
        assertTrue(policy.shouldExport(backoffMs(1, MID)))
    }

    @Test
    fun retryAfterIsHonouredAndCapped() {
        val longer = ExportPolicy()
        longer.record(ExportOutcome.Retryable(429, 3_600_000), 1, 0, MID)
        assertFalse(longer.shouldExport(RETRY_AFTER_MAX_MS - 1))
        assertTrue(longer.shouldExport(RETRY_AFTER_MAX_MS))

        val shorter = ExportPolicy()
        shorter.record(ExportOutcome.Retryable(429, 1_000), 1, 0, MID)
        assertFalse("the backoff still applies", shorter.shouldExport(1_000))
    }

    @Test
    fun jitterIsBounded() {
        for (failures in 1..20) {
            val ceiling = minOf(BACKOFF_BASE_MS shl (failures - 1).coerceAtMost(16), BACKOFF_MAX_MS)
            assertEquals(ceiling / 2, backoffMs(failures, 0.0))
            val high = backoffMs(failures, 0.999_999)
            assertTrue("$failures: $high", high <= ceiling && high > ceiling * 0.99)
            assertTrue(backoffMs(failures, 7.0) <= ceiling)
            assertTrue(backoffMs(failures, -3.0) >= ceiling / 2)
        }
        assertTrue("exponential", backoffMs(2, MID) > backoffMs(1, MID))
    }

    @Test
    fun aBatchIsRetriedABoundedNumberOfTimes() {
        val policy = ExportPolicy()
        assertTrue(policy.record(ExportOutcome.Retryable(null), 1, 0, MID).retryBatch)
        assertTrue(policy.record(ExportOutcome.Retryable(null), 2, 0, MID).retryBatch)
        assertFalse(policy.record(ExportOutcome.Retryable(null), MAX_BATCH_ATTEMPTS, 0, MID).retryBatch)
    }

    @Test
    fun repeatedFailureGivesUpForTheProcess() {
        val policy = ExportPolicy()
        val transitions =
            (1..MAX_CONSECUTIVE_FAILURES).mapNotNull {
                policy.record(ExportOutcome.Retryable(502), it, 0, MID).transition
            }
        assertEquals(listOf(Transition.StartedFailing, Transition.Stopped(OffReason.GaveUp)), transitions)
        assertFalse(policy.shouldExport(Long.MAX_VALUE))
        policy.record(ExportOutcome.Accepted, 1, 0, MID)
        assertFalse(policy.shouldExport(Long.MAX_VALUE))
    }

    @Test
    fun recoveryIsReportedOnceAndClearsTheBackoff() {
        val policy = ExportPolicy()
        policy.record(ExportOutcome.Retryable(503), 1, 0, MID)
        policy.record(ExportOutcome.Retryable(503), 2, 0, MID)
        assertEquals(Transition.Recovered, policy.record(ExportOutcome.Accepted, 1, 1_000_000, MID).transition)
        assertTrue(policy.shouldExport(1_000_000))
    }

    @Test
    fun theConfigBodyYieldsTheEndpointOnly() {
        assertEquals(
            ConfigFetch.Configured("https://v-notes-dev.desync.link"),
            parseConfig("""{"endpoint":"https://v-notes-dev.desync.link/","access_token":"t","expires_at":1}"""),
        )
        assertEquals(ConfigFetch.NotYet, parseConfig("""{"endpoint":""}"""))
        assertEquals(ConfigFetch.NotYet, parseConfig("""{"endpoint":"ftp://x"}"""))
        assertEquals(ConfigFetch.NotYet, parseConfig("not json"))
    }

    private companion object {
        const val MID = 0.5
    }
}
