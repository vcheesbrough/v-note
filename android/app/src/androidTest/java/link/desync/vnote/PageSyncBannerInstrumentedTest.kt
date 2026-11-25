package link.desync.vnote

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import link.desync.vnote.ink.PageInkSession
import link.desync.vnote.ink.PageStatusKind
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * #150 on screen: the open page's banner names the kind of trouble — the
 * server unreachable, another editor, a session only signing in can fix — so
 * none of them reads like another, and none shows while sync is nominal. The
 * session-level behaviour behind each is in [PageInkInstrumentedTest].
 */
@RunWith(AndroidJUnit4::class)
class PageSyncBannerInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    // What the page channel does on each upgrade, set per test before launch.
    @Volatile
    private var pageMode = PageMode.Healthy

    // While false, a page upgrade is answered 503: the server is down.
    private val serverUp = AtomicBoolean(true)
    private val pageUpgrades = AtomicInteger(0)

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                DrawingToolPreferences.clear(context)
                server = MockWebServer()
                server.dispatcher = dispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val baseUrl = server.url("/").toString().removeSuffix("/")
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { store, authRepository ->
                    // The refresh is the identity provider's, which a loopback
                    // test cannot reach; here it simply fails, as it does when
                    // the refresh token has been revoked.
                    object : ApiClient by OkHttpApiClient(baseUrl, store, authRepository) {
                        override suspend fun refreshSession(): Boolean = false
                    }.also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                if (::server.isInitialized) server.close()
            }
        }

    // [pageMode] is read when the page upgrades, which is only once a test
    // opens the page — so a test sets it before [openPage].
    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    @Test
    fun aHealthyPageShowsNoBanner() {
        openPage()
        awaitTagGone("page-status-connecting")
        assertNoBanner()
    }

    /**
     * The server drops the socket and then refuses upgrades (it is down): the
     * disconnected banner stays up through every failed attempt, with Retry.
     * Once the server is back, Retry reconnects at once and the banner goes.
     */
    @Test
    fun anUnreachableServerShowsDisconnectedUntilRetryReconnects() {
        pageMode = PageMode.DropOnLease
        openPage()

        awaitTag("page-status-disconnected")
        composeRule.onNodeWithText(PageInkSession.DISCONNECTED).assertIsDisplayed()
        composeRule.onNodeWithTag("page-status-detail").assertIsDisplayed()
        // Let the backoff grow (1 + 2 + 4 s of refused attempts) so the next
        // automatic attempt is 8 s away: a reconnect inside the window below
        // can only be Retry's. A failed attempt does not flip the banner to a
        // benign "connecting" meanwhile.
        composeRule.waitUntil(timeoutMillis = 15_000) { pageUpgrades.get() >= 4 }
        composeRule.onNodeWithTag("page-status-disconnected").assertIsDisplayed()

        pageMode = PageMode.Healthy
        serverUp.set(true)
        val before = pageUpgrades.get()
        composeRule.onNodeWithTag("page-status-retry").performClick()

        composeRule.waitUntil(timeoutMillis = 2_000) { pageUpgrades.get() > before }
        awaitTagGone("page-status-disconnected", timeoutMillis = 2_000)
        assertNoBanner()
    }

    @Test
    fun anotherEditorShowsTheLeaseBannerAndNothingElse() {
        pageMode = PageMode.LeaseHeldElsewhere
        openPage()

        awaitTag("page-status-lease-held")
        composeRule.onNodeWithText(PageInkSession.LEASE_BLOCKED).assertIsDisplayed()
        // Waiting for the other editor is not something Retry fixes.
        composeRule.onAllNodesWithTag("page-status-retry").assertCountEquals(0)
        composeRule.onAllNodesWithTag("page-status-disconnected").assertCountEquals(0)
    }

    @Test
    fun aRefusedSessionAsksForSignIn() {
        pageMode = PageMode.Unauthorized
        openPage()

        awaitTag("page-status-session-expired")
        composeRule.onNodeWithText(PageInkSession.SESSION_EXPIRED).assertIsDisplayed()
        composeRule.onNodeWithTag("page-status-sign-in").assertIsDisplayed()
        composeRule.onAllNodesWithTag("page-status-disconnected").assertCountEquals(0)
        // Not clicked here: Sign in starts OIDC discovery against the real
        // identity provider, which the test network guard refuses by crashing
        // the run. The button's callback is asserted in
        // PageStatusBannerInstrumentedTest.
    }

    private fun openPage() {
        awaitTag("page-tile-$PAGE_ID")
        composeRule.onNodeWithTag("page-tile-$PAGE_ID").performClick()
    }

    private fun awaitTag(tag: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun awaitTagGone(
        tag: String,
        timeoutMillis: Long = 10_000,
    ) {
        composeRule.waitUntil(timeoutMillis = timeoutMillis) {
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isEmpty()
        }
    }

    private fun assertNoBanner() {
        for (kind in PageStatusKind.entries) {
            composeRule.onAllNodesWithTag("page-status-${kind.tag}").assertCountEquals(0)
        }
    }

    private enum class PageMode { Healthy, DropOnLease, LeaseHeldElsewhere, Unauthorized }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.url.encodedPath) {
                    "/api/me" -> json("""{"sub":"banner-test-user","email":"test@example.com"}""")
                    "/api/pages" ->
                        json(
                            """{"pages":[{"id":"$PAGE_ID","title":"Banner test",""" +
                                """"created_at":"2026-11-19T00:00:00Z","updated_at":"2026-11-19T00:00:00Z"}]}""",
                        )
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    "/api/pages/$PAGE_ID/realtime" -> pageUpgrade()
                    else -> MockResponse(code = 404)
                }
        }

    private fun pageUpgrade(): MockResponse {
        pageUpgrades.incrementAndGet()
        val mode = pageMode
        return when {
            mode == PageMode.Unauthorized -> MockResponse(code = 401)
            !serverUp.get() -> MockResponse(code = 503)
            else -> MockResponse.Builder().webSocketUpgrade(pageListener(mode)).build()
        }
    }

    private fun pageListener(mode: PageMode) =
        object : WebSocketListener() {
            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                val holder = if (mode == PageMode.LeaseHeldElsewhere) ""","lease_holder":"other-device"""" else ""
                webSocket.send("""{"type":"welcome","session_id":"banner-editor","last_seq":0$holder}""")
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                when (JSONObject(text).getString("type")) {
                    "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                    "acquire-lease" ->
                        if (mode == PageMode.DropOnLease) {
                            // Down from here on: every reconnect is refused.
                            serverUp.set(false)
                            webSocket.close(1013, "lagged")
                        } else {
                            webSocket.send("""{"type":"lease-granted"}""")
                        }
                    "release-lease" -> webSocket.close(1000, "lease released")
                }
            }

            override fun onClosing(
                webSocket: WebSocket,
                code: Int,
                reason: String,
            ) {
                webSocket.close(code, reason)
            }
        }

    // A socket the app closes must be answered, or MockWebServer's close waits
    // on the half-closed connection and fails the test in teardown.
    private fun answeringCloses() =
        object : WebSocketListener() {
            override fun onClosing(
                webSocket: WebSocket,
                code: Int,
                reason: String,
            ) {
                webSocket.close(code, reason)
            }
        }

    private fun json(body: String): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body(body)
            .addHeader("Content-Type", "application/json")
            .build()

    private companion object {
        const val PAGE_ID = "page_banner"
    }
}
