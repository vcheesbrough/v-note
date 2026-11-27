package link.desync.vnote

import android.os.Build
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import org.junit.Assert.assertNotNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress

/**
 * #150 on device: the network goes away (airplane mode) while the app is in
 * use. Every request then fails at the transport — no HTTP status, an
 * `IOException` — and the app must say so and carry on, never crash.
 *
 * Airplane mode is switched on as a user would (API 30+, where `cmd
 * connectivity` exists), but it does not cut loopback, so the test server is
 * shut down as well: that is what airplane mode does to a real server's
 * requests, a connection that cannot be made.
 */
@RunWith(AndroidJUnit4::class)
class OfflineInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient
    private var serverClosed = false

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                DrawingToolPreferences.clear(context)
                server = MockWebServer()
                server.dispatcher = dispatcher()
                // An explicit port, not 0: MockWebServer only sets SO_REUSEADDR
                // for one, and the restart in [comingBackOnlineReconnectsTheLibrary]
                // can rebind past this listener's TIME_WAIT connections only if
                // they carry it too.
                server.start(InetAddress.getByName("127.0.0.1"), java.net.ServerSocket(0).use { it.localPort })
                val serverUrl = server.url("/").toString().removeSuffix("/")
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { store, authRepository ->
                    // The identity provider is off-network as well: refreshing fails.
                    object : ApiClient by OkHttpApiClient(serverUrl, store, authRepository) {
                        override suspend fun refreshSession(): Boolean = false
                    }.also { apiClient = it }
                }
            }

            override fun after() {
                setAirplaneMode(false)
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                if (::server.isInitialized && !serverClosed) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    @Test
    fun goingOfflineOnAPageThenBackToTheLibraryDoesNotCrash() {
        openPage()

        goOffline()
        awaitTag("page-status-disconnected")

        composeRule.onNodeWithContentDescription("Back to library").performClick()
        // Back refetches the list, which can only fail now.
        awaitTag("create-page-button")
        stayAlive()
        composeRule.onNodeWithTag("create-page-button").assertExists()
    }

    @Test
    fun goingOfflineInTheLibraryThenCreatingAPageDoesNotCrash() {
        awaitTag("page-tile-$PAGE_ID")

        goOffline()
        composeRule.onNodeWithTag("create-page-button").performClick()

        stayAlive()
        composeRule.onNodeWithTag("create-page-button").assertExists()
    }

    @Test
    fun comingBackOnlineReconnectsTheLibrary() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port

        // Every reconnect now fails through the socket's `onError`, which
        // must keep retrying until the network is back.
        goOffline()
        awaitText("reach the server")
        stayAlive()

        comeBackOnline(port)

        val recovered =
            runCatching {
                composeRule.waitUntil(timeoutMillis = 30_000) { unreachableTexts().isEmpty() }
            }
        // CI keeps only the tail of the log, so a bare timeout says nothing:
        // say what the library still shows and what the server saw.
        check(recovered.isSuccess) {
            "library did not recover after coming back online: still showing ${unreachableTexts()}; " +
                "after the restart the server saw ${server.requestCount} requests: " +
                generateSequence { server.takeRequest(0, java.util.concurrent.TimeUnit.MILLISECONDS) }
                    .take(20)
                    .joinToString { it.url.encodedPath }
        }
        composeRule.onNodeWithTag("page-tile-$PAGE_ID").assertExists()
    }

    /**
     * #589: the app starts with the server unreachable. The session check
     * fails without crashing or signing out, and Retry signs straight in
     * once the server is back.
     */
    @Test
    fun startingOfflineKeepsTheSessionAndRetrySignsInOnceBack() {
        val port = server.port
        goOffline()
        // Relaunch with nothing listening on the server's address.
        composeRule.activityRule.scenario.recreate()

        awaitText("reach the server", timeoutMillis = 20_000)
        composeRule.onNodeWithText("Retry").assertExists()
        stayAlive()
        assertNotNull("an unreachable server never signs the user out", tokenStore.accessToken())

        comeBackOnline(port)
        composeRule.onNodeWithText("Retry").performClick()

        awaitTag("page-tile-$PAGE_ID")
        assertNotNull("signed in on the kept session", tokenStore.accessToken())
    }

    // Airplane mode off and the server answering again on the same address.
    private fun comeBackOnline(port: Int) {
        setAirplaneMode(false)
        server = MockWebServer()
        server.dispatcher = dispatcher()
        server.start(InetAddress.getByName("127.0.0.1"), port)
        serverClosed = false
    }

    private fun unreachableTexts(): List<String> =
        listOf("reach the server", "Failed to connect").flatMap { text ->
            composeRule
                .onAllNodesWithText(text, substring = true)
                .fetchSemanticsNodes()
                .flatMap { node ->
                    node.config
                        .getOrNull(SemanticsProperties.Text)
                        .orEmpty()
                        .map { it.text }
                }
        }

    private fun awaitText(
        text: String,
        timeoutMillis: Long = 15_000,
    ) {
        composeRule.waitUntil(timeoutMillis = timeoutMillis) {
            composeRule.onAllNodesWithText(text, substring = true).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun openPage() {
        awaitTag("page-tile-$PAGE_ID")
        composeRule.onNodeWithTag("page-tile-$PAGE_ID").performClick()
        awaitTag("drawing-tool")
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag("page-status-connecting").fetchSemanticsNodes().isEmpty()
        }
    }

    private fun goOffline() {
        setAirplaneMode(true)
        server.close()
        serverClosed = true
    }

    // A crash kills the instrumentation process, failing the test; give the
    // background work that follows a failure (reconnects, refetches) time to
    // run before declaring the app still standing.
    private fun stayAlive() {
        Thread.sleep(4_000)
        composeRule.waitForIdle()
    }

    private fun awaitTag(tag: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun setAirplaneMode(enabled: Boolean) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        val state = if (enabled) "enable" else "disable"
        InstrumentationRegistry
            .getInstrumentation()
            .uiAutomation
            .executeShellCommand("cmd connectivity airplane-mode $state")
            .close()
    }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.url.encodedPath) {
                    "/api/me" -> json("""{"sub":"offline-user","email":"test@example.com"}""")
                    "/api/pages" ->
                        json(
                            """{"pages":[{"id":"$PAGE_ID","title":"Offline test",""" +
                                """"created_at":"2026-11-25T00:00:00Z","updated_at":"2026-11-25T00:00:00Z"}]}""",
                        )
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    "/api/pages/$PAGE_ID/realtime" -> MockResponse.Builder().webSocketUpgrade(pageListener()).build()
                    else -> MockResponse(code = 404)
                }
        }

    private fun pageListener() =
        object : WebSocketListener() {
            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                webSocket.send("""{"type":"welcome","session_id":"offline-editor","last_seq":0}""")
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                when (JSONObject(text).getString("type")) {
                    "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
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
        const val PAGE_ID = "page_offline"
    }
}
