package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.Stroke
import link.desync.vnote.auth.StrokePoint
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.PageInkSession
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

@RunWith(AndroidJUnit4::class)
class PageInkInstrumentedTest {
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
    fun acquiresLeaseRendersSnapshotAndCommitsStroke() {
        val committed = AtomicReference<String>()
        val commitLatch = CountDownLatch(1)
        server.enqueue(
            MockResponse().withWebSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        webSocket.send("""{"type":"welcome","session_id":"me","last_seq":0}""")
                    }

                    override fun onMessage(
                        webSocket: WebSocket,
                        text: String,
                    ) {
                        when (JSONObject(text).getString("type")) {
                            "subscribe" -> {
                                webSocket.send(
                                    """{"type":"stroke-batch","seq":1,"client_batch_id":"seed","strokes":[{"tool":"pen","color":"#006400","width":4.0,"points":[{"x":1.0,"y":2.0,"t":0}]}]}""",
                                )
                                webSocket.send("""{"type":"synced","last_seq":1}""")
                            }
                            "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                            "commit-batch" -> {
                                committed.set(text)
                                val obj = JSONObject(text)
                                webSocket.send(
                                    JSONObject()
                                        .put("type", "stroke-batch")
                                        .put("seq", 2)
                                        .put("client_batch_id", obj.getString("client_batch_id"))
                                        .put("strokes", obj.getJSONArray("strokes"))
                                        .toString(),
                                )
                                commitLatch.countDown()
                            }
                        }
                    }
                },
            ),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()

        // Snapshot replayed and the edit lease was granted.
        assertTrue("snapshot stroke rendered", awaitUntil { session.strokes.size == 1 })
        assertTrue("lease granted", awaitUntil { session.canEdit })

        // Commit a captured stroke; it renders optimistically and is sent on the wire.
        session.commitStroke(Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16))))
        assertTrue("commit sent", commitLatch.await(5, TimeUnit.SECONDS))

        val commitJson = JSONObject(committed.get())
        assertEquals("commit-batch", commitJson.getString("type"))
        assertEquals(2, commitJson.getJSONArray("strokes").getJSONObject(0).getJSONArray("points").length())

        // Optimistic add + server echo dedupe by client_batch_id => exactly two strokes.
        assertTrue("no duplicate from echo", awaitUntil { session.strokes.size == 2 })
        Thread.sleep(200)
        assertEquals(2, session.strokes.size)

        session.disconnect()
    }

    @Test
    fun blocksInkWhenAnotherSessionHoldsLease() {
        server.enqueue(
            MockResponse().withWebSocketUpgrade(
                object : WebSocketListener() {
                    override fun onOpen(
                        webSocket: WebSocket,
                        response: okhttp3.Response,
                    ) {
                        webSocket.send(
                            """{"type":"welcome","session_id":"me","last_seq":0,"lease_holder":"other"}""",
                        )
                    }

                    override fun onMessage(
                        webSocket: WebSocket,
                        text: String,
                    ) {
                        if (JSONObject(text).getString("type") == "subscribe") {
                            webSocket.send("""{"type":"synced","last_seq":0}""")
                        }
                    }
                },
            ),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()

        assertTrue(
            "blocked banner shown",
            awaitUntil { session.statusBanner == PageInkSession.LEASE_BLOCKED },
        )
        assertEquals(false, session.canEdit)

        session.disconnect()
    }

    private fun awaitUntil(
        timeoutMs: Long = 5_000,
        condition: () -> Boolean,
    ): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) {
                return true
            }
            Thread.sleep(25)
        }
        return condition()
    }
}
