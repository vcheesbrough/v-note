package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.telemetry.ConfigFetch
import link.desync.vnote.telemetry.ExportOutcome
import link.desync.vnote.telemetry.OtlpHttpTransport
import link.desync.vnote.telemetry.Severity
import link.desync.vnote.telemetry.Signal
import link.desync.vnote.telemetry.Telemetry
import link.desync.vnote.telemetry.TelemetryRuntime
import link.desync.vnote.telemetry.Transport
import link.desync.vnote.telemetry.parseTraceparent
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okio.Buffer
import okio.GzipSource
import okio.buffer
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.TimeUnit

// #406: what CI can prove about Android client telemetry without a real
// collector. Live delivery to Tempo/Loki is a deploy-dev smoke (#179); the
// sidecar's handling of an `android` export is covered by #354's e2e.
@RunWith(AndroidJUnit4::class)
class TelemetryInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: OkHttpApiClient? = null

    private val traceparent = Regex("00-[0-9a-f]{32}-[0-9a-f]{16}-01")

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
    }

    @After
    fun tearDown() {
        apiClient?.shutdown()
        server.close()
    }

    private fun baseUrl() = server.url("/").toString().removeSuffix("/")

    private fun client(): OkHttpApiClient {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val authRepository = AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore)
        return OkHttpApiClient(baseUrl(), tokenStore, authRepository).also { apiClient = it }
    }

    private fun RecordedRequest.gunzippedJson(): JSONObject {
        val compressed = Buffer().write(checkNotNull(body) { "export had no body" })
        return JSONObject(GzipSource(compressed).buffer().readUtf8())
    }

    @Test
    fun theApplicationInstallsTheExporterForThisFlavor() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext
        assertTrue("manifest names the Application subclass", application is VNoteApplication)
        // Nothing exports until the server has configured it (#439), and this
        // process has no session to ask with.
        assertFalse(Telemetry.isExporting)
    }

    @Test
    fun restCallsCarryAValidTraceparentBesideTheRequestId() {
        server.enqueue(
            MockResponse
                .Builder()
                .code(200)
                .body("""{"sub":"user-1","email":"user@example.com"}""")
                .addHeader("Content-Type", "application/json")
                .build(),
        )

        runBlocking { assertTrue(client().fetchMe().isSuccess) }

        val request = server.takeRequest(5, TimeUnit.SECONDS)!!
        val header = request.headers["traceparent"].orEmpty()
        assertTrue("traceparent '$header'", traceparent.matches(header))
        assertNotNull("parses as a real, non-zero context", parseTraceparent(header))
        assertTrue("X-Request-Id is kept", request.headers["X-Request-Id"].orEmpty().startsWith("android_"))
    }

    // #453, on a real stack: a request and the failure log written for it are
    // located at the API method that made the request (its `withContext`
    // body), not at the shared helper that ran it or the one that logged it.
    @Test
    fun aRequestAndItsFailureLogAreLocatedAtTheApiMethod() {
        val exported = java.util.Collections.synchronizedList(mutableListOf<Pair<Signal, JSONObject>>())
        val recording =
            TelemetryRuntime(
                object : Transport {
                    override fun isReady() = true

                    override fun credentialId() = 1

                    override fun fetchConfig() = ConfigFetch.Configured("https://ingest.example")

                    override fun send(
                        endpoint: String,
                        signal: Signal,
                        body: String,
                    ): ExportOutcome {
                        exported += signal to JSONObject(body)
                        return ExportOutcome.Accepted
                    }
                },
            )
        val installed = Telemetry.runtime
        Telemetry.install(recording)
        try {
            server.enqueue(MockResponse(code = 500))
            runBlocking { assertTrue(client().deletePage("p1").isFailure) }
            recording.exportNow()
        } finally {
            Telemetry.install(installed)
        }

        fun items(
            signal: Signal,
            outer: String,
            scope: String,
            list: String,
        ) = synchronized(exported) { exported.filter { it.first == signal }.map { it.second } }.flatMap { body ->
            val array =
                body
                    .getJSONArray(outer)
                    .getJSONObject(0)
                    .getJSONArray(scope)
                    .getJSONObject(0)
                    .getJSONArray(list)
            (0 until array.length()).map(array::getJSONObject)
        }

        fun JSONObject.attribute(key: String): String {
            val attributes = getJSONArray("attributes")
            return (0 until attributes.length())
                .map(attributes::getJSONObject)
                .single { it.getString("key") == key }
                .getJSONObject("value")
                .let { it.optString("stringValue", it.optString("intValue")) }
        }

        val span = items(Signal.Traces, "resourceSpans", "scopeSpans", "spans").single { it.getString("name") == "http.client" }
        val log = items(Signal.Logs, "resourceLogs", "scopeLogs", "logRecords").single()
        for ((what, item) in listOf("span" to span, "log" to log)) {
            val function = item.attribute("code.function.name")
            assertTrue("$what located at deletePage, was $function", function.contains("OkHttpApiClient\$deletePage"))
            assertEquals("link/desync/vnote/api/OkHttpApiClient.kt", item.attribute("code.file.path"))
            assertTrue(item.attribute("code.line.number").toInt() > 0)
        }
    }

    // Unlike a browser, OkHttp can put headers on a WebSocket upgrade, which is
    // how the server parents a realtime connection to the app's trace.
    @Test
    fun theRealtimeUpgradeCarriesATraceparent() {
        server.enqueue(MockResponse(code = 404))
        client().openLibrarySocket(
            object : LibraryEventListener {
                override fun onEvent(event: LibraryEvent) = Unit

                override fun onError(message: String) = Unit

                override fun onClosed() = Unit
            },
        )

        val upgrade = server.takeRequest(5, TimeUnit.SECONDS)!!
        assertEquals("/api/realtime", upgrade.target)
        assertTrue(traceparent.matches(upgrade.headers["traceparent"].orEmpty()))
    }

    @Test
    fun anExportIsGzippedJsonToTheConfiguredIngestWithTheBearerToken() {
        // The configuration, then the two exports — to the endpoint the server
        // named, which here is the same mock server.
        server.enqueue(
            MockResponse
                .Builder()
                .code(200)
                .body("""{"endpoint":"${baseUrl()}","access_token":"ignored","expires_at":4102444800}""")
                .addHeader("Content-Type", "application/json")
                .build(),
        )
        server.enqueue(MockResponse(code = 200))
        server.enqueue(MockResponse(code = 200))
        val runtime = TelemetryRuntime(OtlpHttpTransport(baseUrl(), accessToken = tokenStore::accessToken))
        val screen = runtime.startScreen("screen.library")
        runtime.span("realtime.connect", screen).end()
        runtime.log(Severity.Warn, "page channel error", screen)

        runtime.exportNow()

        val config = server.takeRequest(5, TimeUnit.SECONDS)!!
        assertEquals("GET", config.method)
        assertEquals("/api/telemetry/config", config.target)
        assertEquals("Bearer access-token", config.headers["Authorization"])

        val traces = server.takeRequest(5, TimeUnit.SECONDS)!!
        assertEquals("POST", traces.method)
        assertEquals("/v1/traces", traces.target)
        assertEquals("Bearer access-token", traces.headers["Authorization"])
        assertEquals("gzip", traces.headers["Content-Encoding"])
        assertTrue(traces.headers["Content-Type"].orEmpty().startsWith("application/json"))
        val spans =
            traces
                .gunzippedJson()
                .getJSONArray("resourceSpans")
                .getJSONObject(0)
                .getJSONArray("scopeSpans")
                .getJSONObject(0)
                .getJSONArray("spans")
        assertEquals(2, spans.length())
        // The export itself is never traced: no span of its own, no header.
        assertNull(traces.headers["traceparent"])

        val logs = server.takeRequest(5, TimeUnit.SECONDS)!!
        assertEquals("/v1/logs", logs.target)
        assertEquals("Bearer access-token", logs.headers["Authorization"])
        val record =
            logs
                .gunzippedJson()
                .getJSONArray("resourceLogs")
                .getJSONObject(0)
                .getJSONArray("scopeLogs")
                .getJSONObject(0)
                .getJSONArray("logRecords")
                .getJSONObject(0)
        assertEquals(screen.traceId.hex, record.getString("traceId"))
    }

    @Test
    fun nothingIsExportedWithoutASession() {
        tokenStore.clear()
        val runtime = TelemetryRuntime(OtlpHttpTransport(baseUrl(), accessToken = tokenStore::accessToken))
        runtime.log(Severity.Info, "signed out")

        runtime.exportNow()

        assertEquals(0, server.requestCount)
    }
}
