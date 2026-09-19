package link.desync.vnote

import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PixelMap
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotSelected
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import link.desync.vnote.model.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.StrokeStyle
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
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
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CountDownLatch
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
                    OkHttpApiClient(baseUrl, store, authRepository).also { apiClient = it }
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

    @Test
    fun note9ButtonHeldActionsEraseBeforeSamsungPenUp() {
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(
            NOTE9_SPEN_ACTION_DOWN,
            300f,
            FIRST_STROKE_Y,
            buttonState = SPEN_BUTTON_STATE,
            downTime = downTime,
        )
        sendStylus(
            NOTE9_SPEN_ACTION_MOVE,
            320f,
            FIRST_STROKE_Y,
            buttonState = SPEN_BUTTON_STATE,
            downTime = downTime,
        )
        assertTombstoneBeforeLift("seed-first")
        sendStylus(
            NOTE9_SPEN_ACTION_UP,
            320f,
            FIRST_STROKE_Y,
            buttonState = SPEN_BUTTON_STATE,
            downTime = downTime,
        )
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
        composeRule.onNodeWithTag("eraser-tool").assertIsNotSelected()
    }

    @Test
    fun pressureStrokeCommitsV2StyleWithClampedPressure() {
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 260f, FIRST_STROKE_Y, pressure = 0.2f, downTime = downTime)
        sendStylus(MotionEvent.ACTION_MOVE, 320f, FIRST_STROKE_Y, pressure = 0.8f, downTime = downTime)
        // Over-range hardware pressure must clamp to 1.0 on the wire, never pass through.
        sendStylus(MotionEvent.ACTION_UP, 380f, FIRST_STROKE_Y, pressure = 1.6f, downTime = downTime)

        val commit = awaitMutation("commit-batch")
        assertNotNull("drawing commits a batch", commit)
        val stroke = commit!!.getJSONArray("strokes").getJSONObject(0)
        assertEquals(
            "authored ink uses the pressure-sensitive v2 style",
            2,
            stroke.getJSONObject("style").getInt("style_version"),
        )
        val points = stroke.getJSONArray("points")
        assertTrue("down+move+up captured", points.length() >= 3)
        for (index in 0 until points.length()) {
            val point = points.getJSONObject(index)
            assertTrue("point $index carries pressure", point.has("pressure"))
            val pressure = point.getDouble("pressure")
            assertTrue("pressure $pressure normalised to 0..1", pressure in 0.0..1.0)
        }
        assertEquals(
            "final over-range sample clamped to 1.0",
            1.0,
            points.getJSONObject(points.length() - 1).getDouble("pressure"),
            1e-9,
        )
    }

    @Test
    fun coalescedHistoricalSamplesAreCapturedInOrder() {
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 260f, FIRST_STROKE_Y, pressure = 0.3f, downTime = downTime)
        // One MOVE event batching two intermediate (coalesced) samples plus its
        // current sample — all three must land in the committed stroke.
        sendStylusMoveWithHistory(
            current = Triple(360f, FIRST_STROKE_Y, 0.9f),
            historical =
                listOf(
                    Triple(300f, FIRST_STROKE_Y, 0.5f),
                    Triple(330f, FIRST_STROKE_Y, 0.7f),
                ),
            downTime = downTime,
        )
        sendStylus(MotionEvent.ACTION_UP, 400f, FIRST_STROKE_Y, pressure = 1f, downTime = downTime)

        val commit = awaitMutation("commit-batch")
        assertNotNull("drawing commits a batch", commit)
        val points = commit!!.getJSONArray("strokes").getJSONObject(0).getJSONArray("points")
        // down + 2 coalesced + move-current + up == 5 samples.
        assertEquals("coalesced samples captured", 5, points.length())
        val pressures = (0 until points.length()).map { points.getJSONObject(it).getDouble("pressure") }
        assertEquals(listOf(0.3, 0.5, 0.7, 0.9, 1.0), pressures.map { Math.round(it * 10) / 10.0 })
    }

    @Test
    fun pressureStrokeRendersThickerAtHighPressureEnd() {
        // Widen the pen (persisted before the editor loads its preset) so the
        // low- vs high-pressure width gap is unmistakable in the rasterized ink.
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        DrawingToolPreferences(context, "input-test-user").save(
            StrokeStyle(
                styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
                parameters = SolidRoundParameters(width = 24.0),
            ),
        )
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        // Draw at y=300 — the clear band between the seed lines at y=200 and y=400.
        val strokeY = 300f
        val startX = 240f
        val endX = 470f
        val samples = 12
        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, startX, strokeY, pressure = 0.1f, downTime = downTime)
        for (index in 1 until samples) {
            val fraction = index.toFloat() / (samples - 1)
            sendStylus(
                MotionEvent.ACTION_MOVE,
                startX + (endX - startX) * fraction,
                strokeY,
                pressure = 0.1f + 0.9f * fraction,
                downTime = downTime,
            )
        }
        sendStylus(MotionEvent.ACTION_UP, endX, strokeY, pressure = 1f, downTime = downTime)
        assertNotNull("drawing commits a batch", awaitMutation("commit-batch"))

        // Wait until the committed stroke is rasterized at its high-pressure end.
        val pixels =
            composeRule.awaitInkPixels {
                greenThickness(it, (endX - 25f).toInt(), strokeY.toInt()) > 0
            }
        val low = greenThickness(pixels, (startX + 25f).toInt(), strokeY.toInt())
        val high = greenThickness(pixels, (endX - 25f).toInt(), strokeY.toInt())
        assertTrue("high-pressure end thicker than low: low=$low high=$high", high > low + 2)
    }

    // Vertical run of dark-green ink pixels through column [x], within ±30px of
    // [centerY] (kept clear of the seed lines at y=200 and y=400).
    private fun greenThickness(
        pixels: PixelMap,
        x: Int,
        centerY: Int,
    ): Int {
        if (x < 0 || x >= pixels.width) return 0
        var count = 0
        val top = (centerY - 30).coerceAtLeast(0)
        val bottom = (centerY + 30).coerceAtMost(pixels.height - 1)
        for (y in top..bottom) {
            val color = pixels[x, y]
            if (color.green > color.red && color.green > color.blue && color.alpha > 0f) {
                count += 1
            }
        }
        return count
    }

    @Test
    fun pressureTapRendersAsDot() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        DrawingToolPreferences(context, "input-test-user").save(
            StrokeStyle(
                styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
                parameters = SolidRoundParameters(width = 24.0),
            ),
        )
        openEditor()
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()

        // A tap: down + up at the same coordinate (no movement) — a v2 stroke of
        // coincident points that must render as a dot, not collapse to nothing.
        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 300f, 300f, pressure = 0.9f, downTime = downTime)
        sendStylus(MotionEvent.ACTION_UP, 300f, 300f, pressure = 0.9f, downTime = downTime)
        assertNotNull("tap commits a batch", awaitMutation("commit-batch"))

        val pixels = composeRule.awaitInkPixels { greenThickness(it, 300, 300) > 0 }
        assertTrue("v2 tap renders a visible dot", greenThickness(pixels, 300, 300) > 2)
    }

    private fun sendStylusMoveWithHistory(
        current: Triple<Float, Float, Float>,
        historical: List<Triple<Float, Float, Float>>,
        downTime: Long,
    ) {
        val canvas = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()
        val density = composeRule.activity.resources.displayMetrics.density
        val contentLocation = IntArray(2)
        composeRule.activity
            .findViewById<View>(android.R.id.content)
            .getLocationOnScreen(contentLocation)

        fun coords(sample: Triple<Float, Float, Float>): MotionEvent.PointerCoords =
            MotionEvent.PointerCoords().apply {
                x = contentLocation[0] + canvas.left.value * density + sample.first
                y = contentLocation[1] + canvas.top.value * density + sample.second
                pressure = sample.third
                size = 0.1f
            }
        val properties =
            arrayOf(
                MotionEvent.PointerProperties().apply {
                    id = 0
                    toolType = MotionEvent.TOOL_TYPE_STYLUS
                },
            )
        // Build the event on the first historical sample, then addBatch the rest
        // in order; the final addBatch sample becomes the event's current one.
        val ordered = historical + current
        val base = android.os.SystemClock.uptimeMillis()
        val event =
            MotionEvent.obtain(
                downTime,
                base,
                MotionEvent.ACTION_MOVE,
                1,
                properties,
                arrayOf(coords(ordered.first())),
                0,
                0,
                1f,
                1f,
                0,
                0,
                InputDevice.SOURCE_STYLUS,
                0,
            )
        for (index in 1 until ordered.size) {
            event.addBatch(base + index, arrayOf(coords(ordered[index])), 0)
        }
        val decor = composeRule.activity.window.decorView
        val decorLocation = IntArray(2)
        decor.getLocationOnScreen(decorLocation)
        event.offsetLocation(-decorLocation[0].toFloat(), -decorLocation[1].toFloat())
        composeRule.runOnUiThread {
            assertTrue("app window consumed stylus move", decor.dispatchTouchEvent(event))
        }
        event.recycle()
        android.os.SystemClock.sleep(16)
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
        composeRule.awaitInkPixels { hasInkAt(it, SEED_ASSERTION_X, FIRST_STROKE_Y.toInt()) }
    }

    /**
     * The in-progress stroke is drawn by its own canvas layer, above the
     * committed ink. Rasterizing it before ACTION_UP — with the committed seed
     * ink still underneath — is what proves the two layers paint independently:
     * a stylus sample reaches the screen without redrawing the whole page.
     */
    @Test
    fun liveStrokeIsRasterizedBeforeLiftWithCommittedInkIntact() {
        openEditor()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 240f, LIVE_STROKE_Y, downTime = downTime)
        for (x in 260..460 step 20) {
            sendStylus(MotionEvent.ACTION_MOVE, x.toFloat(), LIVE_STROKE_Y, downTime = downTime)
        }

        // One frame carries both halves of the claim: the live stroke reached the
        // screen while the committed seed ink was still on it.
        val pixels = composeRule.awaitInkPixels { hasInkAt(it, 360, LIVE_STROKE_Y.toInt()) }
        assertTrue("live ink painted before lift", hasInkAt(pixels, 360, LIVE_STROKE_Y.toInt()))
        assertTrue(
            "committed ink still painted",
            hasInkAt(pixels, SEED_ASSERTION_X, FIRST_STROKE_Y.toInt()),
        )

        sendStylus(MotionEvent.ACTION_UP, 460f, LIVE_STROKE_Y, downTime = downTime)
    }

    /**
     * Committed geometry is cached per stroke id, so an erase has to evict it
     * and repaint the layer. Without that the stroke would stay on screen after
     * its tombstone was accepted.
     */
    @Test
    fun erasedStrokeLeavesTheCommittedLayer() {
        openEditor()
        val seeded = composeRule.awaitInkPixels()
        assertTrue("seed painted", hasInkAt(seeded, SEED_ASSERTION_X, FIRST_STROKE_Y.toInt()))
        composeRule.onNodeWithTag("eraser-tool").performClick().assertIsSelected()

        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, 300f, FIRST_STROKE_Y, downTime = downTime)
        assertTombstoneBeforeLift("seed-first")
        sendStylus(MotionEvent.ACTION_UP, 300f, FIRST_STROKE_Y, downTime = downTime)

        val erased =
            composeRule.awaitInkPixels {
                !hasInkAt(it, SEED_ASSERTION_X, FIRST_STROKE_Y.toInt())
            }
        assertTrue(
            "erased stroke cleared from the cached layer",
            !hasInkAt(erased, SEED_ASSERTION_X, FIRST_STROKE_Y.toInt()),
        )
        assertTrue(
            "the untouched stroke is still cached and drawn",
            hasInkAt(erased, SEED_ASSERTION_X, SECOND_STROKE_Y.toInt()),
        )
    }

    // Any non-white pixel: the canvas is white and this page has no paper, so
    // colour does not matter — only whether something was drawn there.
    private fun hasInkAt(
        pixels: PixelMap,
        x: Int,
        y: Int,
    ): Boolean = pixels[x, y] != Color.White

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
        pressure: Float = 1f,
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
                    this.pressure = pressure
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
                val path = request.url.encodedPath
                requestPaths.add(path.orEmpty())
                return when (path) {
                    "/api/me" -> inputJsonResponse("""{"sub":"input-test-user","email":"test@example.com"}""")
                    "/api/pages" ->
                        inputJsonResponse(
                            """{"pages":[{"id":"page_input","title":"Input test","created_at":"2026-07-16T00:00:00Z","updated_at":"2026-07-16T00:00:00Z"}]}""",
                        )
                    "/api/realtime" -> librarySocketResponse()
                    "/api/pages/page_input/realtime" -> pageSocketResponse()
                    else -> MockResponse(code = 404)
                }
            }
        }

    private fun librarySocketResponse(): MockResponse =
        MockResponse
            .Builder()
            .webSocketUpgrade(
                object : WebSocketListener() {
                    override fun onClosing(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        webSocket.close(code, reason)
                    }
                },
            ).build()

    private fun pageSocketResponse(): MockResponse =
        MockResponse
            .Builder()
            .webSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        webSocket.send("""{"type":"welcome","session_id":"input-editor","last_seq":1}""")
                    }

                    override fun onMessage(
                        webSocket: WebSocket,
                        text: String,
                    ) {
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

                    override fun onClosing(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        webSocket.close(code, reason)
                    }
                },
            ).build()

    private val seedStrokeBatch =
        """{"type":"stroke-batch","seq":1,"client_batch_id":"seed","strokes":[${strokeJson(
            "seed-first",
            FIRST_STROKE_Y,
        )},${strokeJson("seed-second", SECOND_STROKE_Y)}]}"""

    private fun strokeJson(
        id: String,
        y: Float,
    ): String =
        """{"id":"$id","style":{"tool_kind":"solid_round","style_version":1,"parameters":{"color":"#006400","width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":50.0,"y":$y,"t":0},{"x":1000.0,"y":$y,"t":10}]}"""

    companion object {
        private const val FIRST_STROKE_Y = 200f
        private const val SECOND_STROKE_Y = 400f

        // The clear band between the two seed strokes.
        private const val LIVE_STROKE_Y = 300f
        private const val SEED_ASSERTION_X = 300
        private const val SPEN_BUTTON_STATE = MotionEvent.BUTTON_STYLUS_PRIMARY
        private const val NOTE9_SPEN_ACTION_DOWN = 211
        private const val NOTE9_SPEN_ACTION_UP = 212
        private const val NOTE9_SPEN_ACTION_MOVE = 213
    }
}

private fun inputJsonResponse(body: String): MockResponse =
    MockResponse
        .Builder()
        .code(200)
        .body(body)
        .addHeader("Content-Type", "application/json")
        .build()
