package link.desync.vnote

import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress

/**
 * #150: the startup session check signs the user out only when the server
 * refuses the session. A server answering with an error keeps the stored
 * tokens, so Retry signs straight back in once it recovers.
 */
@RunWith(AndroidJUnit4::class)
class SessionStartupInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    // What `/api/me` answers: 200, 503 or 401.
    @Volatile
    private var meStatus = 503

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
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
                MainActivity.apiClientFactory = { store, _ ->
                    // The API client's 401 refresh fails, as it does for a
                    // revoked refresh token. It deliberately does not clear
                    // the tokens itself: whether they survive is up to the
                    // activity, which is what this test is about.
                    val refusingRefresh =
                        object : AuthRepository(context, AuthConfig.fromBuildConfig(), store) {
                            override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean = !force
                        }
                    OkHttpApiClient(baseUrl, store, refusingRefresh).also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                if (::server.isInitialized) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    @Test
    fun aFailingServerKeepsTheSessionAndRetrySignsBackIn() {
        awaitText("HTTP 503", substring = true)
        assertNotNull("tokens kept through a server error", tokenStore.accessToken())

        meStatus = 200
        composeRule.onNodeWithText("Retry").performClick()

        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag("create-page-button").fetchSemanticsNodes().isNotEmpty()
        }
        assertNotNull("still signed in", tokenStore.accessToken())
    }

    @Test
    fun aRefusedSessionSignsOut() {
        // The rule launched the activity against a 503; recreating it makes
        // the 401 the check that decides what is shown.
        awaitText("HTTP 503", substring = true)
        meStatus = 401
        composeRule.activityRule.scenario.recreate()

        awaitText("Session expired")
        assertNull("signed out locally", tokenStore.accessToken())
    }

    private fun awaitText(
        text: String,
        substring: Boolean = false,
    ) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithText(text, substring = substring).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.url.encodedPath) {
                    "/api/me" ->
                        if (meStatus == 200) {
                            json("""{"sub":"startup-user","email":"test@example.com"}""")
                        } else {
                            MockResponse(code = meStatus)
                        }
                    "/api/pages" -> json("""{"pages":[]}""")
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    else -> MockResponse(code = 404)
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
}
