package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.LibraryEvent
import link.desync.vnote.auth.LibraryEventListener
import link.desync.vnote.auth.TokenStore
import kotlinx.coroutines.runBlocking
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class PageLibraryInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: ApiClient

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start(InetAddress.getByName("127.0.0.1"), 0)
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        tokenStore = TokenStore(context)
        tokenStore.clear()
        tokenStore.saveTokens(
            accessToken = "access-token",
            refreshToken = "refresh-token",
            accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
        )
        val authRepository =
            object : AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore) {
                override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean = true
            }
        apiClient = ApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, authRepository)
    }

    @After
    fun tearDown() {
        tokenStore.clear()
        server.shutdown()
    }

    @Test
    fun pageCrudUsesAuthenticatedApi() {
        server.enqueue(
            jsonResponse(
                """{"pages":[{"id":"page_1","title":"First","created_at":"2026-06-10T22:00:00Z","updated_at":"2026-06-10T22:00:00Z"}]}""",
            ),
        )
        server.enqueue(
            jsonResponse(
                """{"page":{"id":"page_2","title":"Untitled page","created_at":"2026-06-10T22:01:00Z","updated_at":"2026-06-10T22:01:00Z"}}""",
            ).setResponseCode(201),
        )
        server.enqueue(MockResponse().setResponseCode(204))

        runBlocking {
            val pages = apiClient.listPages().getOrThrow()
            assertEquals("First", pages.single().title)

            val created = apiClient.createPage().getOrThrow()
            assertEquals("page_2", created.id)

            assertTrue(apiClient.deletePage(created.id).isSuccess)
        }

        repeat(3) {
            assertEquals("Bearer access-token", server.takeRequest().getHeader("Authorization"))
        }
    }

    @Test
    fun librarySocketReceivesPageCreatedEvent() {
        val latch = CountDownLatch(1)
        server.enqueue(
            MockResponse().withWebSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        webSocket.send(
                            """{"type":"page-created","page":{"id":"page_3","title":"Live","created_at":"2026-06-10T22:02:00Z","updated_at":"2026-06-10T22:02:00Z"}}""",
                        )
                        webSocket.close(1000, "event sent")
                    }
                },
            ),
        )

        val socket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onEvent(event: LibraryEvent) {
                        if (event is LibraryEvent.PageCreated && event.page.title == "Live") {
                            latch.countDown()
                        }
                    }

                    override fun onError(message: String) = Unit

                    override fun onClosed() = Unit
                },
            )

        assertTrue(latch.await(5, TimeUnit.SECONDS))
        socket?.close(1000, "test complete")
        assertEquals("Bearer access-token", server.takeRequest().getHeader("Authorization"))
    }
}

private fun jsonResponse(body: String): MockResponse =
    MockResponse()
        .setResponseCode(200)
        .setBody(body)
        .addHeader("Content-Type", "application/json")
