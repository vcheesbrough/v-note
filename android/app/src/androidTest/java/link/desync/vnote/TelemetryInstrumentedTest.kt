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
import link.desync.vnote.telemetry.OtlpHttpTransport
import link.desync.vnote.telemetry.Severity
import link.desync.vnote.telemetry.Telemetry
import link.desync.vnote.telemetry.TelemetryRuntime
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
        assertEquals(BuildConfig.TELEMETRY_EXPORT, Telemetry.isExporting)
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
    fun anExportIsGzippedJsonToTheAndroidIngressWithTheBearerToken() {
        server.enqueue(MockResponse(code = 200))
        server.enqueue(MockResponse(code = 200))
        val runtime = TelemetryRuntime(OtlpHttpTransport(baseUrl(), accessToken = tokenStore::accessToken))
        val screen = runtime.startScreen("screen.library")
        runtime.span("realtime.connect", screen).end()
        runtime.log(Severity.Warn, "page channel error", screen)

        runtime.exportNow()

        val traces = server.takeRequest(5, TimeUnit.SECONDS)!!
        assertEquals("POST", traces.method)
        assertEquals("/otlp/android/v1/traces", traces.target)
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
        assertEquals("/otlp/android/v1/logs", logs.target)
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
