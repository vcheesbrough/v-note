package link.desync.vnote.library

import kotlinx.coroutines.runBlocking
import link.desync.vnote.sync.ConnectionProblem
import link.desync.vnote.sync.ErrorClass
import org.junit.Assert.assertEquals
import org.junit.Test
import java.net.UnknownHostException

/**
 * The menu's `/health` verdict (#186): a transient failure — a dev redeploy's
 * 404/502 window, a phone without a route yet — is retried rather than
 * reported, and only a failure that outlasts every retry is shown.
 */
class ServerHealthProbeTest {
    private val sleeps = mutableListOf<Long>()

    private fun probe(results: List<Result<Unit>>): Pair<ServerHealthProbe, () -> Int> {
        var calls = 0
        val probe =
            ServerHealthProbe(
                baseUrl = BASE_URL,
                check = { results[minOf(calls++, results.lastIndex)] },
                retryDelaysMillis = listOf(1L, 2L, 4L),
                sleep = { sleeps += it },
            )
        return probe to { calls }
    }

    @Test
    fun aHealthyServerIsReportedWithoutRetrying() {
        val (probe, calls) = probe(listOf(Result.success(Unit)))

        assertEquals(ServerHealth.Healthy(BASE_URL), runBlocking { probe.probe() })
        assertEquals(1, calls())
        assertEquals(emptyList<Long>(), sleeps)
    }

    @Test
    fun aDeployWindowFailureIsRetriedAndNotReported() {
        val (probe, calls) = probe(listOf(httpFailure(404), httpFailure(502), Result.success(Unit)))

        assertEquals(ServerHealth.Healthy(BASE_URL), runBlocking { probe.probe() })
        assertEquals(3, calls())
        assertEquals(listOf(1L, 2L), sleeps)
    }

    @Test
    fun aFailureThatOutlastsEveryRetryIsReportedWithItsLastReason() {
        val (probe, calls) = probe(listOf(httpFailure(502), httpFailure(503)))

        assertEquals(ServerHealth.Unhealthy("HTTP 503"), runBlocking { probe.probe() })
        assertEquals(4, calls())
        assertEquals(listOf(1L, 2L, 4L), sleeps)
    }

    @Test
    fun aTransportErrorIsReportedByItsMessage() {
        val (probe, _) = probe(listOf(Result.failure(UnknownHostException("Unable to resolve host"))))

        assertEquals(ServerHealth.Unhealthy("Unable to resolve host"), runBlocking { probe.probe() })
    }

    @Test
    fun aTransportErrorWithoutAMessageIsReportedByItsType() {
        val (probe, _) = probe(listOf(Result.failure(UnknownHostException())))

        assertEquals(ServerHealth.Unhealthy("UnknownHostException"), runBlocking { probe.probe() })
    }

    @Test
    fun labelsMatchWhatTheMenuHasAlwaysShown() {
        assertEquals("Checking server health…", ServerHealth.Checking.label)
        assertEquals("Server healthy at $BASE_URL", ServerHealth.Healthy(BASE_URL).label)
        assertEquals("Health check failed: HTTP 502", ServerHealth.Unhealthy("HTTP 502").label)
    }

    @Test
    fun onlineTheLineIsTheVerdict() {
        for (health in listOf(ServerHealth.Checking, ServerHealth.Healthy(BASE_URL), ServerHealth.Unhealthy("HTTP 502"))) {
            assertEquals(health.label, health.lineGiven(offline = null))
        }
    }

    /**
     * #657: while the app knows it is offline, a healthy verdict — kept up
     * through a re-probe, and perhaps from before going offline — is not
     * shown. A check under way and a failed one still say what they are.
     */
    @Test
    fun offlineTheLineNeverClaimsHealthy() {
        val offline = ConnectionProblem("v-notes-dev.example", ErrorClass.OFFLINE, correlationId = null, atMillis = 0)

        assertEquals("Offline — can't reach v-notes-dev.example", ServerHealth.Healthy(BASE_URL).lineGiven(offline))
        assertEquals("Checking server health…", ServerHealth.Checking.lineGiven(offline))
        assertEquals("Health check failed: HTTP 502", ServerHealth.Unhealthy("HTTP 502").lineGiven(offline))
    }

    private fun httpFailure(code: Int): Result<Unit> = Result.failure(IllegalStateException("HTTP $code"))

    private companion object {
        const val BASE_URL = "https://v-notes-dev.example"
    }
}
