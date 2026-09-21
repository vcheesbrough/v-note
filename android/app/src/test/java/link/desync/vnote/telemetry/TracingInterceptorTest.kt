package link.desync.vnote.telemetry

import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.SocketEffect
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.IOException

class TracingInterceptorTest {
    private val bodies = mutableListOf<String>()
    private val runtime =
        TelemetryRuntime(
            object : Transport {
                override fun isReady() = true

                override fun send(
                    signal: Signal,
                    body: String,
                ): ExportOutcome {
                    bodies += body
                    return ExportOutcome.Accepted
                }
            },
        )
    private val server = MockWebServer()
    private val http = OkHttpClient.Builder().addInterceptor(TracingInterceptor { runtime }).build()

    @Before
    fun setUp() = server.start()

    @After
    fun tearDown() = server.close()

    private fun call(parent: SpanContext) =
        http
            .newCall(
                Request
                    .Builder()
                    .url(server.url("/api/pages/p1"))
                    .traced("/api/pages/{page_id}", parent)
                    .build(),
            ).execute()
            .close()

    private fun onlySpan(): JSONObject {
        runtime.exportNow()
        val spans =
            JSONObject(bodies.single())
                .getJSONArray("resourceSpans")
                .getJSONObject(0)
                .getJSONArray("scopeSpans")
                .getJSONObject(0)
                .getJSONArray("spans")
        assertEquals(1, spans.length())
        return spans.getJSONObject(0)
    }

    private fun JSONObject.attribute(key: String): JSONObject {
        val attributes = getJSONArray("attributes")
        return (0 until attributes.length())
            .map(attributes::getJSONObject)
            .single { it.getString("key") == key }
            .getJSONObject("value")
    }

    @Test
    fun aSuccessfulCallIsAClientSpanUnderItsTaggedParentNamedByTemplate() {
        server.enqueue(MockResponse(code = 200))
        val parent = SpanContext(TraceId.random(), SpanId.random())

        call(parent)

        val header = server.takeRequest().headers["traceparent"]
        val span = onlySpan()
        assertEquals("http.client", span.getString("name"))
        assertEquals(3, span.getInt("kind"))
        assertEquals(parent.traceId.hex, span.getString("traceId"))
        assertEquals(parent.spanId.hex, span.getString("parentSpanId"))
        // The header names this span, so the server's span is its child.
        assertEquals("00-${parent.traceId.hex}-${span.getString("spanId")}-01", header)
        assertEquals("/api/pages/{page_id}", span.attribute("url.template").getString("stringValue"))
        assertEquals("GET", span.attribute("http.request.method").getString("stringValue"))
        assertEquals("200", span.attribute("http.response.status_code").get("intValue"))
        assertFalse(span.has("status"))
    }

    @Test
    fun anErrorStatusFailsTheSpan() {
        server.enqueue(MockResponse(code = 503))

        call(runtime.screen())

        val span = onlySpan()
        assertEquals(2, span.getJSONObject("status").getInt("code"))
        assertEquals("HTTP 503", span.getJSONObject("status").getString("message"))
    }

    @Test
    fun aTransportFailureFailsTheSpanAndStillReachesTheCaller() {
        server.enqueue(MockResponse.Builder().onRequestStart(SocketEffect.CloseSocket()).build())

        val thrown = runCatching { call(runtime.screen()) }.exceptionOrNull()

        assertTrue("got $thrown", thrown is IOException)
        val status = onlySpan().getJSONObject("status")
        assertEquals(2, status.getInt("code"))
    }
}
