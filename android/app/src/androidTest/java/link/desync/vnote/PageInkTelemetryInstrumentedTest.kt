package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.PageInkSession
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.telemetry.ExportOutcome
import link.desync.vnote.telemetry.Signal
import link.desync.vnote.telemetry.Telemetry
import link.desync.vnote.telemetry.TelemetryRuntime
import link.desync.vnote.telemetry.Transport
import link.desync.vnote.telemetry.parseTraceparent
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
import java.util.Collections
import java.util.concurrent.TimeUnit

// #406: the spans a real page session produces, through the real OkHttp client
// and page channel, against a MockWebServer standing in for the server.
@RunWith(AndroidJUnit4::class)
class PageInkTelemetryInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var apiClient: OkHttpApiClient
    private lateinit var installed: TelemetryRuntime
    private val exported = Collections.synchronizedList(mutableListOf<String>())
    private val recording =
        TelemetryRuntime(
            object : Transport {
                override fun isReady() = true

                override fun send(
                    signal: Signal,
                    body: String,
                ): ExportOutcome {
                    if (signal == Signal.Traces) exported += body
                    return ExportOutcome.Accepted
                }
            },
        )

    @Before
    fun setUp() {
        installed = Telemetry.runtime
        Telemetry.install(recording)
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
        Telemetry.install(installed)
        apiClient.shutdown()
        tokenStore.clear()
        server.close()
    }

    private fun spans(): List<JSONObject> {
        recording.exportNow()
        return exported.flatMap { body ->
            val array =
                JSONObject(body)
                    .getJSONArray("resourceSpans")
                    .getJSONObject(0)
                    .getJSONArray("scopeSpans")
                    .getJSONObject(0)
                    .getJSONArray("spans")
            (0 until array.length()).map(array::getJSONObject)
        }
    }

    private fun List<JSONObject>.named(name: String) = single { it.getString("name") == name }

    @Test
    fun aPageSessionTracesConnectSubscribeAndACommittedStrokeInOneTrace() {
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
                            val message = JSONObject(text)
                            when (message.getString("type")) {
                                "subscribe" -> webSocket.send("""{"type":"synced","last_seq":0}""")
                                "acquire-lease" -> webSocket.send("""{"type":"lease-granted"}""")
                                "commit-batch" ->
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
                    },
                ).build(),
        )

        val session = PageInkSession(apiClient, "page_1", CoroutineScope(Dispatchers.Main))
        session.connect()
        assertTrue("lease granted", awaitUntil { session.canEdit })
        session.commitStroke(Stroke(points = listOf(StrokePoint(1.0, 2.0, 0), StrokePoint(3.0, 4.0, 120))))
        assertTrue("stroke confirmed", awaitUntil { session.strokes.size == 1 && session.pendingBatchCount == 0 })
        val upgrade = server.takeRequest(5, TimeUnit.SECONDS)!!
        session.disconnect()

        val spans = spans()
        val screen = spans.named("screen.page")
        val trace = screen.getString("traceId")
        val screenId = screen.getString("spanId")
        for (name in listOf("realtime.connect", "realtime.subscribe", "http.client", "ink.stroke")) {
            val span = spans.named(name)
            assertEquals("$name in the page's trace", trace, span.getString("traceId"))
            assertEquals("$name under the page's root", screenId, span.getString("parentSpanId"))
        }

        // The upgrade's traceparent names its http.client span, which is how
        // the server's connection span joins this trace.
        val upgradeSpan = spans.named("http.client")
        val sent = parseTraceparent(upgrade.headers["traceparent"])!!
        assertEquals(trace, sent.traceId.hex)
        assertEquals(upgradeSpan.getString("spanId"), sent.spanId.hex)

        // The ink pipeline: capture and commit beneath the stroke, all ended.
        val stroke = spans.named("ink.stroke")
        for (name in listOf("ink.capture", "ink.commit")) {
            assertEquals(stroke.getString("spanId"), spans.named(name).getString("parentSpanId"))
        }
        assertTrue(spans.none { it.has("status") && it.getString("name").startsWith("ink.") })
    }

    private fun awaitUntil(
        timeoutMs: Long = 5_000,
        condition: () -> Boolean,
    ): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return true
            Thread.sleep(25)
        }
        return condition()
    }
}
