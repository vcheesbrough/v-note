package link.desync.vnote.library

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The menu's re-check rules (#186, PR #65 review). Each probe attempt waits on
 * a gate the test opens, so "in flight" is an explicit state here.
 */
class ServerHealthMonitorTest {
    private val gates = mutableListOf<CompletableDeferred<Result<Unit>>>()
    private val givenUp = mutableListOf<String>()
    private val scope = CoroutineScope(Job() + Dispatchers.Unconfined)
    private val monitor =
        ServerHealthMonitor(
            // No retries: one attempt per probe, so one gate per check.
            probe =
                ServerHealthProbe(
                    baseUrl = BASE_URL,
                    check = { CompletableDeferred<Result<Unit>>().also { gates += it }.await() },
                    retryDelaysMillis = emptyList(),
                ),
            scope = scope,
            onGiveUp = { givenUp += it },
        )

    @After
    fun tearDown() = scope.cancel()

    @Test
    fun theFirstCheckShowsCheckingThenTheVerdict() {
        monitor.check()
        assertEquals(ServerHealth.Checking, monitor.state.value)

        gates.single().complete(Result.success(Unit))

        assertEquals(ServerHealth.Healthy(BASE_URL), monitor.state.value)
    }

    @Test
    fun reopeningTheMenuDoesNotRestartAnInFlightProbe() {
        monitor.check()
        gates.single().complete(Result.success(Unit))

        // The server goes down; the menu is opened repeatedly while the
        // re-check is still running.
        monitor.check()
        monitor.check()
        monitor.check()

        assertEquals("one probe for the re-check, not one per open", 2, gates.size)
        assertEquals(ServerHealth.Healthy(BASE_URL), monitor.state.value)

        gates.last().complete(Result.failure(IllegalStateException("HTTP 502")))

        assertEquals(ServerHealth.Unhealthy("HTTP 502"), monitor.state.value)
        assertEquals(listOf("HTTP 502"), givenUp)
    }

    @Test
    fun aRecheckAfterAFailureShowsCheckingNotTheOldFailure() {
        monitor.check()
        gates.single().complete(Result.failure(IllegalStateException("HTTP 503")))
        assertEquals(ServerHealth.Unhealthy("HTTP 503"), monitor.state.value)

        monitor.check()

        assertEquals(ServerHealth.Checking, monitor.state.value)
        gates.last().complete(Result.success(Unit))
        assertEquals(ServerHealth.Healthy(BASE_URL), monitor.state.value)
    }

    @Test
    fun aFinishedProbeIsReplacedByANewOneOnTheNextCheck() {
        monitor.check()
        gates.single().complete(Result.success(Unit))

        monitor.check()

        assertEquals(2, gates.size)
    }

    private companion object {
        const val BASE_URL = "https://v-notes-dev.example"
    }
}
