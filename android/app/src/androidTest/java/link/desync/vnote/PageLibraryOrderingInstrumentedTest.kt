package link.desync.vnote

import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotSelected
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.unit.DpRect
import androidx.compose.ui.unit.dp
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
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

@RunWith(AndroidJUnit4::class)
class PageLibraryOrderingInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient
    private val edited = AtomicBoolean(false)
    private val pageListRequests = AtomicInteger(0)
    private val librarySocket = AtomicReference<WebSocket>()
    private val leaseGranted = CountDownLatch(1)

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                server = MockWebServer()
                server.dispatcher = testDispatcher()
                server.start(InetAddress.getByName("127.0.0.1"), 0)
                val baseUrl = server.url("/").toString().removeSuffix("/")

                tokenStore =
                    TokenStore(InstrumentationRegistry.getInstrumentation().targetContext)
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { store, authRepository ->
                    ApiClient(
                        baseUrl,
                        store,
                        authRepository,
                    ).also { apiClient = it }
                }
            }

            override fun after() {
                MainActivity.apiClientFactory = null
                if (::apiClient.isInitialized) {
                    apiClient.shutdown()
                }
                if (::tokenStore.isInitialized) {
                    tokenStore.clear()
                }
                DrawingToolPreferences.clear(InstrumentationRegistry.getInstrumentation().targetContext)
                if (::server.isInitialized) {
                    server.shutdown()
                }
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(environmentRule).around(composeRule)

    @Test
    fun editedPageMovesToTopWhenReturningToLibrary() {
        composeRule.waitUntil(timeoutMillis = 10_000) {
            pageListRequests.get() >= 1 && pageIsBefore("page_new", "page_old")
        }

        composeRule.onNodeWithTag("page-tile-page_old", useUnmergedTree = true).performClick()
        composeRule.onNodeWithContentDescription("Back to pages").assertIsDisplayed()

        composeRule.waitUntil(timeoutMillis = 5_000) { librarySocket.get() != null }
        edited.set(true)
        assertTrue(
            "edit event sent",
            librarySocket.get()?.send(
                """{"type":"page-thumbnail-updated","page_id":"page_old","thumbnail":{"status":"generating","source_seq":1}}""",
            ) == true,
        )

        composeRule.onNodeWithContentDescription("Back to pages").performClick()
        composeRule.waitUntil(timeoutMillis = 10_000) {
            pageListRequests.get() >= 2 && pageIsBefore("page_old", "page_new")
        }
        assertTrue("edited page is first", pageIsBefore("page_old", "page_new"))
    }

    @Test
    fun pageEditorExposesDrawingEraserAndInteractivePalette() {
        composeRule.onNodeWithTag("page-tile-page_old", useUnmergedTree = true).performClick()

        composeRule.onNodeWithTag("drawing-tool").assertIsDisplayed().assertIsSelected()
        composeRule.onNodeWithTag("eraser-tool").assertIsDisplayed().assertIsNotSelected()
        assertTrue("editor lease granted", leaseGranted.await(5, TimeUnit.SECONDS))
        composeRule.waitForIdle()
        composeRule.onNodeWithTag("tool-palette").assertDoesNotExist()
        assertEquals(48.dp, composeRule.onNodeWithTag("drawing-tool").getUnclippedBoundsInRoot().width())
        assertEquals(48.dp, composeRule.onNodeWithTag("eraser-tool").getUnclippedBoundsInRoot().width())
        val canvasBeforePalette = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()

        composeRule.onNodeWithTag("drawing-tool").performClick()
        composeRule.onNodeWithTag("tool-palette").assertIsDisplayed()
        assertEquals(canvasBeforePalette, composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot())
        composeRule.onNodeWithTag("tool-width-slider").assertIsDisplayed()
        composeRule.onNodeWithTag("tool-width-value").assertTextEquals("4.0")
        listOf(
            "#000000",
            "#4B5563",
            "#006400",
            "#00796B",
            "#1565C0",
            "#6A1B9A",
            "#C62828",
            "#EF6C00",
        ).forEach { color -> composeRule.onNodeWithTag("swatch-$color").assertIsDisplayed() }
        assertEquals(48.dp, composeRule.onNodeWithTag("swatch-#006400").getUnclippedBoundsInRoot().width())

        composeRule.onNodeWithTag("swatch-#C62828").performClick().assertIsSelected()
        composeRule.onNodeWithTag("tool-palette").assertIsDisplayed()
        composeRule
            .onNodeWithTag("tool-width-slider")
            .performSemanticsAction(SemanticsActions.SetProgress) { setProgress ->
                setProgress(8.5f)
            }
        composeRule.onNodeWithTag("tool-width-value").assertTextEquals("8.5")

        composeRule.onNodeWithTag("eraser-tool").performClick().assertIsSelected()
        composeRule.onNodeWithTag("drawing-tool").assertIsNotSelected()
        composeRule.onNodeWithTag("tool-palette").assertDoesNotExist()

        // Returning from eraser selects drawing; it takes a second tap to reopen the palette.
        composeRule.onNodeWithTag("drawing-tool").performClick().assertIsSelected()
        composeRule.onNodeWithTag("tool-palette").assertDoesNotExist()
        composeRule.onNodeWithTag("drawing-tool").performClick()
        composeRule.onNodeWithTag("tool-palette").assertIsDisplayed()
    }

    @Test
    fun drawingSettingsPersistAcrossPagesAndActivityRecreation() {
        composeRule.onNodeWithTag("page-tile-page_old", useUnmergedTree = true).performClick()
        composeRule.onNodeWithTag("drawing-tool").performClick()
        composeRule.onNodeWithTag("swatch-#C62828").performClick().assertIsSelected()
        composeRule
            .onNodeWithTag("tool-width-slider")
            .performSemanticsAction(SemanticsActions.SetProgress) { setProgress ->
                setProgress(8.5f)
            }
        assertSelectedStyle("#C62828", "8.5")

        closePaletteAndReturnToLibrary()
        composeRule.onNodeWithTag("page-tile-page_old", useUnmergedTree = true).performClick()
        openPaletteAndAssertSelectedStyle("#C62828", "8.5")

        closePaletteAndReturnToLibrary()
        composeRule.onNodeWithTag("page-tile-page_new", useUnmergedTree = true).performClick()
        openPaletteAndAssertSelectedStyle("#C62828", "8.5")

        closePaletteAndReturnToLibrary()
        composeRule.activityRule.scenario.recreate()
        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                composeRule
                    .onNodeWithTag("page-tile-page_old", useUnmergedTree = true)
                    .assertIsDisplayed()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithTag("page-tile-page_old", useUnmergedTree = true).performClick()
        openPaletteAndAssertSelectedStyle("#C62828", "8.5")
    }

    private fun openPaletteAndAssertSelectedStyle(
        color: String,
        width: String,
    ) {
        composeRule.onNodeWithTag("drawing-tool").performClick()
        assertSelectedStyle(color, width)
    }

    private fun assertSelectedStyle(
        color: String,
        width: String,
    ) {
        composeRule.onNodeWithTag("tool-palette").assertIsDisplayed()
        composeRule.onNodeWithTag("swatch-$color").assertIsSelected()
        composeRule.onNodeWithTag("tool-width-value").assertTextEquals(width)
    }

    private fun closePaletteAndReturnToLibrary() {
        composeRule.onNodeWithTag("drawing-tool").performClick()
        composeRule.onNodeWithTag("tool-palette").assertDoesNotExist()
        composeRule.onNodeWithContentDescription("Back to pages").performClick()
    }

    private fun pageIsBefore(
        firstId: String,
        secondId: String,
    ): Boolean =
        runCatching {
            val first = composeRule.onNodeWithTag("page-tile-$firstId").getUnclippedBoundsInRoot()
            val second = composeRule.onNodeWithTag("page-tile-$secondId").getUnclippedBoundsInRoot()
            first.precedes(second)
        }.getOrDefault(false)

    private fun testDispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse =
                when (request.requestUrl?.encodedPath) {
                    "/api/me" -> jsonResponse("""{"sub":"test-user","email":"test@example.com"}""")
                    "/api/pages" -> {
                        pageListRequests.incrementAndGet()
                        jsonResponse(pageListJson())
                    }
                    "/api/realtime" ->
                        MockResponse().withWebSocketUpgrade(
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
                        )
                    "/api/pages/page_old/realtime",
                    "/api/pages/page_new/realtime",
                    -> pageSocketResponse()
                    else -> MockResponse().setResponseCode(404)
                }
        }

    private fun pageSocketResponse(): MockResponse =
        MockResponse().withWebSocketUpgrade(
            object : WebSocketListener() {
                override fun onOpen(
                    webSocket: WebSocket,
                    response: okhttp3.Response,
                ) {
                    webSocket.send("""{"type":"welcome","session_id":"editor","last_seq":0}""")
                }

                override fun onMessage(
                    webSocket: WebSocket,
                    text: String,
                ) {
                    when {
                        text.contains("\"type\":\"subscribe\"") ->
                            webSocket.send("""{"type":"synced","last_seq":0}""")
                        text.contains("\"type\":\"acquire-lease\"") -> {
                            webSocket.send("""{"type":"lease-granted"}""")
                            leaseGranted.countDown()
                        }
                        text.contains("\"type\":\"release-lease\"") ->
                            webSocket.close(1000, "lease released")
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
        )

    private fun pageListJson(): String {
        val newer = pageJson("page_new", "Newer", "2026-07-15T09:00:00Z")
        val olderTimestamp = if (edited.get()) "2026-07-15T10:00:00Z" else "2026-07-15T08:00:00Z"
        val older = pageJson("page_old", "Older", olderTimestamp)
        val pages = if (edited.get()) "$older,$newer" else "$newer,$older"
        return """{"pages":[$pages]}"""
    }

    private fun pageJson(
        id: String,
        title: String,
        updatedAt: String,
    ): String = """{"id":"$id","title":"$title","created_at":"2026-07-15T07:00:00Z","updated_at":"$updatedAt"}"""
}

private fun DpRect.precedes(other: DpRect): Boolean = top < other.top || (top == other.top && left < other.left)

private fun DpRect.width() = right - left

private fun jsonResponse(body: String): MockResponse =
    MockResponse()
        .setResponseCode(200)
        .setBody(body)
        .addHeader("Content-Type", "application/json")
