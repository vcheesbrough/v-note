package link.desync.vnote

import android.util.Log
import link.desync.vnote.net.Endpoints
import link.desync.vnote.net.HttpClients
import link.desync.vnote.net.LoopbackOnlyGuard
import link.desync.vnote.net.NonLoopbackHostException
import java.util.Collections

/**
 * Keeps instrumented tests off the network (#464). Installed by [VNoteTestRunner]
 * before the Application starts:
 *
 * - **The app's own defaults are pointed at loopback.** [Endpoints.serverBaseUrl]
 *   is what client telemetry fetches its config from and what an activity
 *   launched without an injected `apiClientFactory` talks to. Nothing listens on
 *   [SERVER_BASE_URL], so those calls fail at once, on the device, instead of
 *   polling the real dev deployment with a test's fake token.
 * - **Everything else is refused, loudly.** Every client built through
 *   [HttpClients] — REST, both WebSockets, `/health`, thumbnails, telemetry, the
 *   token endpoint and AppAuth's discovery — carries [guard], which rejects any
 *   host that is not loopback before it resolves or connects. A refusal is
 *   logged and then crashes the test process from a fresh thread, so the
 *   runner reports the running test as failed with the refused URL: an
 *   `IOException` alone would be swallowed by code that treats network failure
 *   as normal (telemetry does, by design).
 *
 * A test that wants to see a refusal without the crash wraps it in
 * [collectViolations]. The `DensePageSeeder` measurement fixture, which targets
 * a real environment on purpose, calls [allowRealNetworkForMeasurement].
 */
object TestNetwork {
    // Loopback, and the discard port: nothing in an emulator listens there.
    const val SERVER_BASE_URL = "http://127.0.0.1:9"

    private const val TAG = "VNoteTestNetwork"

    val guard = LoopbackOnlyGuard(::onViolation)

    @Volatile
    private var collected: MutableList<NonLoopbackHostException>? = null

    fun install() {
        Endpoints.serverBaseUrl = SERVER_BASE_URL
        HttpClients.hostGuard = guard
    }

    /** Runs [block], returning the refusals it caused instead of crashing on them. */
    @Synchronized
    fun collectViolations(block: () -> Unit): List<NonLoopbackHostException> {
        val violations = Collections.synchronizedList(mutableListOf<NonLoopbackHostException>())
        collected = violations
        try {
            block()
        } finally {
            collected = null
        }
        return synchronized(violations) { violations.toList() }
    }

    /**
     * Lifts the guard for the rest of this process. Only for a measurement
     * fixture run by hand against a real environment, never for a test.
     */
    fun allowRealNetworkForMeasurement() {
        Log.w(TAG, "network guard lifted for a measurement fixture: requests may leave the device")
        guard.allowAnyHost = true
    }

    private fun onViolation(refusal: NonLoopbackHostException) {
        Log.e(TAG, refusal.message, refusal)
        collected?.let { violations ->
            violations += refusal
            return
        }
        val failure =
            AssertionError(
                "${refusal.message} — an instrumented test must talk to a MockWebServer on loopback, " +
                    "not a real host. The cause's stack trace shows the caller.",
                refusal,
            )
        Thread({ throw failure }, "vnote-network-guard").start()
    }
}
