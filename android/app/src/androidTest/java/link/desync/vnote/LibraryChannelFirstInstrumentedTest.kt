package link.desync.vnote

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
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
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * #635 on device: the library opens its channel first and loads the page list
 * only once the socket is open, as the SPA does since #631.
 *
 * The fake server publishes a page created on another device right after the
 * library socket opens, and holds the `GET /api/pages` reply until the app has
 * shown that page. The reply it then sends was read before the create, so it
 * lacks the page: the worst case for the snapshot assignment, which used to
 * roll such an event back (#347). The page must survive it.
 *
 * Loading first, as before #635, fails here rather than passing slowly: the
 * held snapshot keeps the channel from ever opening, so the page never shows.
 */
@RunWith(AndroidJUnit4::class)
class LibraryChannelFirstInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    // Counted down by the test once the live page is on screen; the snapshot
    // is answered only then.
    private val releaseSnapshot = CountDownLatch(1)

    // Every path the fake server was asked for, in arrival order.
    private val requestPaths: MutableList<String> = Collections.synchronizedList(mutableListOf())

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                server = MockWebServer()
                server.dispatcher = dispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val baseUrl = server.url("/").toString().removeSuffix("/")
                DrawingToolPreferences.clear(context)
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { store, authRepository ->
                    OkHttpApiClient(baseUrl, store, authRepository).also { apiClient = it }
                }
            }

            override fun after() {
                // Never leave the dispatcher thread parked on a failed run.
                releaseSnapshot.countDown()
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                if (::server.isInitialized) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    @Test
    fun anEventPublishedBeforeTheSnapshotIsAnsweredIsInTheList() {
        // The event alone puts the page on screen: the snapshot is still held.
        awaitTile(LIVE_PAGE) { "requests so far: $requestPaths" }

        releaseSnapshot.countDown()

        // The snapshot has landed once its only page shows; the live page,
        // which it does not list, must still be there with it.
        awaitTile(SNAPSHOT_PAGE) { "requests so far: $requestPaths" }
        composeRule.onNodeWithTag("page-tile-$LIVE_PAGE", useUnmergedTree = true).assertIsDisplayed()
        // Checked only now: the server records `/api/pages` on its own thread,
        // so before the snapshot has landed the request may not be logged yet.
        val requests = requestPaths.toList()
        assertTrue(
            "the library socket opens before the list is requested: $requests",
            requests.indexOf("/api/realtime") in 0 until requests.indexOf("/api/pages"),
        )
    }

    private fun awaitTile(
        pageId: String,
        observed: () -> String,
    ) {
        val shown =
            runCatching {
                composeRule.waitUntil(timeoutMillis = 10_000) {
                    runCatching {
                        composeRule.onNodeWithTag("page-tile-$pageId", useUnmergedTree = true).assertIsDisplayed()
                        true
                    }.getOrDefault(false)
                }
            }
        check(shown.isSuccess) { "page-tile-$pageId never showed; ${observed()}" }
    }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                requestPaths.add(request.url.encodedPath)
                return when (request.url.encodedPath) {
                    "/api/me" -> json("""{"sub":"test-user","email":"test@example.com"}""")
                    "/api/pages" -> {
                        // Bounded, so a broken app fails the test instead of
                        // hanging the server thread.
                        releaseSnapshot.await(SNAPSHOT_HOLD_SECONDS, TimeUnit.SECONDS)
                        json("""{"pages":[${pageJson(SNAPSHOT_PAGE, "2026-11-27T08:00:00Z")}]}""")
                    }
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(librarySocket()).build()
                    else -> MockResponse(code = 404)
                }
            }
        }

    // The real server subscribes a library socket before it answers the
    // upgrade (#631), so a create committed just after is sent on it.
    private fun librarySocket() =
        object : WebSocketListener() {
            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                webSocket.send("""{"type":"page-created","page":${pageJson(LIVE_PAGE, "2026-11-27T09:00:00Z")}}""")
            }

            override fun onClosing(
                webSocket: WebSocket,
                code: Int,
                reason: String,
            ) {
                webSocket.close(code, reason)
            }
        }

    private fun pageJson(
        id: String,
        updatedAt: String,
    ): String = """{"id":"$id","title":"$id","created_at":"2026-11-27T07:00:00Z","updated_at":"$updatedAt"}"""

    private fun json(body: String): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body(body)
            .addHeader("Content-Type", "application/json")
            .build()

    private companion object {
        const val LIVE_PAGE = "page_live"
        const val SNAPSHOT_PAGE = "page_snapshot"
        const val SNAPSHOT_HOLD_SECONDS = 20L
    }
}
