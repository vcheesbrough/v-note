package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.HttpStatusException
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.api.ServerUnreachableException
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
import link.desync.vnote.sync.PageCreates
import link.desync.vnote.sync.SyncEngine
import link.desync.vnote.sync.SyncStatus
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
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
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

/**
 * #651: the sync engine's own retry rules, with no screen. Nothing it owes is
 * ever given up on; a page that cannot succeed does not stand in the way of
 * the ones that can; and a returning network is acted on at once rather than
 * after the backoff step it interrupts.
 */
@RunWith(AndroidJUnit4::class)
class SyncEngineInstrumentedTest {
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val store get() = LocalStore.shared(context)
    private val server = MockWebServer()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private lateinit var tokenStore: TokenStore
    private lateinit var client: SwitchableApiClient
    private lateinit var status: SyncStatus
    private lateinit var engine: SyncEngine

    // Pages the server answers 410 for: permanently deleted.
    private val purged = ConcurrentHashMap.newKeySet<String>()
    private val upgrades = ConcurrentHashMap<String, AtomicInteger>()
    private val stored = ConcurrentHashMap<String, CopyOnWriteArrayList<String>>()

    @Before
    fun setUp() {
        server.dispatcher = dispatcher()
        server.start(InetAddress.getByName("127.0.0.1"), 0)
        tokenStore = TokenStore(context)
        tokenStore.clear()
        tokenStore.saveTokens("access-token", "refresh-token", System.currentTimeMillis() / 1000 + 3600)
        val provider =
            object : AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore) {
                override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean = true
            }
        client = SwitchableApiClient(OkHttpApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, provider))
        onMain {
            status = SyncStatus(store)
            engine = SyncEngine(client, store, status, scope)
        }
    }

    @After
    fun tearDown() {
        onMain { engine.stop() }
        scope.cancel()
        client.shutdown()
        tokenStore.clear()
        server.close()
    }

    /**
     * More pages are permanently deleted than the engine has background
     * slots, each owing an edit that can never land. They are retried for as
     * long as anyone looks — and a healthy page's edit still gets through,
     * rather than waiting behind them forever.
     */
    @Test
    fun pagesThatCanNeverSucceedAreRetriedForeverWithoutStarvingTheRest() {
        val gone = listOf("page_gone_1", "page_gone_2", "page_gone_3", "page_gone_4")
        purged += gone
        onMain {
            store.library.write((gone + HEALTHY).map(::page))
            gone.forEach { owe(it) }
            owe(HEALTHY)
            engine.start()
        }

        assertTrue("the healthy page's edit lands", awaitUntil(20_000) { stored[HEALTHY]?.size == 1 })
        assertTrue(
            "every deleted page goes on being retried, past the backoff's first steps",
            awaitUntil(60_000) { gone.all { (upgrades[it]?.get() ?: 0) >= 3 } },
        )
        assertEquals(
            "their edits are all still owed",
            gone.toSet(),
            onMain {
                store.outbox.read
                    .pagesWithWork()
                    .toSet()
            },
        )
        assertEquals("and each is offered as a new page", gone.toSet(), onMain { status.gonePages.map { it.pageId }.toSet() })
        assertTrue("nothing was stored for them", gone.all { stored[it].isNullOrEmpty() })
    }

    /**
     * A page that fails is asked once per backoff step, not once for every
     * pass the engine makes within the step: a server that is down, or a
     * page that can never succeed, is not hammered. Attempts fall at 0, 1
     * and 3 s, and the next is due at 7 s.
     */
    @Test
    fun aFailedPageIsTriedOncePerBackoffStep() {
        purged += GONE
        onMain {
            store.library.write(listOf(page(GONE)))
            owe(GONE)
            engine.start()
        }

        assertTrue("retried on the backoff", awaitUntil(10_000) { (upgrades[GONE]?.get() ?: 0) >= 3 })
        Thread.sleep(SETTLE_MILLIS)

        assertEquals("three steps, three attempts", 3, upgrades.getValue(GONE).get())
    }

    /**
     * With the server or the network away, the first create that cannot be
     * sent ends the pass: the others would only fail the same way, one
     * timeout after another, ahead of the replays behind them.
     */
    @Test
    fun aCreateThatCannotReachTheServerEndsThePass() {
        val creates = CountingCreates { Result.failure(ServerUnreachableException(java.io.IOException("no route"))) }
        onMain { LOCAL_PAGES.forEach { store.outbox.createPage(page(it)) } }

        val allLanded = onMain { runBlocking { PageCreates(creates, store, status).push {} } }

        assertEquals(false, allLanded)
        assertEquals("only the first was tried", LOCAL_PAGES.take(1), creates.asked)
        assertEquals(
            "all three are still owed",
            3,
            onMain {
                store.outbox.read
                    .pendingCreates()
                    .size
            },
        )
    }

    /**
     * A create the server answers with an error is that create's failure:
     * it stays owed, and the ones behind it are still sent.
     */
    @Test
    fun aCreateTheServerAnswersWithAnErrorDoesNotHoldTheOthersBack() {
        val creates =
            CountingCreates { id ->
                if (id == LOCAL_PAGES.first()) Result.failure(HttpStatusException(400, "Bad Request", null)) else Result.success(page(id))
            }
        onMain { LOCAL_PAGES.forEach { store.outbox.createPage(page(it)) } }
        val landed = mutableListOf<String>()

        val allLanded = onMain { runBlocking { PageCreates(creates, store, status).push { landed += it.id } } }

        assertEquals("one is still owed", false, allLanded)
        assertEquals("every create was tried", LOCAL_PAGES, creates.asked)
        assertEquals("the others landed", LOCAL_PAGES.drop(1), landed)
        assertEquals(
            "the refused one stays owed",
            LOCAL_PAGES.take(1),
            onMain {
                store.outbox.read
                    .pendingCreates()
                    .map { it.pageId }
            },
        )
    }

    // The real client, but for creates: answered by [answer], and counted.
    private inner class CountingCreates(
        private val answer: (String) -> Result<PageSummary>,
    ) : ApiClient by client {
        val asked = CopyOnWriteArrayList<String>()

        override suspend fun createPage(
            title: String?,
            paper: Paper,
            id: String?,
        ): Result<PageSummary> {
            asked += checkNotNull(id)
            return answer(id)
        }
    }

    /**
     * The server cannot be found for long enough that the next retry is 16 s
     * away. When the network returns, [SyncEngine.retryNow] sends what is
     * owed at once — well inside that step.
     */
    @Test
    fun aReturningNetworkIsActedOnAtOnceNotAfterTheBackoff() {
        client.dnsDown = true
        onMain {
            store.library.write(listOf(page(HEALTHY)))
            owe(HEALTHY)
            engine.start()
        }
        // Attempts at 0, 1, 3, 7 and 15 s: after the fifth, the next is 16 s on.
        assertTrue("retried on the backoff", awaitUntil(30_000) { client.pageSocketsRefused >= 5 })
        assertEquals("still owed", 1, onMain { store.outbox.read.unsyncedCount() })

        client.dnsDown = false
        onMain { engine.retryNow() }

        assertTrue("sent without waiting out the step", awaitUntil(RETRY_NOW_WITHIN_MILLIS) { stored[HEALTHY]?.size == 1 })
        assertTrue("nothing owed", awaitUntil(5_000) { onMain { store.outbox.read.unsyncedCount() } == 0 })
        assertEquals("reachable again", null, onMain { status.problem })
    }

    private fun owe(pageId: String) {
        store.outbox.enqueue(pageId, Edit.Batch("batch_$pageId", listOf(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0))))))
    }

    private fun page(id: String) = PageSummary(id = id, title = "", createdAt = STAMP, updatedAt = STAMP)

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.url.encodedPath
                if (!path.startsWith("/api/pages/") || !path.endsWith("/realtime")) return MockResponse(code = 404)
                val pageId = path.removePrefix("/api/pages/").removeSuffix("/realtime")
                upgrades.getOrPut(pageId) { AtomicInteger() }.incrementAndGet()
                return if (pageId in
                    purged
                ) {
                    MockResponse(code = 410)
                } else {
                    MockResponse.Builder().webSocketUpgrade(pageChannel(pageId)).build()
                }
            }
        }

    // Welcomes, has nothing to replay, and stores and echoes every commit.
    private fun pageChannel(pageId: String) =
        object : WebSocketListener() {
            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                webSocket.send("""{"type":"welcome","session_id":"s","last_seq":0}""")
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                val message = JSONObject(text)
                when (message.getString("type")) {
                    "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                    "commit-batch" -> {
                        val id = message.getString("client_batch_id")
                        stored.getOrPut(pageId) { CopyOnWriteArrayList() }.addIfAbsent(id)
                        webSocket.send(
                            JSONObject()
                                .put("type", "stroke-batch")
                                .put("seq", 1)
                                .put("client_batch_id", id)
                                .put("strokes", message.getJSONArray("strokes"))
                                .toString(),
                        )
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

    private fun <T> onMain(block: () -> T): T {
        var result: Result<T>? = null
        InstrumentationRegistry.getInstrumentation().runOnMainSync { result = runCatching(block) }
        return checkNotNull(result).getOrThrow()
    }

    private fun awaitUntil(
        timeoutMs: Long,
        condition: () -> Boolean,
    ): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return true
            Thread.sleep(25)
        }
        return condition()
    }

    private companion object {
        const val HEALTHY = "page_healthy"
        const val GONE = "page_gone"
        val LOCAL_PAGES = listOf("page_local_1", "page_local_2", "page_local_3")

        // Well inside the 4 s between the third attempt and the fourth.
        const val SETTLE_MILLIS = 1_500L
        const val STAMP = "2026-11-28T00:00:00Z"

        // Far inside the 16 s step the retry cuts short.
        const val RETRY_NOW_WITHIN_MILLIS = 5_000L
    }
}
