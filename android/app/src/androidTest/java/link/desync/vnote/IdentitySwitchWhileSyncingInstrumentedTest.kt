package link.desync.vnote

import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.atomic.AtomicInteger

/**
 * #159 review: a different identity signs in while sync is running for the
 * one the device holds. The new identity's tokens are stored by then, so
 * anything still syncing would open the old identity's pages with them — and
 * Cancel would no longer change nothing. Sync must stop while the user
 * decides.
 *
 * The old identity's page cannot be reached (503, so it keeps retrying with
 * backoff); a page upgrade carrying the new identity's token is refused.
 */
@RunWith(AndroidJUnit4::class)
class IdentitySwitchWhileSyncingInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: ApiClient? = null
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val store get() = LocalStore.shared(context)
    private val pageUpgrades = AtomicInteger()
    private val upgradesWithTheNewToken = AtomicInteger()

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                server = MockWebServer()
                server.dispatcher = dispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val serverUrl = server.url("/").toString().removeSuffix("/")
                store.identity.save(PREVIOUS)
                store.library.write(listOf(PAGE))
                store.outbox.enqueue(
                    PAGE.id,
                    Edit.Batch("batch_owed", listOf(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))),
                )
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(OLD_TOKEN, "old-refresh", System.currentTimeMillis() / 1000 + 3600)
                MainActivity.apiClientFactory = { tokens, authRepository ->
                    object : ApiClient by OkHttpApiClient(serverUrl, tokens, authRepository) {
                        override suspend fun refreshSession(): Boolean = false
                    }.also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                apiClient?.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                if (::server.isInitialized) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    @Test
    fun syncStopsWhileTheSwitchIsDecidedAndCancelChangesNothing() {
        // Signed in from the device, with the owed page being retried.
        composeRule.waitUntil(timeoutMillis = 10_000) { pageUpgrades.get() >= 1 }
        val previousTokens = tokenStore.snapshot()

        // A sign-in as someone else completes: their tokens are stored first.
        tokenStore.saveTokens(NEW_TOKEN, "new-refresh", System.currentTimeMillis() / 1000 + 3600)
        composeRule.runOnUiThread { composeRule.activity.adoptIdentity(NEXT, previousTokens) }
        awaitTag("identity-switch-dialog")

        // Longer than the page's next backoff step: a session still running
        // would have reconnected by now, with the new token.
        Thread.sleep(RETRY_WINDOW_MILLIS)
        assertEquals("nothing synced with the new identity's token", 0, upgradesWithTheNewToken.get())

        composeRule.onNodeWithTag("identity-switch-cancel").performClick()

        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag("identity-switch-dialog").fetchSemanticsNodes().isEmpty()
        }
        assertEquals("the previous sign-in is back", OLD_TOKEN, tokenStore.accessToken())
        assertEquals(PREVIOUS.sub, store.identity.current()?.sub)
        assertEquals("the owed change is still owed", 1, store.outbox.read.unsyncedCount())
    }

    private fun awaitTag(tag: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.url.encodedPath) {
                    "/api/me" -> json("""{"sub":"${PREVIOUS.sub}","email":"${PREVIOUS.email}"}""")
                    "/api/pages" -> json(JSONObject().put("pages", JSONArray().put(pageJson())).toString())
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    "/api/pages/${PAGE.id}/realtime" -> {
                        pageUpgrades.incrementAndGet()
                        if (request.headers["Authorization"] == "Bearer $NEW_TOKEN") {
                            upgradesWithTheNewToken.incrementAndGet()
                            MockResponse(code = 403)
                        } else {
                            MockResponse(code = 503)
                        }
                    }
                    else -> MockResponse(code = 404)
                }
        }

    private fun pageJson(): JSONObject =
        JSONObject()
            .put("id", PAGE.id)
            .put("title", PAGE.title)
            .put("created_at", PAGE.createdAt)
            .put("updated_at", PAGE.updatedAt)
            .put("paper", "none")

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
        const val OLD_TOKEN = "old-access"
        const val NEW_TOKEN = "new-access"
        const val RETRY_WINDOW_MILLIS = 9_000L
        val PREVIOUS = MeProfile(sub = "previous-user", email = "previous@example.com")
        val NEXT = MeProfile(sub = "next-user", email = "next@example.com")
        val PAGE =
            PageSummary(
                id = "page_owed",
                title = "Owed",
                createdAt = "2026-11-28T00:00:00Z",
                updatedAt = "2026-11-28T00:00:00Z",
            )
    }
}
