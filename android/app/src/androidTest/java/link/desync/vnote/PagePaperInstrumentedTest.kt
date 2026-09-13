package link.desync.vnote

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import link.desync.vnote.ink.Paper
import link.desync.vnote.ink.PaperPreferences
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
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
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Paper on the real Compose surface: the palette, the wire message it sends,
 * the pixels it paints behind seeded ink, the lease gate, and the sticky
 * new-page default.
 */
@RunWith(AndroidJUnit4::class)
class PagePaperInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient
    private val seedDelivered = CountDownLatch(1)
    private val leaseGranted = CountDownLatch(1)
    private val mutations = LinkedBlockingQueue<JSONObject>()
    private val createRequestBodies = ConcurrentLinkedQueue<String>()
    private val requestPaths = ConcurrentLinkedQueue<String>()
    private val pageMessages = ConcurrentLinkedQueue<String>()

    /** When true the page socket denies the lease instead of granting it. */
    private val denyLease = AtomicBoolean(false)

    /** When true `set-paper` is answered with a `paper_failed` error. */
    private val failPaper = AtomicBoolean(false)

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                DrawingToolPreferences.clear(context)
                PaperPreferences.clear(context)
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
                val context = InstrumentationRegistry.getInstrumentation().targetContext
                DrawingToolPreferences.clear(context)
                PaperPreferences.clear(context)
                if (::server.isInitialized) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(environmentRule).around(composeRule)

    @Test
    fun paletteOpensSendsSetPaperAndLeavesTheDrawingToolSelected() {
        openEditor()

        composeRule.onNodeWithTag("paper-tool").performClick()
        composeRule.onNodeWithTag("paper-palette").assertIsDisplayed()
        // All seven choices are offered.
        Paper.ALL.forEach { paper ->
            composeRule.onNodeWithTag("paper-option-${paper.wireValue}").assertIsDisplayed()
        }

        composeRule.onNodeWithTag("paper-option-ruled-margin-narrow").performClick()

        val message = awaitMutation("set-paper")
        assertNotNull("set-paper sent; messages=$pageMessages", message)
        assertEquals("ruled-margin-narrow", message!!.getString("paper"))
        assertTrue(message.getString("client_mutation_id").isNotEmpty())

        // Picking paper must not disturb the active drawing tool.
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
    }

    /** Paper reaches the pixels, behind the ink — the seeded stroke stays ink-coloured. */
    @Test
    fun paperRendersBehindSeededInk() {
        openEditor()
        composeRule.onNodeWithTag("paper-tool").performClick()
        composeRule.onNodeWithTag("paper-option-squared-small").performClick()

        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching { countPaperPixels() > 0 }.getOrDefault(false)
        }
        assertTrue("paper pixels painted", countPaperPixels() > 0)

        // The seeded stroke is still green-dominant ink where it was drawn: the
        // paper colours are provably outside that classifier, and paper is drawn
        // underneath in any case.
        val pixels = composeRule.onNodeWithTag("ink-canvas").captureToImage().toPixelMap()
        val ink = pixels[SEED_ASSERTION_X, SEED_STROKE_Y.toInt()]
        assertTrue(
            "seeded ink stays ink-coloured, got $ink",
            ink.green > ink.red && ink.green > ink.blue,
        )
    }

    /** `Welcome` is authoritative, so paper set elsewhere renders as soon as the page opens. */
    @Test
    fun welcomeCarriedPaperRendersOnOpen() {
        welcomePaper = "ruled-wide"
        openEditor()
        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching { countPaperPixels() > 0 }.getOrDefault(false)
        }
        assertTrue("Welcome-carried paper is rendered", countPaperPixels() > 0)
    }

    /** Without the edit lease the control is disabled; the lease banner explains why. */
    @Test
    fun leaseDeniedDisablesThePaperControl() {
        denyLease.set(true)
        openEditorWithoutLease()
        composeRule.onNodeWithTag("paper-tool").assertIsNotEnabled()
    }

    /** An acknowledged change becomes the sticky new-page default. */
    @Test
    fun acknowledgedChangePersistsTheStickyDefault() {
        openEditor()
        composeRule.onNodeWithTag("paper-tool").performClick()
        composeRule.onNodeWithTag("paper-option-squared-large").performClick()

        assertNotNull("set-paper sent", awaitMutation("set-paper"))
        // The mock server replies with `paper-changed`; only then is the
        // preference written.
        composeRule.waitUntil(timeoutMillis = 5_000) {
            storedPaper() == Paper.SquaredLarge
        }
        assertEquals(Paper.SquaredLarge, storedPaper())
    }

    /**
     * A change the server refuses must not become the sticky default. The
     * preference is written on acknowledgement, not on dispatch, so a rejected
     * pick leaves both the canvas and the stored default untouched.
     */
    @Test
    fun rejectedChangeDoesNotPersistTheStickyDefault() {
        failPaper.set(true)
        openEditor()
        composeRule.onNodeWithTag("paper-tool").performClick()
        composeRule.onNodeWithTag("paper-option-squared-large").performClick()

        assertNotNull("set-paper sent", awaitMutation("set-paper"))
        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching { countPaperPixels() == 0 }.getOrDefault(false)
        }
        assertEquals(
            "a refused change must not become the new-page default",
            Paper.None,
            storedPaper(),
        )
    }

    /** A rejected change reverts to the last confirmed value rather than sticking. */
    @Test
    fun paperFailedRevertsTheOptimisticChange() {
        failPaper.set(true)
        openEditor()
        composeRule.onNodeWithTag("paper-tool").performClick()
        composeRule.onNodeWithTag("paper-option-squared-small").performClick()

        assertNotNull("set-paper sent", awaitMutation("set-paper"))
        // The server rejects, the session reverts to Paper.None, and no paper
        // pixels survive on the canvas.
        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching { countPaperPixels() == 0 }.getOrDefault(false)
        }
        assertEquals("optimistic paper reverted", 0, countPaperPixels())
    }

    /** A new page is born with the paper last picked on this device. */
    @Test
    fun newPagesInheritTheStickyPaperDefault() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        PaperPreferences(context, TEST_USER_ID).save(Paper.SquaredLarge)

        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                composeRule.onNodeWithTag("page-tile-page_paper").assertIsDisplayed()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithText("New page").performClick()

        composeRule.waitUntil(timeoutMillis = 10_000) { createRequestBodies.isNotEmpty() }
        val body = JSONObject(createRequestBodies.first())
        assertEquals("squared-large", body.getString("paper"))
    }

    // ---- harness --------------------------------------------------------

    /**
     * Paper pixels blend toward white preserving their channel ordering, which
     * is disjoint from the green-dominant ink classifier.
     */
    private fun storedPaper(): Paper {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        return PaperPreferences(context, TEST_USER_ID).load()
    }

    private fun countPaperPixels(): Int {
        val pixels = composeRule.onNodeWithTag("ink-canvas").captureToImage().toPixelMap()
        var count = 0
        for (y in 0 until pixels.height) {
            for (x in 0 until pixels.width) {
                val pixel = pixels[x, y]
                if (pixel == Color.White) continue
                val isRule = pixel.blue > pixel.green && pixel.green > pixel.red
                val isMargin = pixel.red > pixel.green && pixel.red > pixel.blue
                if (isRule || isMargin) count++
            }
        }
        return count
    }

    private fun openEditor() {
        openEditorWithoutLease()
        assertTrue(
            "edit lease granted; messages=$pageMessages",
            leaseGranted.await(5, TimeUnit.SECONDS),
        )
    }

    private fun openEditorWithoutLease() {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                composeRule.onNodeWithTag("page-tile-page_paper").assertIsDisplayed()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithTag("page-tile-page_paper").performClick()
        // Touch the editor through Compose before blocking on a latch: a bare
        // `await` is not a synchronization point, so a composition failure here
        // would otherwise surface only as an unexplained timeout.
        composeRule.onNodeWithTag("drawing-tool").assertIsSelected()
        assertTrue(
            "seed stroke delivered; requests=$requestPaths messages=$pageMessages",
            seedDelivered.await(5, TimeUnit.SECONDS),
        )
        composeRule.waitUntil(timeoutMillis = 5_000) {
            runCatching {
                val pixels = composeRule.onNodeWithTag("ink-canvas").captureToImage().toPixelMap()
                pixels[SEED_ASSERTION_X, SEED_STROKE_Y.toInt()] != Color.White
            }.getOrDefault(false)
        }
    }

    private fun awaitMutation(
        type: String,
        timeoutMillis: Long = 5_000,
    ): JSONObject? {
        val deadline = System.currentTimeMillis() + timeoutMillis
        while (System.currentTimeMillis() < deadline) {
            val message = mutations.poll(200, TimeUnit.MILLISECONDS) ?: continue
            if (message.getString("type") == type) {
                return message
            }
        }
        return null
    }

    private var welcomePaper: String = "none"

    private fun testDispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.url.encodedPath
                requestPaths.add("${request.method} $path")
                return when {
                    path == "/api/me" ->
                        jsonResponse("""{"sub":"$TEST_USER_ID","email":"test@example.com"}""")
                    path == "/api/pages" && request.method == "POST" -> {
                        createRequestBodies.add(request.body?.utf8().orEmpty())
                        jsonResponse(
                            """{"page":{"id":"page_new","title":"Untitled page","created_at":"2026-07-25T00:00:00Z","updated_at":"2026-07-25T00:00:00Z","paper":"squared-large"}}""",
                        )
                    }
                    path == "/api/pages" ->
                        jsonResponse(
                            """{"pages":[{"id":"page_paper","title":"Paper test","created_at":"2026-07-25T00:00:00Z","updated_at":"2026-07-25T00:00:00Z","paper":"none"}]}""",
                        )
                    path == "/api/realtime" -> librarySocketResponse()
                    path == "/api/pages/page_paper/realtime" -> pageSocketResponse()
                    path == "/api/pages/page_new/realtime" -> pageSocketResponse()
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
                        webSocket.send(
                            """{"type":"welcome","session_id":"paper-editor","last_seq":1,"paper":"$welcomePaper"}""",
                        )
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
                            "acquire-lease" ->
                                if (denyLease.get()) {
                                    webSocket.send("""{"type":"lease-denied","holder":"other-device"}""")
                                } else {
                                    webSocket.send("""{"type":"lease-granted"}""")
                                    leaseGranted.countDown()
                                }
                            "set-paper" -> {
                                mutations.offer(message)
                                if (failPaper.get()) {
                                    webSocket.send(
                                        JSONObject()
                                            .put("type", "error")
                                            .put("code", "paper_failed")
                                            .put("message", "could not persist the page paper")
                                            .put(
                                                "client_mutation_id",
                                                message.getString("client_mutation_id"),
                                            ).toString(),
                                    )
                                } else {
                                    webSocket.send(
                                        JSONObject()
                                            .put("type", "paper-changed")
                                            .put("paper", message.getString("paper"))
                                            .put("revision", 2)
                                            .toString(),
                                    )
                                }
                            }
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
        """{"type":"stroke-batch","seq":1,"client_batch_id":"seed","strokes":[""" +
            """{"id":"seed-paper","style":{"tool_kind":"solid_round","style_version":1,""" +
            """"parameters":{"color":"#006400","width":4.0,"cap_style":"round","join_style":"round"}},""" +
            """"points":[{"x":50.0,"y":$SEED_STROKE_Y,"t":0},{"x":1000.0,"y":$SEED_STROKE_Y,"t":10}]}]}"""

    companion object {
        private const val TEST_USER_ID = "paper-test-user"
        private const val SEED_STROKE_Y = 200f
        private const val SEED_ASSERTION_X = 300
    }
}

private fun jsonResponse(body: String): MockResponse =
    MockResponse
        .Builder()
        .code(200)
        .body(body)
        .addHeader("Content-Type", "application/json")
        .build()
