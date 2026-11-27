package link.desync.vnote

import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.junit4.ComposeTestRule
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.Collections
import java.util.concurrent.atomic.AtomicReference

/**
 * The recycle bin (#631) on a device, against a fake server that keeps the
 * pages' state the way the real one does: deleting a page moves it to the bin,
 * the bin screen lists it with its thumbnail and binning time and offers
 * Restore, Delete permanently and Empty bin, and a page binned on another
 * device while it is open here stays open with a notice and a Restore action.
 */
@RunWith(AndroidJUnit4::class)
class PageBinInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    // The fake server's pages, by id: live ones and binned ones (with their
    // `binned_at`). Seeded per test before the activity starts.
    private val live: MutableMap<String, String> = Collections.synchronizedMap(linkedMapOf())
    private val binned: MutableMap<String, Pair<String, String>> = Collections.synchronizedMap(linkedMapOf())

    // Every mutating request, as "METHOD /path", in arrival order.
    private val mutations: MutableList<String> = Collections.synchronizedList(mutableListOf())

    // Every page-channel message this device sent.
    private val pageMessages: MutableList<String> = Collections.synchronizedList(mutableListOf())
    private val librarySocket = AtomicReference<WebSocket>()
    private val pageSocket = AtomicReference<WebSocket>()

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                live["page_alpha"] = "Alpha"
                live["page_beta"] = "Beta"
                binned["page_old"] = "Old notes" to "2026-11-27T20:00:00Z"
                server = MockWebServer()
                server.dispatcher = dispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val baseUrl = server.url("/").toString().removeSuffix("/")
                tokenStore = TokenStore(InstrumentationRegistry.getInstrumentation().targetContext)
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
    fun deletingAPageMovesItToTheBinAndRestoreBringsItBack() {
        awaitTag("page-tile-page_alpha")

        composeRule.onNodeWithContentDescription("Delete Alpha").performClick()
        composeRule.onNodeWithText("Move to the bin?").assertIsDisplayed()
        composeRule.onNodeWithTag("confirm-move-to-bin").performClick()
        awaitMutation("DELETE /api/pages/page_alpha")
        awaitGone("page-tile-page_alpha")
        composeRule.onNodeWithTag("page-tile-page_beta").assertIsDisplayed()

        openBin()
        awaitTag("bin-row-page_alpha")
        composeRule.onNodeWithTag("bin-thumbnail-page_alpha", useUnmergedTree = true).assertIsDisplayed()
        composeRule.onNodeWithTag("bin-binned-at-page_alpha", useUnmergedTree = true).assertIsDisplayed()
        composeRule.onNodeWithText("Moved to the bin just now", useUnmergedTree = true).assertIsDisplayed()
        // Most recently binned first.
        assertTrue(
            "the page binned just now is listed above the older one",
            top("bin-row-page_alpha") < top("bin-row-page_old"),
        )

        composeRule.onNodeWithTag("bin-restore-page_alpha", useUnmergedTree = true).performClick()
        awaitMutation("POST /api/pages/page_alpha/restore")
        awaitGone("bin-row-page_alpha")

        composeRule.onNodeWithContentDescription("Back to library").performClick()
        awaitTag("page-tile-page_alpha")
    }

    @Test
    fun deletePermanentlyAsksFirstThenRemovesThePageForGood() {
        openBin()
        awaitTag("bin-row-page_old")

        composeRule.onNodeWithTag("bin-delete-page_old", useUnmergedTree = true).performClick()
        composeRule.onNodeWithTag("purge-dialog").assertIsDisplayed()
        composeRule.onNodeWithText("Delete permanently?").assertIsDisplayed()
        // Cancelling deletes nothing.
        composeRule.onNodeWithText("Cancel").performClick()
        composeRule.onNodeWithTag("bin-row-page_old").assertIsDisplayed()
        assertTrue(mutations.none { it.startsWith("DELETE /api/bin") })

        composeRule.onNodeWithTag("bin-delete-page_old", useUnmergedTree = true).performClick()
        composeRule.onNodeWithTag("confirm-purge").performClick()
        awaitMutation("DELETE /api/bin/page_old")
        awaitGone("bin-row-page_old")
        composeRule.onNodeWithText("The bin is empty").assertIsDisplayed()
    }

    @Test
    fun emptyBinPermanentlyDeletesEveryBinnedPage() {
        binned["page_older"] = "Older notes" to "2026-11-27T19:00:00Z"
        openBin()
        awaitTag("bin-row-page_old")
        awaitTag("bin-row-page_older")

        composeRule.onNodeWithTag("empty-bin").performClick()
        composeRule.onNodeWithText("Empty the bin?").assertIsDisplayed()
        composeRule.onNodeWithTag("confirm-purge").performClick()
        awaitMutation("DELETE /api/bin")
        awaitGone("bin-row-page_old")
        awaitGone("bin-row-page_older")
        composeRule.onNodeWithText("The bin is empty").assertIsDisplayed()
        // The live library is untouched.
        composeRule.onNodeWithContentDescription("Back to library").performClick()
        awaitTag("page-tile-page_alpha")
        awaitTag("page-tile-page_beta")
    }

    /**
     * Acceptance 2: another device bins the page this one has open. The page
     * stays open with the notice, ink drawn now is still sent — the server
     * keeps it — and Restore brings the page back and clears the notice.
     */
    @Test
    fun anOpenPageBinnedElsewhereKeepsTakingInkAndOffersRestore() {
        awaitTag("page-tile-page_alpha")
        composeRule.onNodeWithTag("page-tile-page_alpha", useUnmergedTree = true).performClick()
        composeRule.waitUntil(timeoutMillis = 10_000) { pageMessages.any { it.contains("\"subscribe\"") } }

        // Binned on another device: the server tells the library and the page.
        val binnedAt = "2026-11-27T21:00:00Z"
        moveToBin("page_alpha", binnedAt)
        librarySocket.get().send(pageBinnedEvent("page_alpha", "Alpha", binnedAt))
        pageSocket.get().send("""{"type":"page-binned","binned_at":"$binnedAt"}""")

        awaitTag("page-binned-notice")
        composeRule
            .onNodeWithText(
                "Page “Alpha” was moved to the bin on another device; your changes are kept there.",
                useUnmergedTree = true,
            ).assertIsDisplayed()
        // Still open: binning is not a close.
        composeRule.onNodeWithContentDescription("Back to library").assertIsDisplayed()

        // Ink drawn on the binned page is still committed.
        drawStroke()
        composeRule.waitUntil(timeoutMillis = 10_000) { pageMessages.any { it.contains("\"commit-batch\"") } }

        composeRule.onNodeWithTag("page-binned-restore", useUnmergedTree = true).performClick()
        awaitMutation("POST /api/pages/page_alpha/restore")
        awaitGone("page-binned-notice")
    }

    /** A device that opens (or reconnects to) a binned page is told at once. */
    @Test
    fun aPageThatIsAlreadyBinnedShowsTheNoticeOnOpen() {
        awaitTag("page-tile-page_beta")
        // Binned by another device after this one listed it.
        moveToBin("page_beta", "2026-11-27T21:00:00Z")
        composeRule.onNodeWithTag("page-tile-page_beta", useUnmergedTree = true).performClick()

        awaitTag("page-binned-notice")
        composeRule.onNodeWithTag("page-binned-restore", useUnmergedTree = true).assertIsDisplayed()
    }

    private fun openBin() {
        composeRule.onNodeWithTag("main-menu-button").performClick()
        composeRule.onNodeWithTag("open-bin").performClick()
        awaitTag("bin-screen")
    }

    private fun awaitTag(tag: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTagCount(tag) > 0
        }
    }

    private fun awaitGone(tag: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTagCount(tag) == 0
        }
    }

    private fun ComposeTestRule.onAllNodesWithTagCount(tag: String): Int =
        onAllNodes(hasTestTag(tag), useUnmergedTree = true).fetchSemanticsNodes().size

    private fun awaitMutation(mutation: String) {
        composeRule.waitUntil(timeoutMillis = 10_000) { mutation in mutations }
        assertEquals("sent once", 1, mutations.count { it == mutation })
    }

    private fun top(tag: String): Float =
        composeRule
            .onNodeWithTag(tag)
            .getUnclippedBoundsInRoot()
            .top.value

    // A short pen stroke across the canvas, dispatched as the S Pen would.
    private fun drawStroke() {
        composeRule.waitForIdle()
        val downTime = android.os.SystemClock.uptimeMillis()
        listOf(MotionEvent.ACTION_DOWN to 200f, MotionEvent.ACTION_MOVE to 300f, MotionEvent.ACTION_UP to 400f)
            .forEach { (action, x) -> sendStylus(action, x, 300f, downTime) }
    }

    private fun sendStylus(
        action: Int,
        localX: Float,
        localY: Float,
        downTime: Long,
    ) {
        val canvas = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()
        val density = composeRule.activity.resources.displayMetrics.density
        val content = IntArray(2)
        composeRule.activity
            .findViewById<View>(android.R.id.content)
            .getLocationOnScreen(content)
        val event =
            MotionEvent.obtain(
                downTime,
                android.os.SystemClock.uptimeMillis(),
                action,
                1,
                arrayOf(
                    MotionEvent.PointerProperties().apply {
                        id = 0
                        toolType = MotionEvent.TOOL_TYPE_STYLUS
                    },
                ),
                arrayOf(
                    MotionEvent.PointerCoords().apply {
                        x = content[0] + canvas.left.value * density + localX
                        y = content[1] + canvas.top.value * density + localY
                        pressure = 1f
                        size = 0.1f
                    },
                ),
                0,
                0,
                1f,
                1f,
                0,
                0,
                InputDevice.SOURCE_STYLUS,
                0,
            )
        val decor = composeRule.activity.window.decorView
        val decorLocation = IntArray(2)
        decor.getLocationOnScreen(decorLocation)
        event.offsetLocation(-decorLocation[0].toFloat(), -decorLocation[1].toFloat())
        composeRule.runOnUiThread { decor.dispatchTouchEvent(event) }
        event.recycle()
        android.os.SystemClock.sleep(16)
    }

    private fun moveToBin(
        pageId: String,
        binnedAt: String,
    ) {
        live.remove(pageId)?.let { title -> binned[pageId] = title to binnedAt }
    }

    private fun pageJson(
        id: String,
        title: String,
        binnedAt: String? = null,
    ): String {
        val bin = binnedAt?.let { ""","binned_at":"$it"""" } ?: ""
        return """{"id":"$id","title":"$title","created_at":"2026-11-27T07:00:00Z",""" +
            """"updated_at":"2026-11-27T08:00:00Z","thumbnail":{"status":"empty"}$bin}"""
    }

    private fun pageBinnedEvent(
        id: String,
        title: String,
        binnedAt: String,
    ): String = """{"type":"page-binned","page":${pageJson(id, title, binnedAt)}}"""

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.url.encodedPath
                val method = request.method ?: "GET"
                if (method != "GET") mutations.add("$method $path")
                return when {
                    path == "/api/me" -> json("""{"sub":"test-user","email":"test@example.com"}""")
                    path == "/api/pages" && method == "GET" ->
                        json(pageList(live.entries.map { (id, title) -> pageJson(id, title) }))
                    path == "/api/bin" && method == "GET" ->
                        json(
                            pageList(
                                binned.entries
                                    .sortedByDescending { it.value.second }
                                    .map { (id, entry) -> pageJson(id, entry.first, entry.second) },
                            ),
                        )
                    path == "/api/bin" && method == "DELETE" -> {
                        binned.clear()
                        MockResponse(code = 204)
                    }
                    path.startsWith("/api/bin/") && method == "DELETE" -> {
                        binned.remove(path.removePrefix("/api/bin/"))
                        MockResponse(code = 204)
                    }
                    path.endsWith("/restore") && method == "POST" -> restore(path.removePrefix("/api/pages/").removeSuffix("/restore"))
                    path.startsWith("/api/pages/") && path.endsWith("/realtime") -> pageSocketResponse(path)
                    path.startsWith("/api/pages/") && method == "DELETE" -> {
                        val id = path.removePrefix("/api/pages/")
                        moveToBin(
                            id,
                            java.time.Instant
                                .now()
                                .toString(),
                        )
                        binned[id]?.let { (title, at) -> librarySocket.get()?.send(pageBinnedEvent(id, title, at)) }
                        MockResponse(code = 204)
                    }
                    path == "/api/realtime" -> librarySocketResponse()
                    else -> MockResponse(code = 404)
                }
            }
        }

    private fun restore(id: String): MockResponse {
        val title = binned.remove(id)?.first ?: live[id] ?: return MockResponse(code = 403)
        live[id] = title
        librarySocket.get()?.send("""{"type":"page-restored","page":${pageJson(id, title)}}""")
        pageSocket.get()?.send("""{"type":"page-restored"}""")
        return json("""{"page":${pageJson(id, title)}}""")
    }

    private fun librarySocketResponse(): MockResponse =
        MockResponse
            .Builder()
            .webSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        librarySocket.set(webSocket)
                    }

                    override fun onClosing(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        webSocket.close(code, reason)
                    }
                },
            ).build()

    // The page channel: welcome, then — as the real server does — a
    // `page-binned` straight after it when the page is in the bin; a subscribe
    // is caught up at once, and every commit is echoed.
    private fun pageSocketResponse(path: String): MockResponse {
        val pageId = path.removePrefix("/api/pages/").removeSuffix("/realtime")
        return MockResponse
            .Builder()
            .webSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        pageSocket.set(webSocket)
                        webSocket.send("""{"type":"welcome","session_id":"device-a","last_seq":0}""")
                        binned[pageId]?.let { (_, at) -> webSocket.send("""{"type":"page-binned","binned_at":"$at"}""") }
                    }

                    override fun onMessage(
                        webSocket: WebSocket,
                        text: String,
                    ) {
                        pageMessages.add(text)
                        val message = JSONObject(text)
                        when (message.getString("type")) {
                            "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                            "commit-batch" ->
                                webSocket.send(
                                    JSONObject()
                                        .put("type", "stroke-batch")
                                        .put("seq", 1)
                                        .put("client_batch_id", message.getString("client_batch_id"))
                                        .put("strokes", message.getJSONArray("strokes"))
                                        .toString(),
                                )
                        }
                    }

                    override fun onClosing(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        webSocket.close(code, reason)
                    }
                },
            ).build()
    }

    private fun pageList(pages: List<String>): String = """{"pages":[${pages.joinToString(",")}]}"""

    private fun json(body: String): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body(body)
            .addHeader("Content-Type", "application/json")
            .build()
}
