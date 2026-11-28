package link.desync.vnote

import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.model.MeProfile
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
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress

/**
 * #159, acceptance 9, on screen: the device holds one identity's data with a
 * change still owed, and a different identity signs in. The app warns with
 * the count before anything is deleted; Cancel undoes the sign-in and keeps
 * everything, Delete and continue starts the install over for the new one.
 *
 * Driven through the session check rather than a browser sign-in (which the
 * test network guard forbids): the stored identity was signed out, the
 * tokens in the store are the new sign-in's, and `/me` says who that is.
 */
@RunWith(AndroidJUnit4::class)
class IdentitySwitchInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: ApiClient? = null
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val store get() = LocalStore.shared(context)

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                server = MockWebServer()
                server.dispatcher = dispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val serverUrl = server.url("/").toString().removeSuffix("/")
                store.identity.save(PREVIOUS)
                store.outbox.enqueue(
                    "page_previous",
                    Edit.Batch("batch_previous", listOf(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))),
                )
                store.identity.markSignedOut()
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens("new-access", "new-refresh", System.currentTimeMillis() / 1000 + 3600)
                MainActivity.apiClientFactory = { tokens, authRepository ->
                    OkHttpApiClient(serverUrl, tokens, authRepository).also { apiClient = it }
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
    fun cancelChangesNothing() {
        awaitTag("identity-switch-dialog")
        composeRule.onNodeWithText("1 unsynced change from another account will be deleted.").assertExists()

        composeRule.onNodeWithTag("identity-switch-cancel").performClick()

        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithText("Sign in").fetchSemanticsNodes().isNotEmpty()
        }
        assertEquals("the device still belongs to the previous identity", PREVIOUS.sub, store.identity.current()?.sub)
        assertEquals("its change is still owed", 1, store.outbox.read.unsyncedCount())
        assertNull("the new sign-in is undone", tokenStore.accessToken())
    }

    @Test
    fun deleteAndContinueStartsOverForTheNewIdentity() {
        awaitTag("identity-switch-dialog")

        composeRule.onNodeWithTag("identity-switch-delete").performClick()

        awaitTag("create-page-button")
        assertEquals(NEXT.sub, store.identity.current()?.sub)
        assertEquals("the previous identity's changes are gone", 0, store.outbox.read.unsyncedCount())
        assertEquals("new-access", tokenStore.accessToken())
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
                    "/api/me" -> json("""{"sub":"${NEXT.sub}","email":"${NEXT.email}"}""")
                    "/api/pages" -> json("""{"pages":[]}""")
                    "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    else -> MockResponse(code = 404)
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
        val PREVIOUS = MeProfile(sub = "previous-user", email = "previous@example.com")
        val NEXT = MeProfile(sub = "next-user", email = "next@example.com")
    }
}
