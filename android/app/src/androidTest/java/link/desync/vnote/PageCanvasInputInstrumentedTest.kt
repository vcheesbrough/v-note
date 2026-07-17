package link.desync.vnote

import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotSelected
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class PageCanvasInputInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient
    private val seedDelivered = CountDownLatch(1)
    private val leaseGranted = CountDownLatch(1)
    private val mutations = LinkedBlockingQueue<JSONObject>()
    private val requestPaths = ConcurrentLinkedQueue<String>()
    private val pageMessages = ConcurrentLinkedQueue<String>()

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                DrawingToolPreferences.clear(context)
                server = MockWebServer()
                server.dispatcher = testDispatcher()
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
                    ApiClient(baseUrl, store, authRepository).also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) apiClient.shutdown()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                if (::server.isInitialized) server.shutdown()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(environmentRule).around(composeRule)

    @Test
    fun explicitEraserHidesAndCommitsHitBeforeStylusLift() {
        openEditor()
        composeRule.onNodeWithTag("eraser-tool").performClick().assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 300f, FIRST_STROKE_Y, downTime = downTime)

        assertTombstoneBeforeLift("seed-first")
        sendStylus(MotionEvent.ACTION_UP, 300f, FIRST_STROKE_Y, downTime = downTime)
    }

    @Test
    fun sPenButtonMotionEventEnablesOnlyTemporaryEraserMode() {
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(
            MotionEvent.ACTION_DOWN,
            300f,
            FIRST_STROKE_Y,
            buttonState = SPEN_BUTTON_STATE,
            downTime = downTime,
        )
        assertTombstoneBeforeLift("seed-first")
        sendStylus(
            MotionEvent.ACTION_UP,
            300f,
            FIRST_STROKE_Y,
            downTime = downTime,
        )
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
        composeRule.onNodeWithTag("eraser-tool").assertIsNotSelected()

        val nextDownTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 300f, 100f, downTime = nextDownTime)
        sendStylus(MotionEvent.ACTION_MOVE, 350f, 120f, downTime = nextDownTime)
        sendStylus(MotionEvent.ACTION_UP, 400f, 140f, downTime = nextDownTime)
        assertNotNull("button release restores drawing", awaitMutation("commit-batch"))
        assertNull("button release does not keep erasing", awaitMutation("commit-tombstones", 350))

        composeRule.onNodeWithTag("eraser-tool").performClick().assertIsSelected()
        composeRule.onNodeWithTag("ink-canvas").performTouchInput {
            down(Offset(300f, SECOND_STROKE_Y - 100f))
            moveTo(Offset(300f, SECOND_STROKE_Y))
            up()
        }
        assertNull("finger contact never creates tombstones", awaitMutation("commit-tombstones", 350))
    }

    @Test
    fun sPenButtonOwnsOlderOsMouseClassifiedContactStream() {
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(
            MotionEvent.ACTION_HOVER_MOVE,
            300f,
            FIRST_STROKE_Y,
            buttonState = SPEN_BUTTON_STATE,
            downTime = downTime,
        )
        sendStylus(
            MotionEvent.ACTION_DOWN,
            300f,
            FIRST_STROKE_Y,
            toolType = MotionEvent.TOOL_TYPE_FINGER,
            source = InputDevice.SOURCE_MOUSE,
            downTime = downTime,
        )
        assertTombstoneBeforeLift("seed-first")
        sendStylus(
            MotionEvent.ACTION_UP,
            300f,
            FIRST_STROKE_Y,
            toolType = MotionEvent.TOOL_TYPE_FINGER,
            source = InputDevice.SOURCE_MOUSE,
            downTime = downTime,
        )
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
    }

    private fun openEditor() {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                composeRule.onNodeWithTag("page-tile-page_input").assertIsDisplayed()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithTag("page-tile-page_input").performClick()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
        assertTrue(
            "seed stroke delivered; requests=$requestPaths messages=$pageMessages",
            seedDelivered.await(5, TimeUnit.SECONDS),
        )
        assertTrue(
            "edit lease granted; requests=$requestPaths messages=$pageMessages",
            leaseGranted.await(5, TimeUnit.SECONDS),
        )
        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching {
                val pixels = composeRule.onNodeWithTag("ink-canvas").captureToImage().toPixelMap()
                pixels[SEED_ASSERTION_X, FIRST_STROKE_Y.toInt()] != Color.White
            }.getOrDefault(false)
        }
    }

    private fun assertTombstoneBeforeLift(expectedStrokeId: String) {
        val message = awaitMutation("commit-tombstones")
        assertNotNull("tombstone sent before ACTION_UP", message)
        val ids = message!!.getJSONArray("stroke_ids")
        assertEquals(1, ids.length())
        assertEquals(expectedStrokeId, ids.getString(0))
    }

    private fun awaitMutation(
        type: String,
        timeoutMillis: Long = 5_000,
    ): JSONObject? {
        val deadline = System.currentTimeMillis() + timeoutMillis
        while (System.currentTimeMillis() < deadline) {
            val remaining = (deadline - System.currentTimeMillis()).coerceAtLeast(1)
            val message = mutations.poll(remaining, TimeUnit.MILLISECONDS) ?: return null
            if (message.getString("type") == type) return message
        }
        return null
    }

    private fun sendStylus(
        action: Int,
        localX: Float,
        localY: Float,
        toolType: Int = MotionEvent.TOOL_TYPE_STYLUS,
        buttonState: Int = 0,
        source: Int = InputDevice.SOURCE_STYLUS,
        downTime: Long,
    ) {
        val canvas = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()
        val density = composeRule.activity.resources.displayMetrics.density
        val contentLocation = IntArray(2)
        composeRule.activity
            .findViewById<View>(android.R.id.content)
            .getLocationOnScreen(contentLocation)
        val screenX = contentLocation[0] + canvas.left.value * density + localX
        val screenY = contentLocation[1] + canvas.top.value * density + localY
        val properties =
            arrayOf(
                MotionEvent.PointerProperties().apply {
                    id = 0
                    this.toolType = toolType
                },
            )
        val coordinates =
            arrayOf(
                MotionEvent.PointerCoords().apply {
                    x = screenX
                    y = screenY
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
                buttonState,
                1f,
                1f,
                0,
                0,
                source,
                0,
            )
        val decor = composeRule.activity.window.decorView
        val decorLocation = IntArray(2)
        decor.getLocationOnScreen(decorLocation)
        event.offsetLocation(-decorLocation[0].toFloat(), -decorLocation[1].toFloat())
        composeRule.runOnUiThread {
            val consumed =
                if (
                    action == MotionEvent.ACTION_HOVER_ENTER ||
                    action == MotionEvent.ACTION_HOVER_MOVE ||
                    action == MotionEvent.ACTION_HOVER_EXIT ||
                    action == MotionEvent.ACTION_BUTTON_PRESS ||
                    action == MotionEvent.ACTION_BUTTON_RELEASE
                ) {
                    decor.dispatchGenericMotionEvent(event)
                } else {
                    decor.dispatchTouchEvent(event)
                }
            assertTrue("app window consumed stylus event", consumed)
        }
        event.recycle()
        android.os.SystemClock.sleep(16)
    }

    private fun testDispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.requestUrl?.encodedPath
                requestPaths.add(path.orEmpty())
                return when (path) {
                    "/api/me" -> inputJsonResponse("""{"sub":"input-test-user","email":"test@example.com"}""")
                    "/api/pages" ->
                        inputJsonResponse(
                            """{"pages":[{"id":"page_input","title":"Input test","created_at":"2026-07-16T00:00:00Z","updated_at":"2026-07-16T00:00:00Z"}]}""",
                        )
                    "/api/realtime" -> librarySocketResponse()
                    "/api/pages/page_input/realtime" -> pageSocketResponse()
                    else -> MockResponse().setResponseCode(404)
                }
            }
        }

    private fun librarySocketResponse(): MockResponse =
        MockResponse().withWebSocketUpgrade(
            object : WebSocketListener() {
                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(code, reason)
                }
            },
        )

    private fun pageSocketResponse(): MockResponse =
        MockResponse().withWebSocketUpgrade(
            object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: okhttp3.Response) {
                    webSocket.send("""{"type":"welcome","session_id":"input-editor","last_seq":1}""")
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    pageMessages.add(text)
                    val message = JSONObject(text)
                    when (message.getString("type")) {
                        "subscribe" -> {
                            webSocket.send(seedStrokeBatch)
                            webSocket.send("""{"type":"synced","last_seq":1}""")
                            seedDelivered.countDown()
                        }
                        "acquire-lease" -> {
                            webSocket.send("""{"type":"lease-granted"}""")
                            leaseGranted.countDown()
                        }
                        "commit-batch" -> mutations.offer(message)
                        "commit-tombstones" -> mutations.offer(message)
                        "release-lease" -> webSocket.close(1000, "lease released")
                    }
                }

                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(code, reason)
                }
            },
        )

    private val seedStrokeBatch =
        """{"type":"stroke-batch","seq":1,"client_batch_id":"seed","strokes":[${strokeJson("seed-first", FIRST_STROKE_Y)},${strokeJson("seed-second", SECOND_STROKE_Y)}]}"""

    private fun strokeJson(id: String, y: Float): String =
        """{"id":"$id","style":{"tool_kind":"solid_round","style_version":1,"parameters":{"color":"#006400","width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":50.0,"y":$y,"t":0},{"x":1000.0,"y":$y,"t":10}]}"""

    companion object {
        private const val FIRST_STROKE_Y = 200f
        private const val SECOND_STROKE_Y = 400f
        private const val SEED_ASSERTION_X = 300
        private const val SPEN_BUTTON_STATE = MotionEvent.BUTTON_STYLUS_PRIMARY
    }
}

private fun inputJsonResponse(body: String): MockResponse =
    MockResponse()
        .setResponseCode(200)
        .setBody(body)
        .addHeader("Content-Type", "application/json")
