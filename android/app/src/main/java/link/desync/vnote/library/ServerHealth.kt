package link.desync.vnote.library

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

// The server's `/health` as the library menu reports it (#186).
sealed interface ServerHealth {
    data object Checking : ServerHealth

    data class Healthy(
        val baseUrl: String,
    ) : ServerHealth

    data class Unhealthy(
        val reason: String,
    ) : ServerHealth

    val label: String
        get() =
            when (this) {
                Checking -> "Checking server health…"
                is Healthy -> "Server healthy at $baseUrl"
                is Unhealthy -> "Health check failed: $reason"
            }
}

// Probes `/health`, retrying before it reports a failure.
//
// A single probe used to be taken when the library first composed and shown
// until the app restarted. Every dev redeploy has a window of several seconds
// in which Traefik answers `/health` with 404 (no router yet) or 502 (the new
// container not listening yet), and a phone often has no route for a moment
// after waking; either froze "Health check failed" into the menu although the
// server was fine seconds later. A failure is now reported only once every
// retry in [retryDelaysMillis] has failed too, and the menu re-probes whenever
// it is opened.
class ServerHealthProbe(
    private val baseUrl: String,
    private val check: suspend () -> Result<Unit>,
    private val retryDelaysMillis: List<Long> = DEFAULT_RETRY_DELAYS_MILLIS,
    private val sleep: suspend (Long) -> Unit = { delay(it) },
) {
    suspend fun probe(): ServerHealth {
        var result = check()
        for (backoff in retryDelaysMillis) {
            if (result.isSuccess) break
            sleep(backoff)
            result = check()
        }
        return result.fold(
            onSuccess = { ServerHealth.Healthy(baseUrl) },
            onFailure = { error -> ServerHealth.Unhealthy(error.message ?: error.javaClass.simpleName) },
        )
    }

    companion object {
        // About 15 s in all: longer than the dev redeploy gap Traefik reports.
        val DEFAULT_RETRY_DELAYS_MILLIS = listOf(1_000L, 2_000L, 4_000L, 8_000L)
    }
}

// The menu's health line over time (#186): probed when the library first
// composes and again each time the menu opens, so it is never a stale
// snapshot from app start.
//
// A check while one is already in flight is ignored rather than restarting
// it (PR #65 review): restarting on every menu open could keep a probe from
// ever finishing, leaving a stale result up. A re-check after a healthy
// result keeps showing it until the new verdict is in; one after a failure
// shows [ServerHealth.Checking] instead of the old failure.
class ServerHealthMonitor(
    private val probe: ServerHealthProbe,
    private val scope: CoroutineScope,
    private val onGiveUp: (reason: String) -> Unit = {},
) {
    private val mutableState = MutableStateFlow<ServerHealth>(ServerHealth.Checking)
    val state: StateFlow<ServerHealth> = mutableState.asStateFlow()

    private var inFlight: Job? = null

    fun check() {
        if (inFlight?.isActive == true) return
        if (mutableState.value !is ServerHealth.Healthy) mutableState.value = ServerHealth.Checking
        inFlight =
            scope.launch {
                val result = probe.probe()
                if (result is ServerHealth.Unhealthy) onGiveUp(result.reason)
                mutableState.value = result
            }
    }
}
