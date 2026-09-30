package link.desync.vnote

import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.espresso.Espresso
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.atomic.AtomicInteger

/**
 * The library menu's server-health line (#186), end to end through
 * [OkHttpApiClient] against a fake server.
 *
 * It used to be one probe taken when the library first composed, sent to
 * `BuildConfig.BASE_URL` on a client of its own, and shown until the app
 * restarted — so a single failed request during a dev redeploy left "Health
 * check failed" in the menu for good. These pin the fix: the probe goes to the
 * app's own server, a transient failure is retried rather than shown, and
 * opening the menu re-checks instead of repeating a stale failure.
 *
 * Signed out on purpose: the line is shown either way, and no other request
 * competes with the probe.
 */
@RunWith(AndroidJUnit4::class)
class ServerHealthInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient
    private lateinit var baseUrl: String

    private val healthRequests = AtomicInteger(0)

    // `/health` answers 503 for the next [failuresLeft] requests, then 200.
    // The app starts inside a redeploy window: two failures, fewer than the
    // probe's three attempts.
    private val failuresLeft = AtomicInteger(LAUNCH_FAILURES)

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                server = MockWebServer()
                server.dispatcher = testDispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                baseUrl = server.url("/").toString().removeSuffix("/")

                tokenStore = TokenStore(InstrumentationRegistry.getInstrumentation().targetContext)
                tokenStore.clear()
                MainActivity.healthRetryDelaysMillis = listOf(RETRY_DELAY_MS, RETRY_DELAY_MS)
                MainActivity.apiClientFactory = { store, authRepository ->
                    OkHttpApiClient(baseUrl, store, authRepository).also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                MainActivity.healthRetryDelaysMillis = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                if (::server.isInitialized) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(environmentRule).around(composeRule)

    @Test
    fun aTransientHealthFailureIsRetriedNotReported() {
        openMenu()

        awaitHealthLine("Server healthy at $baseUrl")
        assertTrue(
            "the launch probe retried through the failures",
            healthRequests.get() >= LAUNCH_FAILURES + 1,
        )
    }

    @Test
    fun openingTheMenuRechecksInsteadOfShowingAStaleFailure() {
        // Down for longer than any retry budget.
        failuresLeft.set(Int.MAX_VALUE)

        openMenu()
        awaitHealthLine("Health check failed: HTTP 503")

        // The server recovers; the menu must say so the next time it opens.
        failuresLeft.set(0)
        Espresso.pressBack()
        openMenu()

        awaitHealthLine("Server healthy at $baseUrl")
    }

    private fun openMenu() {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                composeRule.onNodeWithText("Sign in").assertExists()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithTag("main-menu-button").performClick()
    }

    private fun healthLine(): SemanticsNodeInteraction = composeRule.onNodeWithTag("server-health")

    private fun awaitHealthLine(expected: String) {
        composeRule.waitUntil(timeoutMillis = 15_000) {
            runCatching {
                healthLine().assertTextEquals(expected)
                true
            }.getOrDefault(false)
        }
        healthLine().assertTextEquals(expected)
    }

    private fun testDispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.url.encodedPath) {
                    "/health" -> {
                        healthRequests.incrementAndGet()
                        if (failuresLeft.getAndUpdate { if (it > 0) it - 1 else 0 } > 0) {
                            MockResponse(code = 503)
                        } else {
                            MockResponse(code = 200, body = """{"status":"ok"}""")
                        }
                    }
                    else -> MockResponse(code = 404)
                }
        }

    private companion object {
        const val RETRY_DELAY_MS = 50L
        const val LAUNCH_FAILURES = 2
    }
}
