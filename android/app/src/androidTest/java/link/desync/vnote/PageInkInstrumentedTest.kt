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
import link.desync.vnote.ink.PageStatusKind
import link.desync.vnote.ink.Paper
import link.desync.vnote.ink.SessionSync
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
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
import java.util.concurrent.CopyOnWriteArrayList
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
          "style_version": 2,
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

// A page socket that answers the app's close. The app closes its page socket
// when the session disconnects, and MockWebServer's shutdown waits on a
// half-closed connection until it gives up. The edit lease used to hide this:
// every fake closed its side on `release-lease` (#630).
private open class AnsweringCloses : WebSocketListener() {
    override fun onClosing(
        webSocket: WebSocket,
        code: Int,
        reason: String,
    ) {
        webSocket.close(code, reason)
    }
}

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
    fun rendersSnapshotThenCommitsStroke() {
        val committed = AtomicReference<String>()
        val commitLatch = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        session.connect()

        // Snapshot replayed; the page is open for editing.
        assertTrue("snapshot stroke rendered", awaitUntil { session.strokes.size == 1 })
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

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
                    object : AnsweringCloses() {
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

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        val stroke = Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)))
        session.connect()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

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

    /**
     * #150, #159: a `commit_failed` while connected (the server is up but could
     * not store the batch) says it is retrying, keeps the stroke on the canvas
     * and in the outbox, re-sends it under the same `client_batch_id`, and
     * clears once the server stores and echoes it. Ink is never refused.
     */
    @Test
    fun aFailedCommitIsKeptAndRetriedUntilStored() {
        val commits = CopyOnWriteArrayList<String>()
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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
                            val message = JSONObject(text)
                            when (message.getString("type")) {
                                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                                "commit-batch" -> {
                                    commits += message.getString("client_batch_id")
                                    if (commits.size == 1) {
                                        webSocket.send(
                                            JSONObject()
                                                .put("type", "error")
                                                .put("code", "commit_failed")
                                                .put("message", "could not persist strokes")
                                                .put("client_mutation_id", message.getString("client_batch_id"))
                                                .toString(),
                                        )
                                    } else {
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
                            }
                        }
                    },
                ).build(),
        )

        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

        session.commitStroke(Stroke(points = listOf(StrokePoint(10.0, 20.0, 0))))
        assertTrue(
            "retrying shown",
            awaitUntil { session.status?.kind == PageStatusKind.Retrying },
        )
        assertEquals(PageInkSession.SYNC_RETRYING, session.statusBanner)
        assertEquals("the failed batch is kept, not discarded", 1, session.pendingBatchCount)
        assertEquals("and still drawn", 1, session.strokes.size)
        // Retrying is not a disconnect: the socket stays live.
        assertTrue("still live", session.canEdit)

        assertTrue(
            "recovered once the retry is stored",
            awaitUntil(timeoutMs = 10_000) { session.pendingBatchCount == 0 && session.status == null },
        )
        assertEquals("confirmed stroke rendered once", 1, session.strokes.size)
        assertEquals(
            "nothing owed once stored",
            0,
            LocalStore
                .shared(context())
                .outbox.read
                .awaitingCount(),
        )
        assertEquals("re-sent under the same id", 1, commits.toSet().size)
        assertEquals("sent twice", 2, commits.size)

        session.disconnect()
    }

    /**
     * #159: a definitive rejection is not retried and never counts as synced.
     * The ink is kept — drawn, and in the outbox as rejected — and the page
     * says why, offering recovery.
     */
    @Test
    fun aRejectedCommitIsKeptShownAndNeverSynced() {
        val commitReceived = CountDownLatch(1)
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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
                                "commit-batch" -> {
                                    commitReceived.countDown()
                                    webSocket.send(
                                        """{"type":"error","code":"invalid_stroke_style","message":"Commit rejected"}""",
                                    )
                                }
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        session.connect()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

        session.commitStroke(Stroke(points = listOf(StrokePoint(10.0, 20.0, 0))))
        assertTrue("commit sent", commitReceived.await(5, TimeUnit.SECONDS))
        assertTrue("no longer pending", awaitUntil { session.pendingBatchCount == 0 })
        assertEquals("the refused ink is still drawn", 1, session.strokes.size)
        // Await rather than read: `handle` publishes strokes before it sets the
        // banner, so polling on one and immediately reading the other races the
        // remainder of that handler.
        assertTrue(
            "commit rejection shown",
            awaitUntil { session.statusBanner == "Commit rejected" },
        )
        assertEquals(PageStatusKind.Rejected, session.status?.kind)
        assertEquals("not live once refused", false, session.canEdit)
        val rejections =
            LocalStore
                .shared(context())
                .outbox.read
                .rejections()
        assertEquals("recorded as refused, with its stroke", listOf(1), rejections.map { it.strokeCount })
        assertEquals("invalid_stroke_style", rejections.single().code)

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
                    object : AnsweringCloses() {
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
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        session.connect()
        assertTrue("stroke loaded", awaitUntil { session.strokes.singleOrNull()?.id == "seed-stroke" })
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

        session.eraseStrokes(listOf("seed-stroke", "seed-stroke"))

        assertEquals("stroke hidden before acknowledgement", emptyList<Stroke>(), session.strokes)
        assertTrue("tombstone sent", tombstoneLatch.await(5, TimeUnit.SECONDS))
        val message = JSONObject(tombstone.get())
        assertEquals("commit-tombstones", message.getString("type"))
        assertEquals(1, message.getJSONArray("stroke_ids").length())
        assertEquals("seed-stroke", message.getJSONArray("stroke_ids").getString(0))

        session.disconnect()
    }

    /**
     * #159: `tombstone_failed` is the server failing to store an erase, not a
     * refusal. The stroke stays erased here, the erase stays owed, and it is
     * sent again with the backoff until the server takes it.
     */
    @Test
    fun aFailedEraseStaysErasedAndIsRetried() {
        val tombstoneReceived = CountDownLatch(1)
        val allowFailure = CountDownLatch(1)
        val mutationId = AtomicReference<String>()
        val erases =
            java.util.concurrent.atomic
                .AtomicInteger()
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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
                                "commit-tombstones" -> {
                                    val message = JSONObject(text)
                                    mutationId.set(message.getString("client_mutation_id"))
                                    if (erases.incrementAndGet() == 1) {
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
                                    } else {
                                        webSocket.send(
                                            JSONObject()
                                                .put("type", "tombstone-batch")
                                                .put("revision", 2)
                                                .put("client_mutation_id", mutationId.get())
                                                .put("stroke_ids", message.getJSONArray("stroke_ids"))
                                                .toString(),
                                        )
                                    }
                                }
                            }
                        }
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        session.connect()
        assertTrue("stroke loaded", awaitUntil { session.strokes.singleOrNull()?.id == "seed-stroke" })
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

        try {
            session.eraseStrokes(listOf("seed-stroke"))
            assertTrue("tombstone sent", tombstoneReceived.await(5, TimeUnit.SECONDS))
            assertEquals("stroke hidden before failure", emptyList<Stroke>(), session.strokes)
        } finally {
            allowFailure.countDown()
        }

        assertTrue("retrying shown", awaitUntil { session.status?.kind == PageStatusKind.Retrying })
        assertEquals("the stroke stays erased", emptyList<Stroke>(), session.strokes)
        assertTrue("still live", session.canEdit)
        assertTrue("the erase is sent again", awaitUntil(timeoutMs = 10_000) { erases.get() >= 2 })
        assertTrue("and settles once stored", awaitUntil(timeoutMs = 10_000) { session.status == null })
        assertEquals(
            "nothing owed",
            0,
            LocalStore
                .shared(context())
                .outbox.read
                .awaitingCount(),
        )

        session.disconnect()
    }

    /**
     * #390: the page is drawn a round trip before its replay lands. A stroke
     * finished before then is drawn and held — not refused — and sent exactly
     * once when the replay is applied.
     */
    @Test
    fun aStrokeDrawnBeforeTheReplayLandsIsHeldThenSentOnce() {
        val subscribed = AtomicReference<WebSocket>()
        val commits = CopyOnWriteArrayList<JSONObject>()
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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
                            val message = JSONObject(text)
                            when (message.getString("type")) {
                                // Parked: the test answers it once a stroke is held.
                                "subscribe" -> subscribed.set(webSocket)
                                "commit-batch" -> {
                                    commits += message
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
                        }
                    },
                ).build(),
        )

        val session = openSession()
        assertTrue("replay requested", awaitUntil { subscribed.get() != null })
        val stroke = Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)))
        onMain { session.commitStroke(stroke) }

        assertEquals("held stroke is drawn", listOf(stroke.id), onMain { session.strokes.map { it.id } })
        assertEquals("and waits", 1, onMain { session.pendingBatchCount })
        Thread.sleep(300)
        assertEquals("nothing is sent before the replay lands", 0, commits.size)

        subscribed.get().send("""{"type":"page-replay","page_id":"page_1","last_seq":0,"batches":[],"tombstones":[]}""")
        assertTrue("sent once caught up", awaitUntil { commits.size == 1 })
        assertEquals(
            stroke.id,
            commits[0].getJSONArray("strokes").getJSONObject(0).getString("id"),
        )
        assertTrue("confirmed by the echo", awaitUntil { onMain { session.pendingBatchCount == 0 } })
        Thread.sleep(300)
        assertEquals("sent exactly once", 1, commits.size)
        assertEquals(listOf(stroke.id), onMain { session.strokes.map { it.id } })

        session.disconnect()
    }

    /**
     * #630 acceptance 1: there is no edit lease. Another device inks the same
     * page while this one does: neither is refused, each of this session's
     * commits is echoed in place, and the other device's batches land live in
     * server order, so the page is the union.
     */
    @Test
    fun anotherDevicesInkArrivesLiveWhileThisDeviceInks() {
        val live = AtomicReference<WebSocket>()
        val commits = CopyOnWriteArrayList<JSONObject>()
        openPage(live) { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }
        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })
        echoCommits(commits)

        // The other device inks first, then this one, then the other again.
        live.get().send(remoteBatch(seq = 1, id = "remote-1"))
        assertTrue("the other device's ink arrives live", awaitUntil { onMain { session.strokes.map { it.id } == listOf("remote-1") } })
        val mine = Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)))
        onMain { session.commitStroke(mine) }
        assertTrue("this device's stroke committed", awaitUntil { commits.size == 1 })
        live.get().send(remoteBatch(seq = 3, id = "remote-2"))

        assertTrue(
            "both devices' ink on the page",
            awaitUntil { onMain { session.strokes.map { it.id }.toSet() == setOf("remote-1", mine.id, "remote-2") } },
        )
        assertTrue("own stroke confirmed", awaitUntil { onMain { session.pendingBatchCount == 0 } })
        assertEquals(
            "stacked in server arrival order",
            listOf("remote-1", mine.id, "remote-2"),
            onMain { session.strokes.map { it.id } },
        )
        assertEquals("never refused", null, session.status)
        assertTrue("still editing", session.canEdit)

        session.disconnect()
    }

    /**
     * #630 acceptance 2: another device erases while this one draws. The
     * erasure reaches this session live, removes only the strokes it names
     * (one of them this device's own, already confirmed), and leaves ink this
     * device is still waiting on untouched.
     */
    @Test
    fun anotherDevicesErasureAppliesLiveWithoutTouchingPendingInk() {
        val live = AtomicReference<WebSocket>()
        openPage(live) { webSocket, type ->
            if (type == "subscribe") webSocket.send(replayFrame(batchCount = 2))
        }
        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })
        assertEquals(listOf("replay-stroke-0", "replay-stroke-1"), onMain { session.strokes.map { it.id } })

        // A commit the server has not echoed yet: the eraser on the other
        // device cannot have seen it.
        val pending = Stroke(points = listOf(StrokePoint(5.0, 5.0, 0)))
        onMain { session.commitStroke(pending) }
        live.get().send(
            """{"type":"tombstone-batch","revision":3,"client_mutation_id":"remote-erase","stroke_ids":["replay-stroke-0"]}""",
        )

        assertTrue(
            "the erased stroke leaves the canvas",
            awaitUntil { onMain { session.strokes.map { it.id } == listOf("replay-stroke-1", pending.id) } },
        )
        assertEquals("pending ink kept", 1, onMain { session.pendingBatchCount })
        assertEquals("never refused", null, session.status)

        session.disconnect()
    }

    /**
     * #630 acceptance 3: paper set on two devices converges on the last
     * arrival. This session's own request is overtaken by another device's
     * change landing after it; every `paper-changed` is applied in order.
     */
    @Test
    fun paperChangesFromTwoDevicesConvergeOnTheLastArrival() {
        val live = AtomicReference<WebSocket>()
        val requested = CountDownLatch(1)
        openPage(live) { webSocket, type ->
            when (type) {
                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                "set-paper" -> requested.countDown()
            }
        }
        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })

        onMain { session.setPaper(Paper.RuledWide) }
        assertTrue("paper requested", requested.await(5, TimeUnit.SECONDS))
        live.get().send("""{"type":"paper-changed","paper":"ruled-wide","revision":2}""")
        live.get().send("""{"type":"paper-changed","paper":"squared-small","revision":3}""")

        assertTrue("last arrival wins", awaitUntil { onMain { session.paper == Paper.SquaredSmall } })
        Thread.sleep(200)
        assertEquals(Paper.SquaredSmall, onMain { session.paper })

        session.disconnect()
    }

    /**
     * Individual `stroke-batch` messages arriving inside the replay window —
     * which is what a sibling device's live commit looks like while this
     * session is still catching up — must land on the canvas as one paint at
     * `synced`, not N. That stepwise reveal is the bug in #312.
     *
     * A `paper-changed` is sent *after* the batches and the channel is
     * ordered, so seeing its paper proves every batch has already been
     * handled: the emptiness asserted here is held-back ink, not ink that has
     * yet to arrive.
     */
    @Test
    fun batchesArrivingDuringReplayAreWithheldUntilSyncedThenPublishedInOneStep() {
        val socket = AtomicReference<WebSocket>()
        openPage(socket) { webSocket, type ->
            if (type == "subscribe") {
                repeat(REPLAY_STROKES) { index -> webSocket.send(replayBatch(index)) }
                webSocket.send("""{"type":"paper-changed","paper":"ruled-wide","revision":1}""")
            }
        }

        val session = openSession()

        assertTrue("replay delivered", awaitUntil { session.paper == Paper.RuledWide })
        assertEquals("not live until synced", false, session.canEdit)
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

    /**
     * #279: the server closes a page subscriber that fell behind its fan-out
     * (1013 "lagged") and relies on the client to come back. The session must
     * reconnect on its own, resubscribe from the last `seq` it holds, and end up
     * with what the server has — here a batch, an erasure of ink it already
     * showed and a paper change, all of which it missed while lagging.
     */
    @Test
    fun reconnectsAfterServerCloseAndConvergesOnMissedInkErasureAndPaper() {
        val resubscribedFrom = AtomicReference<Long>()
        // First connection: one batch, then the server gives up on this socket.
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(replayFrame(batchCount = 1))
                webSocket.close(1013, "lagged")
            }
        }
        // Second: what the server holds now — batches 2 and 3, batch 1's
        // stroke erased, and a different paper.
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
                        override fun onOpen(
                            webSocket: WebSocket,
                            response: okhttp3.Response,
                        ) {
                            webSocket.send(
                                """{"type":"welcome","session_id":"me-again","last_seq":3,"paper":"ruled-wide"}""",
                            )
                        }

                        override fun onMessage(
                            webSocket: WebSocket,
                            text: String,
                        ) {
                            val message = JSONObject(text)
                            when (message.getString("type")) {
                                "subscribe" -> {
                                    resubscribedFrom.set(message.getLong("from_seq"))
                                    val batches = listOf(1, 2).joinToString(",") { replayBatchBody(it) }
                                    webSocket.send(
                                        """{"type":"page-replay","page_id":"page_1","last_seq":3,""" +
                                            """"batches":[$batches],"tombstones":[{"revision":4,""" +
                                            """"client_mutation_id":"m1","stroke_ids":["replay-stroke-0"]}]}""",
                                    )
                                }
                            }
                        }
                    },
                ).build(),
        )

        val session = openSession()

        assertTrue(
            "converged on the server's ink",
            awaitUntil(timeoutMs = 10_000) {
                session.strokes.map { it.id } == listOf("replay-stroke-1", "replay-stroke-2")
            },
        )
        assertEquals("resubscribed from the last seq it held", 1L, resubscribedFrom.get())
        assertEquals("paper from the fresh welcome", Paper.RuledWide, session.paper)
        assertTrue("editing again after reconnecting", awaitUntil { session.canEdit })
        assertEquals("banner cleared", null, session.statusBanner)
        assertEquals("one reconnect, not a loop", 2, server.requestCount)

        session.disconnect()
    }

    /** A page the user left must not reconnect behind their back. */
    @Test
    fun doesNotReconnectAfterDisconnect() {
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }
        val session = openSession()
        assertTrue("connected", awaitUntil { session.canEdit })

        session.disconnect()
        Thread.sleep(2_500)

        assertEquals("no second upgrade", 1, server.requestCount)
    }

    /**
     * The user leaves while a reconnect is scheduled: the pending reconnect is
     * cancelled, not merely made harmless when it fires.
     */
    @Test
    fun leavingWhileAReconnectIsPendingCancelsIt() {
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.close(1013, "lagged")
        }
        val session = openSession()
        assertTrue(
            "server close seen",
            awaitUntil { session.statusBanner == PageInkSession.DISCONNECTED },
        )

        // Within the 1 s backoff: a reconnect is pending right now.
        session.disconnect()
        Thread.sleep(2_500)

        assertEquals("the pending reconnect never opened a socket", 1, server.requestCount)
    }

    /**
     * #150: the server drops the socket. The banner says disconnected (not
     * "connecting", which reads as healthy) for the whole reconnect, carries
     * the technical cause, and clears only once the next socket welcomes.
     */
    @Test
    fun aDropShowsDisconnectedUntilTheNextWelcome() {
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.close(1013, "lagged")
        }
        val reconnected = CountDownLatch(1)
        openPage { _, type -> if (type == "subscribe") reconnected.countDown() }

        val session = openSession()
        assertTrue(
            "disconnected shown",
            awaitUntil { session.status?.kind == PageStatusKind.Disconnected },
        )
        assertEquals(PageInkSession.DISCONNECTED, session.statusBanner)
        assertTrue("cause given", session.status?.detail?.isNotBlank() == true)
        session.commitStroke(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))
        assertEquals("ink is kept, not refused, while disconnected (#390, #159)", 1, session.strokes.size)

        assertTrue("reconnected", reconnected.await(5, TimeUnit.SECONDS))
        assertTrue("banner cleared by the welcome", awaitUntil { session.status == null })
        session.disconnect()
    }

    /**
     * #150: a 401 on the upgrade is a stale token, not a dead network — one
     * forced refresh, and the page reconnects at once with the new token.
     */
    @Test
    fun anUnauthorizedUpgradeRefreshesTheTokenAndReconnects() {
        server.enqueue(MockResponse.Builder().code(401).build())
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }

        val session = openSession()

        assertTrue("connected with the refreshed token", awaitUntil { session.canEdit })
        assertEquals("no banner once live", null, session.status)
        assertEquals("one refused upgrade, one accepted", 2, server.requestCount)
        session.disconnect()
    }

    /**
     * #150 review: OIDC discovery throws when the identity provider is
     * unreachable. A refresh that cannot run must end in the session-expired
     * state, not escape the page's coroutine and crash the app.
     */
    @Test
    fun aRefreshThatThrowsEndsInSessionExpiredRatherThanACrash() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val throwingAuth =
            object : AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore) {
                override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean =
                    throw java.io.IOException("identity provider unreachable")
            }
        val client = OkHttpApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, throwingAuth)
        server.enqueue(MockResponse.Builder().code(401).build())
        try {
            val session = PageInkSession(client, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
            session.connect()

            assertTrue(
                "session expired shown",
                awaitUntil { session.status?.kind == PageStatusKind.SessionExpired },
            )
            session.commitStroke(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))
            assertEquals("editing carries on (#159)", 1, session.strokes.size)
            session.disconnect()
        } finally {
            client.shutdown()
        }
    }

    /**
     * #150, #159: when the refreshed token is refused too, the session is
     * gone. Sync pauses for a sign-in and the page stops reconnecting — every
     * further attempt would be refused the same way — while editing carries
     * on, and a 401 is never recorded as a rejection.
     */
    @Test
    fun aSessionTheRefreshCannotRenewStopsAndAsksForSignIn() {
        server.enqueue(MockResponse.Builder().code(401).build())
        server.enqueue(MockResponse.Builder().code(401).build())

        val session = openSession()

        assertTrue(
            "session expired shown",
            awaitUntil { session.status?.kind == PageStatusKind.SessionExpired },
        )
        assertEquals(PageInkSession.SESSION_EXPIRED, session.statusBanner)
        session.commitStroke(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))
        assertEquals("editing carries on", 1, session.strokes.size)
        val outbox = LocalStore.shared(context()).outbox.read
        assertEquals("the stroke is owed, not refused", 1, outbox.awaitingCount())
        assertTrue("a 401 is never a rejection", outbox.rejections().isEmpty())
        Thread.sleep(2_500)
        assertEquals("stopped after the refreshed token was refused", 2, server.requestCount)
        session.disconnect()
    }

    /**
     * #279 review: a batch in flight when the server drops the socket, and which
     * the server never stored, is re-committed on the next socket once it has
     * caught up — under the same `client_batch_id`, so a server that had stored it
     * would only re-echo it — and local state ends equal to the server's.
     */
    @Test
    fun anUnacknowledgedBatchIsRecommittedAfterReconnectAndConverges() {
        val firstCommit = AtomicReference<JSONObject>()
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }
        dropOnCommit(firstCommit)
        val recommits = CopyOnWriteArrayList<JSONObject>()
        enqueueEchoingPage(stored = null, commits = recommits)

        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })
        val stroke = Stroke(points = listOf(StrokePoint(10.0, 20.0, 0), StrokePoint(30.0, 25.0, 16)))
        session.commitStroke(stroke)

        assertTrue("re-committed on the new socket", awaitUntil(timeoutMs = 10_000) { recommits.size == 1 })
        assertTrue("confirmed by the echo", awaitUntil { session.pendingBatchCount == 0 })
        assertEquals(
            "same client_batch_id, so the server can dedupe",
            firstCommit.get().getString("client_batch_id"),
            recommits[0].getString("client_batch_id"),
        )
        assertEquals("local ink equals the server's", listOf(stroke.id), session.strokes.map { it.id })
        Thread.sleep(300)
        assertEquals("sent exactly once more", 1, recommits.size)

        session.disconnect()
    }

    /**
     * The other half: the server *did* store the batch before the socket
     * dropped, so the reconnect's replay confirms it and nothing is re-sent.
     */
    @Test
    fun aBatchTheServerStoredIsConfirmedByTheReplayAndNotResent() {
        val stored = AtomicReference<JSONObject>()
        openPage { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }
        dropOnCommit(stored)
        val recommits = CopyOnWriteArrayList<JSONObject>()
        enqueueEchoingPage(stored = stored, commits = recommits)

        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })
        val stroke = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0)))
        session.commitStroke(stroke)

        assertTrue(
            "confirmed by the replay",
            awaitUntil(timeoutMs = 10_000) { session.pendingBatchCount == 0 && session.canEdit },
        )
        Thread.sleep(300)
        assertEquals("nothing re-sent", 0, recommits.size)
        assertEquals("local ink equals the server's", listOf(stroke.id), session.strokes.map { it.id })

        session.disconnect()
    }

    /**
     * #159, acceptance 1 and 3 at the session level: a stroke accepted while
     * the server is down outlives its session. The next session — a restarted
     * app opens a new one — draws it from the device's copy before any answer,
     * sends it once, and after the echo a third session sends nothing.
     */
    @Test
    fun aStrokeKeptByOneSessionIsSentOnceByTheNextAndNeverAgain() {
        server.enqueue(MockResponse(code = 503))
        val first = openSession()
        assertTrue("offline", awaitUntil { first.status?.kind == PageStatusKind.Disconnected })
        val stroke = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0)))
        first.commitStroke(stroke)
        first.disconnect()
        assertEquals(
            "owed",
            1,
            LocalStore
                .shared(context())
                .outbox.read
                .awaitingCount(),
        )

        val commits = CopyOnWriteArrayList<JSONObject>()
        enqueueEchoingPage(stored = null, commits = commits)
        val second = openSession()
        assertEquals("drawn from the device's copy at once", listOf(stroke.id), second.strokes.map { it.id })
        assertTrue("acknowledged", awaitUntil(timeoutMs = 10_000) { second.pendingBatchCount == 0 && second.canEdit })
        assertEquals("sent once", 1, commits.size)
        assertEquals(
            "nothing owed",
            0,
            LocalStore
                .shared(context())
                .outbox.read
                .awaitingCount(),
        )
        second.disconnect()

        val again = CopyOnWriteArrayList<JSONObject>()
        enqueueEchoingPage(stored = AtomicReference(commits.single()), commits = again)
        val third = openSession()
        assertTrue("caught up", awaitUntil { third.canEdit })
        Thread.sleep(300)
        assertEquals("nothing sent again", 0, again.size)
        assertEquals("drawn once", listOf(stroke.id), third.strokes.map { it.id })
        third.disconnect()
    }

    /**
     * #159, acceptance 10: an upgrade answered 403 (not found, or not this
     * user's) is a definitive refusal. What the page owes is recorded as
     * refused — kept, drawn, and never retried or counted as synced.
     */
    @Test
    fun aForbiddenUpgradeIsRecordedAsARejection() {
        val stroke = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0)))
        LocalStore.shared(context()).outbox.enqueue("page_1", Edit.Batch("batch_owed", listOf(stroke)))
        server.enqueue(MockResponse(code = 403))

        val session = openSession()

        assertTrue("refused", awaitUntil { session.status?.kind == PageStatusKind.Rejected })
        assertEquals("kept and drawn", listOf(stroke.id), session.strokes.map { it.id })
        val rejection =
            LocalStore
                .shared(context())
                .outbox.read
                .rejections()
                .single()
        assertEquals("page_1", rejection.pageId)
        assertEquals(1, rejection.strokeCount)
        Thread.sleep(2_500)
        assertEquals("not retried", 1, server.requestCount)
        session.disconnect()
    }

    /**
     * #159: the server never echoes an add whose every stroke is erased
     * (delete wins). A batch this device still owes whose stroke another
     * device erased meanwhile is settled by the replayed tombstone, not
     * re-sent forever.
     */
    @Test
    fun anOwedBatchWhoseStrokesWereErasedElsewhereIsSettledByTheTombstone() {
        val stroke = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0)), id = "erased-elsewhere")
        LocalStore.shared(context()).outbox.enqueue("page_1", Edit.Batch("batch_owed", listOf(stroke)))
        openPage { webSocket, type ->
            if (type == "subscribe") {
                webSocket.send(
                    """{"type":"page-replay","page_id":"page_1","last_seq":0,"batches":[],""" +
                        """"tombstones":[{"revision":3,"client_mutation_id":"other-device-erase","stroke_ids":["erased-elsewhere"]}]}""",
                )
            }
        }

        val session = openSession()

        assertTrue("settled", awaitUntil { session.canEdit && session.pendingBatchCount == 0 })
        assertEquals("not drawn", emptyList<String>(), session.strokes.map { it.id })
        assertEquals(
            "nothing owed",
            0,
            LocalStore
                .shared(context())
                .outbox.read
                .awaitingCount(),
        )
        session.disconnect()
    }

    /**
     * #390: strokes drawn while the socket is down are held, then sent in the
     * order they were drawn once the next socket has caught up — once each.
     */
    @Test
    fun strokesDrawnWhileDisconnectedAreSentInOrderOnceTheNextSocketCatchesUp() {
        val first = AtomicReference<WebSocket>()
        openPage(first) { webSocket, type ->
            if (type == "subscribe") webSocket.send("""{"type":"synced","last_seq":0}""")
        }
        val commits = CopyOnWriteArrayList<JSONObject>()
        enqueueEchoingPage(stored = null, commits = commits)

        val session = openSession()
        assertTrue("page channel caught up", awaitUntil { session.canEdit })
        first.get().close(1013, "lagged")
        assertTrue("disconnected", awaitUntil { !session.canEdit })

        val strokes =
            listOf(
                Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))),
                Stroke(points = listOf(StrokePoint(3.0, 4.0, 0))),
            )
        onMain { strokes.forEach(session::commitStroke) }
        assertEquals("held strokes are drawn", strokes.map { it.id }, onMain { session.strokes.map { it.id } })

        assertTrue("both sent on the new socket", awaitUntil(timeoutMs = 10_000) { commits.size == 2 })
        assertEquals(
            "in the order they were drawn",
            strokes.map { it.id },
            commits.map { it.getJSONArray("strokes").getJSONObject(0).getString("id") },
        )
        assertTrue("confirmed by the echoes", awaitUntil { onMain { session.pendingBatchCount == 0 } })
        Thread.sleep(300)
        assertEquals("each sent once", 2, commits.size)
        assertEquals(strokes.map { it.id }, onMain { session.strokes.map { it.id } })

        session.disconnect()
    }

    /**
     * Makes the *first* enqueued page drop its socket (1013) on the first
     * `commit-batch`, recording the commit — the in-flight batch a lagged or
     * failed connection leaves unacknowledged. MockWebServer serves responses
     * in order, so this wraps the page [openPage] just enqueued.
     */
    private fun dropOnCommit(commit: AtomicReference<JSONObject>) {
        droppingCommit = commit
    }

    // Read by [openPage]'s listener: when set, a `commit-batch` is recorded and
    // answered by closing the socket instead of by the test's handler.
    private var droppingCommit: AtomicReference<JSONObject>? = null

    /**
     * A page that replays [stored] (a commit the server kept) as seq 1 when
     * given, and echoes every commit it receives into
     * [commits] — the server the session reconnects to.
     */
    private fun enqueueEchoingPage(
        stored: AtomicReference<JSONObject>?,
        commits: MutableList<JSONObject>,
    ) {
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
                        override fun onOpen(
                            webSocket: WebSocket,
                            response: okhttp3.Response,
                        ) {
                            val head = if (stored?.get() != null) 1 else 0
                            webSocket.send("""{"type":"welcome","session_id":"me-again","last_seq":$head}""")
                        }

                        override fun onMessage(
                            webSocket: WebSocket,
                            text: String,
                        ) {
                            val message = JSONObject(text)
                            when (message.getString("type")) {
                                "subscribe" -> {
                                    val kept = stored?.get()
                                    val batches =
                                        if (kept == null) {
                                            ""
                                        } else {
                                            JSONObject()
                                                .put("seq", 1)
                                                .put("client_batch_id", kept.getString("client_batch_id"))
                                                .put("strokes", kept.getJSONArray("strokes"))
                                                .toString()
                                        }
                                    val head = if (kept == null) 0 else 1
                                    webSocket.send(
                                        """{"type":"page-replay","page_id":"page_1","last_seq":$head,""" +
                                            """"batches":[$batches],"tombstones":[]}""",
                                    )
                                }
                                "commit-batch" -> {
                                    commits += message
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
                        }
                    },
                ).build(),
        )
    }

    private fun context() = InstrumentationRegistry.getInstrumentation().targetContext

    private fun localSync(): SessionSync = SessionSync(LocalStore.shared(context()))

    private fun openSession(): PageInkSession {
        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main), sync = localSync())
        session.connect()
        return session
    }

    /**
     * Enqueue a page channel that welcomes and delegates every client message
     * to [onMessage]. Nothing is answered on its own: a test that wants the
     * page live replies to `subscribe` itself.
     */
    private fun openPage(
        socket: AtomicReference<WebSocket>? = null,
        onMessage: (WebSocket, String) -> Unit,
    ) {
        server.enqueue(
            MockResponse
                .Builder()
                .webSocketUpgrade(
                    object : AnsweringCloses() {
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
                            val dropping = droppingCommit
                            if (type == "commit-batch" && dropping != null) {
                                dropping.set(JSONObject(text))
                                webSocket.close(1013, "lagged")
                                return
                            }
                            val echoing = echoingCommits
                            if (type == "commit-batch" && echoing != null) {
                                val commit = JSONObject(text)
                                webSocket.send(
                                    JSONObject()
                                        .put("type", "stroke-batch")
                                        .put("seq", 2)
                                        .put("client_batch_id", commit.getString("client_batch_id"))
                                        .put("strokes", commit.getJSONArray("strokes"))
                                        .toString(),
                                )
                                // Recorded once the echo is queued, so a test
                                // that waits on it sends its next frame after.
                                echoing += commit
                            }
                            onMessage(webSocket, type)
                        }
                    },
                ).build(),
        )
    }

    /**
     * Echo every `commit-batch` that reaches the page from now on as the
     * server would, at seq 2 — between the other device's batches at 1 and 3.
     */
    private fun echoCommits(commits: MutableList<JSONObject>) {
        echoingCommits = commits
    }

    // Read by [openPage]'s listener: when set, a `commit-batch` is recorded and
    // echoed at seq 2 before the test's own handler runs.
    private var echoingCommits: MutableList<JSONObject>? = null

    /** Another device's stroke as the server fans it out. */
    private fun remoteBatch(
        seq: Int,
        id: String,
    ): String =
        """{"type":"stroke-batch","seq":$seq,"client_batch_id":"remote-batch-$seq","strokes":[{"id":"$id",""" +
            """"style":{"tool_kind":"solid_round","style_version":2,"parameters":{"color":"#1565C0",""" +
            """"width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":$seq.0,"y":9.0,"t":0}]}]}"""

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
            "style_version": 2,
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

    // Session state is written on the main thread; read and drive it there too.
    private fun <T> onMain(block: () -> T): T {
        val result = AtomicReference<T>()
        InstrumentationRegistry.getInstrumentation().runOnMainSync { result.set(block()) }
        return result.get()
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
