package link.desync.vnote.library

import kotlinx.coroutines.delay

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
