package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.PageInkSession
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.WebSocket
import okhttp3.WebSocketListener
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

// Enough batches that a per-batch publish would be unmistakable, while keeping
// the MockWebServer exchange quick.
private const val REPLAY_STROKES = 25

private val seedStrokeMessage =
    """
    {
      "type": "stroke-batch",
      "seq": 1,
      "client_batch_id": "seed",
      "strokes": [{
        "id": "seed-stroke",
        "style": {
          "tool_kind": "solid_round",
          "style_version": 1,
          "parameters": {
            "color": "#006400",
            "width": 4.0,
            "cap_style": "round",
            "join_style": "round"
          }
        },
        "points": [{"x": 1.0, "y": 2.0, "t": 0}]
      }]
    }
    """.trimIndent()

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
        apiClient = OkHttpApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, authRepository)
    }

    @After
    fun tearDown() {
        apiClient.shutdown()
        tokenStore.clear()
        server.close()
    }

    @Test
    fun acquiresLeaseRendersSnapshotAndCommitsStroke() {
        val committed = AtomicReference<String>()
        val commitLatch = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
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
                                    webSocket.send(seedStrokeMessage)
                                    webSocket.send("""{"type":"synced","last_seq":1}""")
                                }
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "release-lease" -> webSocket.close(1000, "lease released")
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
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()

        // Snapshot replayed and the edit lease was granted.
        assertTrue("snapshot stroke rendered", awaitUntil { session.strokes.size == 1 })
        assertTrue("lease granted", awaitUntil { session.canEdit })

        // Commit a captured stroke; it is sent on the wire and rendered on echo.
        session.commitStroke(
            Stroke(
                points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)),
                style =
                    StrokeStyle(
                        parameters = SolidRoundParameters(color = "#C62828", width = 8.5),
                    ),
            ),
        )
        assertTrue("commit sent", commitLatch.await(5, TimeUnit.SECONDS))

        val commitJson = JSONObject(committed.get())
        assertEquals("commit-batch", commitJson.getString("type"))
        assertEquals(
            2,
            commitJson
                .getJSONArray("strokes")
                .getJSONObject(0)
                .getJSONArray("points")
                .length(),
        )
        val style = commitJson.getJSONArray("strokes").getJSONObject(0).getJSONObject("style")
        assertEquals("solid_round", style.getString("tool_kind"))
        assertEquals("#C62828", style.getJSONObject("parameters").getString("color"))
        assertEquals(8.5, style.getJSONObject("parameters").getDouble("width"), 0.0)

        // Snapshot stroke + server echo => exactly two committed strokes.
        assertTrue("no duplicate from echo", awaitUntil { session.strokes.size == 2 })
        Thread.sleep(200)
        assertEquals(2, session.strokes.size)

        session.disconnect()
    }

    @Test
    fun keepsCompletedStrokeVisibleUntilDelayedEchoWithoutDuplication() {
        val commitReceived = CountDownLatch(1)
        val releaseEcho = CountDownLatch(1)
        val echoesSent = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
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
                                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "release-lease" -> webSocket.close(1000, "lease released")
                                "commit-batch" -> {
                                    val commit = JSONObject(text)
                                    commitReceived.countDown()
                                    releaseEcho.await(5, TimeUnit.SECONDS)
                                    val echo =
                                        JSONObject()
                                            .put("type", "stroke-batch")
                                            .put("seq", 1)
                                            .put("client_batch_id", commit.getString("client_batch_id"))
                                            .put("strokes", commit.getJSONArray("strokes"))
                                            .toString()
                                    webSocket.send(echo)
                                    webSocket.send(echo)
                                    echoesSent.countDown()
                                }
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        val stroke = Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)))
        session.connect()
        assertTrue("lease granted", awaitUntil { session.canEdit })

        session.commitStroke(stroke)
        assertTrue("commit sent", commitReceived.await(5, TimeUnit.SECONDS))
        assertEquals("local pending ink remains visible", listOf(stroke), session.strokes)
        assertEquals("one local batch awaits its echo", 1, session.pendingBatchCount)

        releaseEcho.countDown()
        assertTrue("echoes sent", echoesSent.await(5, TimeUnit.SECONDS))
        assertTrue("local batch promoted by echo", awaitUntil { session.pendingBatchCount == 0 })
        assertEquals("confirmed stroke rendered once", listOf(stroke), session.strokes)
        Thread.sleep(200)
        assertEquals("duplicate echo does not duplicate ink", listOf(stroke), session.strokes)

        session.disconnect()
    }

    @Test
    fun clearsPendingInkWhenServerRejectsCommit() {
        val commitReceived = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
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
                                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "release-lease" -> webSocket.close(1000, "lease released")
                                "commit-batch" -> {
                                    commitReceived.countDown()
                                    webSocket.send(
                                        """{"type":"error","code":"commit_failed","message":"Commit failed"}""",
                                    )
                                }
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()
        assertTrue("lease granted", awaitUntil { session.canEdit })

        session.commitStroke(Stroke(points = listOf(StrokePoint(10.0, 20.0, 0))))
        assertTrue("commit sent", commitReceived.await(5, TimeUnit.SECONDS))
        assertTrue("rejection clears pending ink", awaitUntil { session.pendingBatchCount == 0 })
        assertEquals("uncommitted ink is not rendered", emptyList<Stroke>(), session.strokes)
        // Await rather than read: `handle` publishes strokes before it sets the
        // banner, so polling on one and immediately reading the other races the
        // remainder of that handler.
        assertTrue(
            "commit failure shown",
            awaitUntil { session.statusBanner == "Commit failed" },
        )
        assertEquals("input blocked", false, session.canEdit)

        session.disconnect()
    }

    @Test
    fun erasingVisibleStrokeHidesItImmediatelyAndCommitsTombstone() {
        val tombstone = AtomicReference<String>()
        val tombstoneLatch = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : WebSocketListener() {
                        override fun onOpen(
                            webSocket: WebSocket,
                            response: okhttp3.Response,
                        ) {
                            webSocket.send("""{"type":"welcome","session_id":"me","last_seq":1}""")
                        }

                        override fun onMessage(
                            webSocket: WebSocket,
                            text: String,
                        ) {
                            when (JSONObject(text).getString("type")) {
                                "subscribe" -> {
                                    webSocket.send(seedStrokeMessage)
                                    webSocket.send("""{"type":"synced","last_seq":1}""")
                                }
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "commit-tombstones" -> {
                                    tombstone.set(text)
                                    val message = JSONObject(text)
                                    webSocket.send(
                                        JSONObject()
                                            .put("type", "tombstone-batch")
                                            .put("revision", 2)
                                            .put(
                                                "client_mutation_id",
                                                message.getString("client_mutation_id"),
                                            ).put("stroke_ids", message.getJSONArray("stroke_ids"))
                                            .toString(),
                                    )
                                    tombstoneLatch.countDown()
                                }
                                "release-lease" -> webSocket.close(1000, "lease released")
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()
        assertTrue("stroke loaded", awaitUntil { session.strokes.singleOrNull()?.id == "seed-stroke" })
        assertTrue("lease granted", awaitUntil { session.canEdit })

        session.eraseStrokes(listOf("seed-stroke", "seed-stroke"))

        assertEquals("stroke hidden before acknowledgement", emptyList<Stroke>(), session.strokes)
        assertTrue("tombstone sent", tombstoneLatch.await(5, TimeUnit.SECONDS))
        val message = JSONObject(tombstone.get())
        assertEquals("commit-tombstones", message.getString("type"))
        assertEquals(1, message.getJSONArray("stroke_ids").length())
        assertEquals("seed-stroke", message.getJSONArray("stroke_ids").getString(0))

        session.disconnect()
    }

    @Test
    fun failedEraseRestoresOptimisticallyHiddenStroke() {
        val tombstoneReceived = CountDownLatch(1)
        val allowFailure = CountDownLatch(1)
        val mutationId = AtomicReference<String>()
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : WebSocketListener() {
                        override fun onOpen(
                            webSocket: WebSocket,
                            response: okhttp3.Response,
                        ) {
                            webSocket.send("""{"type":"welcome","session_id":"me","last_seq":1}""")
                        }

                        override fun onMessage(
                            webSocket: WebSocket,
                            text: String,
                        ) {
                            when (JSONObject(text).getString("type")) {
                                "subscribe" -> {
                                    webSocket.send(seedStrokeMessage)
                                    webSocket.send("""{"type":"synced","last_seq":1}""")
                                }
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "commit-tombstones" -> {
                                    val message = JSONObject(text)
                                    mutationId.set(message.getString("client_mutation_id"))
                                    tombstoneReceived.countDown()
                                    allowFailure.await(5, TimeUnit.SECONDS)
                                    webSocket.send(
                                        JSONObject()
                                            .put("type", "error")
                                            .put("code", "tombstone_failed")
                                            .put("message", "Could not erase stroke")
                                            .put("client_mutation_id", mutationId.get())
                                            .toString(),
                                    )
                                }
                                "release-lease" -> webSocket.close(1000, "lease released")
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()
        assertTrue("stroke loaded", awaitUntil { session.strokes.singleOrNull()?.id == "seed-stroke" })
        assertTrue("lease granted", awaitUntil { session.canEdit })

        try {
            session.eraseStrokes(listOf("seed-stroke"))
            assertTrue("tombstone sent", tombstoneReceived.await(5, TimeUnit.SECONDS))
            assertEquals("stroke hidden before failure", emptyList<Stroke>(), session.strokes)
        } finally {
            allowFailure.countDown()
        }

        assertTrue(
            "failed erase restores the original stroke",
            awaitUntil { session.strokes.singleOrNull()?.id == "seed-stroke" },
        )
        assertTrue(
            "failure shown",
            awaitUntil { session.statusBanner == "Could not erase stroke" },
        )
        assertEquals("input blocked after server failure", false, session.canEdit)

        session.disconnect()
    }

    @Test
    fun blocksInkWhenAnotherSessionHoldsLease() {
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
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
                            when (JSONObject(text).getString("type")) {
                                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                                "release-lease" -> webSocket.close(1000, "lease released")
                            }
                        }
                    },
                ).build(),
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

    /**
     * Individual `stroke-batch` messages arriving inside the replay window —
     * which is what a sibling device's live commit looks like while this
     * session is still catching up — must land on the canvas as one paint at
     * `synced`, not N. That stepwise reveal is the bug in #312.
     *
     * `lease-granted` is sent *after* the batches and the channel is ordered, so
     * `canEdit` proves every batch has already been handled: the emptiness
     * asserted here is held-back ink, not ink that has yet to arrive.
     */
    @Test
    fun batchesArrivingDuringReplayAreWithheldUntilSyncedThenPublishedInOneStep() {
        val socket = AtomicReference<WebSocket>()
        openPage(socket) { webSocket, type ->
            if (type == "subscribe") {
                repeat(REPLAY_STROKES) { index -> webSocket.send(replayBatch(index)) }
            }
        }

        val session = openSession()

        assertTrue("replay delivered", awaitUntil { session.canEdit })
        assertEquals("no ink painted mid-replay", 0, session.strokes.size)

        socket.get().send("""{"type":"synced","last_seq":$REPLAY_STROKES}""")

        assertTrue("whole page painted at synced", awaitUntil { session.strokes.size == REPLAY_STROKES })
        session.disconnect()
    }

    /**
     * What the server actually sends since #323: the whole replay in **one**
     * `page-replay` frame. N stored batches cost one message, and the page is
     * painted once — no `synced` follows, because the frame carries it.
     */
    @Test
    fun aCoalescedReplayFramePaintsTheWholePageInOneStep() {
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(replayFrame(REPLAY_STROKES))
            }
        }

        val session = openSession()

        assertTrue(
            "whole page painted from one frame",
            awaitUntil { session.strokes.size == REPLAY_STROKES },
        )
        assertEquals(
            (0 until REPLAY_STROKES).map { "replay-stroke-$it" },
            session.strokes.map { it.id },
        )
        session.disconnect()
    }

    /** A tombstone inside the replay frame is applied in that same publish. */
    @Test
    fun replayAppliesItsOwnTombstonesBeforePublishing() {
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(
                    replayFrame(
                        batchCount = 2,
                        tombstonedStrokeIds = listOf("replay-stroke-0"),
                    ),
                )
            }
        }

        val session = openSession()

        assertTrue("surviving stroke painted", awaitUntil { session.strokes.size == 1 })
        assertEquals("replay-stroke-1", session.strokes[0].id)
        session.disconnect()
    }

    /** `replay_failed` must show the ink that did arrive, not a blank page. */
    @Test
    fun failureDuringReplayPublishesWhatArrived() {
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(replayBatch(0))
                webSocket.send(replayBatch(1))
                webSocket.send(
                    """{"type":"error","code":"replay_failed","message":"could not load page ink"}""",
                )
            }
        }

        val session = openSession()

        assertTrue("partial ink painted", awaitUntil { session.strokes.size == 2 })
        assertTrue("error surfaced", awaitUntil { session.statusBanner != null })
        session.disconnect()
    }

    /** Nor may a socket that drops before `synced` leave the page blank. */
    @Test
    fun socketCloseDuringReplayPublishesWhatArrived() {
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(replayBatch(0))
                webSocket.send(replayBatch(1))
                webSocket.close(1000, "dropped mid-replay")
            }
        }

        val session = openSession()

        assertTrue("partial ink painted", awaitUntil { session.strokes.size == 2 })
        session.disconnect()
    }

    private fun openSession(): PageInkSession {
        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()
        return session
    }

    /**
     * Enqueue a page channel that welcomes, grants the lease, and delegates
     * every client message to [onMessage]. The lease reply is sent after
     * [onMessage] has run, preserving replay-then-lease ordering.
     */
    private fun openPage(
        socket: AtomicReference<WebSocket>? = null,
        onMessage: (WebSocket, String) -> Unit,
    ) {
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : WebSocketListener() {
                        override fun onOpen(
                            webSocket: WebSocket,
                            response: okhttp3.Response,
                        ) {
                            socket?.set(webSocket)
                            webSocket.send("""{"type":"welcome","session_id":"me","last_seq":0}""")
                        }

                        override fun onMessage(
                            webSocket: WebSocket,
                            text: String,
                        ) {
                            val type = JSONObject(text).getString("type")
                            onMessage(webSocket, type)
                            when (type) {
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "release-lease" -> webSocket.close(1000, "lease released")
                            }
                        }
                    },
                ).build(),
        )
    }

    /** One stored batch as its own `stroke-batch` frame. */
    private fun replayBatch(index: Int): String = """{"type":"stroke-batch",${batchFields(index)}}"""

    /** The same batch as an element of a `page-replay` frame's `batches`. */
    private fun replayBatchBody(index: Int): String = """{${batchFields(index)}}"""

    private fun batchFields(index: Int): String =
        """
        "seq": ${index + 1},
        "client_batch_id": "replay-$index",
        "strokes": [{
          "id": "replay-stroke-$index",
          "style": {
            "tool_kind": "solid_round",
            "style_version": 1,
            "parameters": {
              "color": "#006400",
              "width": 4.0,
              "cap_style": "round",
              "join_style": "round"
            }
          },
          "points": [{"x": ${index * 10}.0, "y": 2.0, "t": 0}]
        }]
        """.trimIndent()

    /**
     * One coalesced `page-replay` frame carrying [batchCount] stored batches
     * and, when given, a tombstone batch erasing [tombstonedStrokeIds].
     *
     * The tombstoned strokes are still present in `batches` here, unlike a real
     * server frame, so the client's own delete-wins pass is what the assertion
     * rests on.
     */
    private fun replayFrame(
        batchCount: Int,
        tombstonedStrokeIds: List<String> = emptyList(),
    ): String {
        val tombstones =
            if (tombstonedStrokeIds.isEmpty()) {
                "[]"
            } else {
                """[{"revision":1,"client_mutation_id":"m1",""" +
                    """"stroke_ids":[${tombstonedStrokeIds.joinToString(",") { "\"$it\"" }}]}]"""
            }
        val batches = (0 until batchCount).joinToString(",") { replayBatchBody(it) }
        return """{"type":"page-replay","page_id":"page_1","last_seq":$batchCount,""" +
            """"batches":[$batches],"tombstones":$tombstones}"""
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
