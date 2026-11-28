package link.desync.vnote

import android.os.Build
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.compose.ui.graphics.PixelMap
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
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
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

/**
 * #159 end to end on the device: the app is local-first. It opens offline
 * from the stored identity, keeps every edit through a restart, and catches
 * up with the server on its own once it is back — each edit stored exactly
 * once, however often it was sent. A refusal is offered as a new page.
 *
 * "Offline" is the server shut down (and airplane mode on, API 30+), as in
 * [OfflineInstrumentedTest]; "back" is a new server on the same port with the
 * same canonical state, which lives in this class and outlives each server.
 */
@RunWith(AndroidJUnit4::class)
class OfflineReplayInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: ApiClient? = null
    private var serverClosed = false
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val store get() = LocalStore.shared(context)

    // The server's canonical state: per page, its batches in seq order.
    private val batches = ConcurrentHashMap<String, CopyOnWriteArrayList<JSONObject>>()
    private val commitsReceived = ConcurrentHashMap<String, AtomicInteger>()
    private val created = CopyOnWriteArrayList<String>()
    private val purged = ConcurrentHashMap.newKeySet<String>()

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                DrawingToolPreferences.clear(context)
                batches[PAGE_ID] = CopyOnWriteArrayList()
                startServer(java.net.ServerSocket(0).use { it.localPort })
                val serverUrl = server.url("/").toString().removeSuffix("/")
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { tokens, authRepository ->
                    object : ApiClient by OkHttpApiClient(serverUrl, tokens, authRepository) {
                        override suspend fun refreshSession(): Boolean = false
                    }.also { apiClient = it }
                }
            }

            override fun after() {
                setAirplaneMode(false)
                MainActivity.apiClientFactory = null
                apiClient?.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(context)
                if (::server.isInitialized && !serverClosed) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    /**
     * Acceptance 2, 11: with no network at all, a cold start opens straight
     * into the library from the stored identity — no error screen, no Retry.
     * Existing pages open and take ink, new pages can be made, and the status
     * icon on every screen says offline with what is waiting.
     */
    @Test
    fun aColdStartWithNoNetworkOpensTheLibraryAndEverythingStillWorks() {
        awaitTag("page-tile-$PAGE_ID")
        goOffline()
        composeRule.activityRule.scenario.recreate()

        awaitTag("page-tile-$PAGE_ID")
        assertEquals("no error screen", 0, composeRule.onAllNodesWithText("Retry").fetchSemanticsNodes().size)
        awaitTag("sync-status-offline")

        openPage(PAGE_ID)
        awaitTag("sync-status-offline")
        drawStroke()
        awaitOwed(1)
        leavePage()

        composeRule.onNodeWithTag("create-page-button").performClick()
        awaitTag("ink-canvas")
        drawStroke()
        leavePage()

        assertEquals("one page made here", 1, store.library.localOnlyIds().size)
        awaitOwed(3)
        composeRule.onNodeWithTag("sync-status-offline").performClick()
        awaitTag("sync-status-details")
        awaitTag("sync-status-pending")
        composeRule.onNodeWithTag("sync-status-host").assertExists()
        composeRule.onNodeWithTag("sync-status-error-class").assertExists()
        composeRule.onNodeWithTag("sync-status-last-sync").assertExists()
    }

    /**
     * Acceptance 1, 3, 15: a stroke drawn offline is on disk before it is
     * drawn, survives the app restarting, and is replayed on its own once the
     * server is back — stored exactly once, and not sent again after a
     * further restart.
     */
    @Test
    fun offlineInkSurvivesARestartAndIsStoredExactlyOnceWhenBack() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        openPage(PAGE_ID)
        goOffline()
        drawStroke()
        awaitOwed(1)
        val batchId =
            store.outbox.read
                .forPage(PAGE_ID)
                .single()
                .opId
        assertEquals(
            "on disk, for the next process",
            1,
            LocalStore(context, "vnote-local.db").outbox.read.awaitingCount(),
        )
        leavePage()

        composeRule.activityRule.scenario.recreate()
        openPage(PAGE_ID)
        val pixels = composeRule.awaitInkPixels { hasInkNear(it, STROKE_MID_X, STROKE_Y.toInt()) }
        assertTrue("the stroke is drawn after the restart", hasInkNear(pixels, STROKE_MID_X, STROKE_Y.toInt()))
        leavePage()

        comeBackOnline(port)
        awaitOwed(0, timeoutMillis = 45_000)
        assertEquals("stored once", listOf(batchId), batches.getValue(PAGE_ID).map { it.getString("client_batch_id") })

        val sent = commitsReceived.getValue(batchId).get()
        composeRule.activityRule.scenario.recreate()
        awaitTag("page-tile-$PAGE_ID")
        Thread.sleep(3_000)
        assertEquals("not sent again once acknowledged", sent, commitsReceived.getValue(batchId).get())
        assertEquals("still stored once", 1, batches.getValue(PAGE_ID).size)
    }

    /** A page made offline is created on the server under its own id, then its ink follows. */
    @Test
    fun aPageMadeOfflineIsCreatedUnderItsOwnIdWhenBack() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        goOffline()
        composeRule.onNodeWithTag("create-page-button").performClick()
        awaitTag("ink-canvas")
        drawStroke()
        leavePage()
        val localId = store.library.localOnlyIds().single()

        comeBackOnline(port)
        awaitOwed(0, timeoutMillis = 45_000)

        assertEquals("created once, under the device's id", listOf(localId), created.toList())
        assertEquals("its stroke stored once", 1, batches.getValue(localId).size)
        assertTrue("no longer only on the device", store.library.localOnlyIds().isEmpty())
    }

    /**
     * Acceptance 4, 6, 15: edits to a page permanently deleted elsewhere are
     * refused on replay. The refusal is shown, never counted as synced, and
     * offers the strokes as a new page — which then syncs like any other.
     */
    @Test
    fun editsToAPageDeletedElsewhereAreOfferedAsANewPage() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        openPage(PAGE_ID)
        goOffline()
        drawStroke()
        awaitOwed(1)
        leavePage()

        purged += PAGE_ID
        comeBackOnline(port)
        awaitTag("sync-status-rejected", timeoutMillis = 45_000)
        assertEquals(
            "refused, not synced",
            1,
            store.outbox.read
                .rejections()
                .size,
        )
        assertEquals("nothing stored on the deleted page", 0, batches.getValue(PAGE_ID).size)

        composeRule.onNodeWithTag("sync-status-rejected").performClick()
        awaitTag("sync-rejection-$PAGE_ID")
        composeRule.onNodeWithTag("sync-rejection-keep").performClick()
        awaitTag("ink-canvas")

        awaitOwed(0, timeoutMillis = 45_000)
        assertTrue(
            "the refusal is settled",
            store.outbox.read
                .rejections()
                .isEmpty(),
        )
        val newPage = created.single()
        assertEquals("the stroke landed on the new page", 1, batches.getValue(newPage).size)
    }

    /** Discard is the other way out of a refusal: the edits go, and nothing is created. */
    @Test
    fun discardingARefusalLetsItsEditsGo() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        openPage(PAGE_ID)
        goOffline()
        drawStroke()
        awaitOwed(1)
        leavePage()

        purged += PAGE_ID
        comeBackOnline(port)
        awaitTag("sync-status-rejected", timeoutMillis = 45_000)
        composeRule.onNodeWithTag("sync-status-rejected").performClick()
        awaitTag("sync-rejection-$PAGE_ID")
        composeRule.onNodeWithTag("sync-rejection-discard").performClick()

        awaitOwed(0)
        assertTrue(
            "nothing refused",
            store.outbox.read
                .rejections()
                .isEmpty(),
        )
        assertTrue("no page created", created.isEmpty())
        composeRule.waitUntil(timeoutMillis = 10_000) {
            composeRule.onAllNodesWithTag("sync-status-rejected").fetchSemanticsNodes().isEmpty()
        }
    }

    private fun awaitOwed(
        count: Int,
        timeoutMillis: Long = 10_000,
    ) {
        val reached =
            runCatching {
                composeRule.waitUntil(timeoutMillis) { store.outbox.read.unsyncedCount() == count }
            }
        check(reached.isSuccess) {
            "expected $count owed, found ${store.outbox.read.unsyncedCount()}: " +
                store.outbox.read
                    .forPage(PAGE_ID)
                    .map { "${it.kind}/${it.state}" } +
                " rejections=${store.outbox.read.rejections()} created=$created"
        }
    }

    private fun openPage(pageId: String) {
        awaitTag("page-tile-$pageId")
        composeRule.onNodeWithTag("page-tile-$pageId").performClick()
        awaitTag("ink-canvas")
    }

    private fun leavePage() {
        composeRule.onNodeWithContentDescription("Back to library").performClick()
        awaitTag("create-page-button")
    }

    private fun drawStroke() {
        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, STROKE_START_X, downTime)
        for (x in (STROKE_START_X.toInt() + 20)..STROKE_END_X.toInt() step 20) {
            sendStylus(MotionEvent.ACTION_MOVE, x.toFloat(), downTime)
        }
        sendStylus(MotionEvent.ACTION_UP, STROKE_END_X, downTime)
        composeRule.waitForIdle()
    }

    private fun sendStylus(
        action: Int,
        localX: Float,
        downTime: Long,
    ) {
        val canvas = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()
        val density = composeRule.activity.resources.displayMetrics.density
        val contentLocation = IntArray(2)
        composeRule.activity
            .findViewById<View>(android.R.id.content)
            .getLocationOnScreen(contentLocation)
        val properties = arrayOf(MotionEvent.PointerProperties().apply { toolType = MotionEvent.TOOL_TYPE_STYLUS })
        val coordinates =
            arrayOf(
                MotionEvent.PointerCoords().apply {
                    x = contentLocation[0] + canvas.left.value * density + localX
                    y = contentLocation[1] + canvas.top.value * density + STROKE_Y
                    pressure = 1f
                    size = 0.1f
                },
            )
        val event =
            MotionEvent.obtain(
                downTime,
                android.os.SystemClock.uptimeMillis(),
                action,
                1,
                properties,
                coordinates,
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
    }

    // The default pen is dark green; anything green-dominant near the point is ink.
    private fun hasInkNear(
        pixels: PixelMap,
        x: Int,
        y: Int,
    ): Boolean =
        (-INK_SEARCH..INK_SEARCH).any { dx ->
            (-INK_SEARCH..INK_SEARCH).any { dy ->
                val px = x + dx
                val py = y + dy
                px in 0 until pixels.width &&
                    py in 0 until pixels.height &&
                    pixels[px, py].let { it.green > it.red + 0.15f && it.green > it.blue + 0.15f }
            }
        }

    private fun awaitTag(
        tag: String,
        timeoutMillis: Long = 10_000,
    ) {
        composeRule.waitUntil(timeoutMillis) {
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun goOffline() {
        setAirplaneMode(true)
        server.close()
        serverClosed = true
    }

    private fun comeBackOnline(port: Int) {
        setAirplaneMode(false)
        startServer(port)
    }

    // An explicit port, so "back" is the same address; MockWebServer sets
    // SO_REUSEADDR only for one (see [OfflineInstrumentedTest]).
    private fun startServer(port: Int) {
        server = MockWebServer()
        server.dispatcher = dispatcher()
        server.start(InetAddress.getByName("127.0.0.1"), port)
        serverClosed = false
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
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.url.encodedPath
                return when {
                    path == "/api/me" -> json("""{"sub":"replay-user","email":"test@example.com"}""")
                    path == "/api/pages" && request.method == "POST" -> create(JSONObject(request.body?.utf8().orEmpty()))
                    path == "/api/pages" -> json(JSONObject().put("pages", JSONArray().put(pageJson(PAGE_ID))).toString())
                    path == "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(answeringCloses()).build()
                    path.startsWith("/api/pages/") && path.endsWith("/realtime") -> {
                        val pageId = path.removePrefix("/api/pages/").removeSuffix("/realtime")
                        if (pageId in purged) {
                            MockResponse(code = 410)
                        } else {
                            MockResponse.Builder().webSocketUpgrade(pageChannel(pageId)).build()
                        }
                    }
                    else -> MockResponse(code = 404)
                }
            }
        }

    private fun create(body: JSONObject): MockResponse {
        val id = body.getString("id")
        if (id !in created) created += id
        batches.putIfAbsent(id, CopyOnWriteArrayList())
        return MockResponse
            .Builder()
            .code(201)
            .body(JSONObject().put("page", pageJson(id)).toString())
            .addHeader("Content-Type", "application/json")
            .build()
    }

    // The server side of one page's channel: replays what it holds after the
    // client's cursor, and stores each batch once, echoing every commit.
    private fun pageChannel(pageId: String) =
        object : WebSocketListener() {
            private val held get() = batches.getOrPut(pageId) { CopyOnWriteArrayList() }

            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                webSocket.send(
                    JSONObject()
                        .put("type", "welcome")
                        .put("session_id", "s")
                        .put("last_seq", held.size)
                        .toString(),
                )
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                val message = JSONObject(text)
                when (message.getString("type")) {
                    "subscribe" -> {
                        val from = message.getLong("from_seq")
                        held.filter { it.getLong("seq") > from }.forEach { webSocket.send(it.toString()) }
                        webSocket.send(JSONObject().put("type", "synced").put("last_seq", held.size).toString())
                    }
                    "commit-batch" -> {
                        val id = message.getString("client_batch_id")
                        commitsReceived.getOrPut(id) { AtomicInteger() }.incrementAndGet()
                        val stored =
                            synchronized(held) {
                                held.firstOrNull { it.getString("client_batch_id") == id }
                                    ?: JSONObject()
                                        .put("type", "stroke-batch")
                                        .put("seq", held.size + 1)
                                        .put("client_batch_id", id)
                                        .put("strokes", message.getJSONArray("strokes"))
                                        .also { held += it }
                            }
                        webSocket.send(stored.toString())
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

    private fun pageJson(id: String): JSONObject =
        JSONObject()
            .put("id", id)
            .put("title", if (id == PAGE_ID) "Replay test" else "")
            .put("created_at", "2026-11-28T00:00:00Z")
            .put("updated_at", "2026-11-28T00:00:00Z")
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
        const val PAGE_ID = "page_replay"
        const val STROKE_Y = 300f
        const val STROKE_START_X = 240f
        const val STROKE_END_X = 440f
        const val STROKE_MID_X = 340
        const val INK_SEARCH = 6
    }
}
