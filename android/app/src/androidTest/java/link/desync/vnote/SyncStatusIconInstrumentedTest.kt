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
 * #159 on screen: the top bar's sync status icon — on every screen — names
 * the kind of trouble (offline, sign-in needed) and says nothing while sync
 * is nominal. The open page shows no banner for any of it: only a refusal is
 * the page's to show. The session-level behaviour behind each state is in
 * [PageInkInstrumentedTest].
 */
@RunWith(AndroidJUnit4::class)
class SyncStatusIconInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    // What the page channel does on each upgrade, set per test before launch.
    @Volatile
    private var pageMode = PageMode.Healthy

    // While false, a page upgrade is answered 503: the server is down.
    private val serverUp = AtomicBoolean(true)
    private val pageUpgrades = AtomicInteger(0)

    // Set once the shared page has sent another device's batch.
    private val remoteInkSent = AtomicBoolean(false)

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
    fun aHealthyPageShowsNoBannerAndNoIcon() {
        openPage()
        // Long enough to connect, replay and settle.
        Thread.sleep(1_000)
        composeRule.waitForIdle()
        awaitNoIcon()
        assertNoBanner()
    }

    /**
     * The server drops the socket and then refuses upgrades (it is down): the
     * offline icon shows on the page, its details name the host and the
     * error class, and no banner covers the canvas. Once the server is back,
     * the page reconnects on its own and the icon goes.
     */
    @Test
    fun anUnreachableServerShowsTheOfflineIconUntilItIsBack() {
        pageMode = PageMode.DropAfterSubscribe
        openPage()

        awaitTag("sync-status-offline")
        assertNoBanner()
        composeRule.onNodeWithTag("sync-status-offline").performClick()
        composeRule.onNodeWithTag("sync-status-details").assertIsDisplayed()
        composeRule.onNodeWithTag("sync-status-host").assertIsDisplayed()
        composeRule.onNodeWithTag("sync-status-error-class").assertIsDisplayed()
        composeRule.onNodeWithTag("sync-status-last-sync").assertIsDisplayed()
        composeRule.onNodeWithText("Close").performClick()

        pageMode = PageMode.Healthy
        serverUp.set(true)
        composeRule.waitUntil(timeoutMillis = 40_000) {
            composeRule.onAllNodesWithTag("sync-status-offline").fetchSemanticsNodes().isEmpty()
        }
        assertNoBanner()
    }

    /**
     * #630: there is no edit lease, so another device inking the same page is
     * not trouble. Its ink arrives on the open page and nothing is shown.
     */
    @Test
    fun anotherDeviceInkingThePageShowsNothing() {
        pageMode = PageMode.SharedWithAnotherDevice
        openPage()

        composeRule.waitUntil(timeoutMillis = 5_000) { remoteInkSent.get() }
        // Long enough for the remote batch to be applied and anything drawn.
        Thread.sleep(500)
        composeRule.waitForIdle()
        assertNoBanner()
        awaitNoIcon()
    }

    /**
     * #159: a refused session pauses sync — the icon asks for a sign-in, with
     * a Sign in button in its details — while the page stays open for
     * editing, with no banner.
     */
    @Test
    fun aRefusedSessionAsksForSignInFromTheIcon() {
        pageMode = PageMode.Unauthorized
        openPage()

        awaitTag("sync-status-sign-in")
        assertNoBanner()
        composeRule.onNodeWithTag("sync-status-sign-in").performClick()
        composeRule.onNodeWithTag("sync-status-details").assertIsDisplayed()
        composeRule.onNodeWithText(PageInkSession.SESSION_EXPIRED).assertIsDisplayed()
        // Not clicked here: Sign in starts OIDC discovery against the real
        // identity provider, which the test network guard refuses by crashing
        // the run.
        composeRule.onAllNodesWithTag("sync-status-offline").assertCountEquals(0)
    }

    private fun awaitNoIcon() {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            listOf("offline", "sign-in", "rejected").all { state ->
                composeRule.onAllNodesWithTag("sync-status-$state").fetchSemanticsNodes().isEmpty()
            }
        }
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

    private fun assertNoBanner() {
        for (kind in PageStatusKind.entries) {
            composeRule.onAllNodesWithTag("page-status-${kind.tag}").assertCountEquals(0)
        }
    }

    private enum class PageMode { Healthy, DropAfterSubscribe, SharedWithAnotherDevice, Unauthorized }

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
                webSocket.send("""{"type":"welcome","session_id":"banner-editor","last_seq":0}""")
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                when (JSONObject(text).getString("type")) {
                    "subscribe" ->
                        if (mode == PageMode.DropAfterSubscribe) {
                            // Down from here on: every reconnect is refused.
                            serverUp.set(false)
                            webSocket.close(1013, "lagged")
                        } else {
                            webSocket.send("""{"type":"synced","last_seq":0}""")
                            if (mode == PageMode.SharedWithAnotherDevice) {
                                webSocket.send(REMOTE_BATCH)
                                remoteInkSent.set(true)
                            }
                        }
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

        // Another device's stroke, fanned out to this session.
        const val REMOTE_BATCH =
            """{"type":"stroke-batch","seq":1,"client_batch_id":"other-device-batch","strokes":[{"id":"other-stroke",""" +
                """"style":{"tool_kind":"solid_round","style_version":2,"parameters":{"color":"#1565C0",""" +
                """"width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":40.0,"y":40.0,"t":0}]}]}"""
    }
}
