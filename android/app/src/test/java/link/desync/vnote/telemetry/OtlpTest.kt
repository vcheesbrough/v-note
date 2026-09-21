package link.desync.vnote.telemetry

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.random.Random

class OtlpTest {
    private val context = SpanContext(TraceId("0af7651916cd43dd8448eb211c80319c"), SpanId("b7ad6b7169203331"))

    @Test
    fun traceparentIsVersionZeroAndAlwaysSampled() {
        assertEquals("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01", context.traceparent())
    }

    @Test
    fun parseTraceparentRoundTripsAndRejectsInvalid() {
        assertEquals(context, parseTraceparent(context.traceparent()))
        assertNull(parseTraceparent(null))
        assertNull(parseTraceparent("garbage"))
        assertNull(parseTraceparent("00-${"0".repeat(32)}-b7ad6b7169203331-01"))
        assertNull(parseTraceparent("00-0af7651916cd43dd8448eb211c80319c-${"0".repeat(16)}-01"))
        assertNull(parseTraceparent("00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01"))
    }

    @Test
    fun randomIdsAreLowercaseHexOfTheRightLength() {
        val trace = TraceId.random()
        val span = SpanId.random()
        assertTrue(trace.hex.matches(Regex("[0-9a-f]{32}")))
        assertTrue(span.hex.matches(Regex("[0-9a-f]{16}")))
    }

    @Test
    fun anAllZeroRandomIdIsRepaired() {
        val zeros =
            object : Random() {
                override fun nextBits(bitCount: Int): Int = 0
            }
        assertNotEquals("0".repeat(32), TraceId.random(zeros).hex)
        assertNotEquals("0".repeat(16), SpanId.random(zeros).hex)
    }

    @Test
    fun spanEncodingFollowsOtlpJsonRules() {
        val span =
            FinishedSpan(
                context = context,
                parentSpanId = SpanId("00f067aa0ba902b7"),
                name = "http.client",
                kind = SpanKind.Client,
                startUnixNanos = 1_700_000_000_000_000_001,
                endUnixNanos = 1_700_000_000_500_000_000,
                attributes =
                    listOf(
                        Attribute("url.template", "/api/pages"),
                        Attribute("http.response.status_code", 200),
                        Attribute("vnote.seq", 7L),
                        Attribute("flag", true),
                        Attribute("ratio", 0.5),
                    ),
                error = "HTTP 500",
            )
        val encoded =
            JSONObject(tracesRequest(listOf(span)))
                .getJSONArray("resourceSpans")
                .getJSONObject(0)
                .getJSONArray("scopeSpans")
                .getJSONObject(0)
                .getJSONArray("spans")
                .getJSONObject(0)

        assertEquals("0af7651916cd43dd8448eb211c80319c", encoded.getString("traceId"))
        assertEquals("b7ad6b7169203331", encoded.getString("spanId"))
        assertEquals("00f067aa0ba902b7", encoded.getString("parentSpanId"))
        assertEquals(3, encoded.getInt("kind"))
        // 64-bit integers are strings on the wire.
        assertEquals("1700000000000000001", encoded.get("startTimeUnixNano"))
        assertEquals("1700000000500000000", encoded.get("endTimeUnixNano"))
        assertEquals(2, encoded.getJSONObject("status").getInt("code"))
        assertEquals("HTTP 500", encoded.getJSONObject("status").getString("message"))

        val values =
            (0 until encoded.getJSONArray("attributes").length()).associate { index ->
                val attribute = encoded.getJSONArray("attributes").getJSONObject(index)
                attribute.getString("key") to attribute.getJSONObject("value")
            }
        assertEquals("/api/pages", values.getValue("url.template").getString("stringValue"))
        assertEquals("200", values.getValue("http.response.status_code").get("intValue"))
        assertEquals("7", values.getValue("vnote.seq").get("intValue"))
        assertTrue(values.getValue("flag").getBoolean("boolValue"))
        assertEquals(0.5, values.getValue("ratio").getDouble("doubleValue"), 0.0)
    }

    @Test
    fun aRootSpanWithoutErrorOmitsParentAndStatus() {
        val root = FinishedSpan(context, null, "screen.library", SpanKind.Internal, 1, 1)
        val encoded =
            JSONObject(tracesRequest(listOf(root)))
                .getJSONArray("resourceSpans")
                .getJSONObject(0)
                .getJSONArray("scopeSpans")
                .getJSONObject(0)
                .getJSONArray("spans")
                .getJSONObject(0)
        assertFalse(encoded.has("parentSpanId"))
        assertFalse(encoded.has("status"))
        assertFalse(encoded.has("attributes"))
        assertEquals(1, encoded.getInt("kind"))
    }

    @Test
    fun logRecordsCarrySeverityAndTraceCorrelation() {
        val record = LogRecord(42, Severity.Warn, "page channel error", context = context)
        val encoded =
            JSONObject(logsRequest(listOf(record)))
                .getJSONArray("resourceLogs")
                .getJSONObject(0)
                .getJSONArray("scopeLogs")
                .getJSONObject(0)
                .getJSONArray("logRecords")
                .getJSONObject(0)
        assertEquals("42", encoded.get("timeUnixNano"))
        assertEquals(13, encoded.getInt("severityNumber"))
        assertEquals("WARN", encoded.getString("severityText"))
        assertEquals("page channel error", encoded.getJSONObject("body").getString("stringValue"))
        assertEquals(context.traceId.hex, encoded.getString("traceId"))
        assertEquals(context.spanId.hex, encoded.getString("spanId"))
    }

    @Test
    fun severityNumbersAreTheFirstOfEachOtlpRange() {
        assertEquals(listOf(5, 9, 13, 17), Severity.entries.map { it.number })
    }

    // The sidecar keeps exactly these three and overwrites identity; the app
    // sends nothing that would only be thrown away (or, worse, not).
    @Test
    fun resourceCarriesOnlyTheSidecarsAllowList() {
        for (body in listOf(tracesRequest(emptyList()), logsRequest(emptyList()))) {
            val root = JSONObject(body)
            val resource =
                (root.optJSONArray("resourceSpans") ?: root.getJSONArray("resourceLogs"))
                    .getJSONObject(0)
                    .getJSONObject("resource")
                    .getJSONArray("attributes")
            val keys = (0 until resource.length()).map { resource.getJSONObject(it).getString("key") }.toSet()
            assertEquals(setOf("telemetry.sdk.name", "telemetry.sdk.language", "telemetry.sdk.version"), keys)
        }
    }

    // What MAX_BATCH relies on: a full batch of realistic spans is well under the
    // ingress's 1 MiB cap before gzip, and gzip then takes it far lower.
    @Test
    fun aFullBatchStaysWellUnderTheIngressCap() {
        val spans =
            List(MAX_BATCH) {
                FinishedSpan(
                    SpanContext(TraceId.random(), SpanId.random()),
                    SpanId.random(),
                    "ink.commit",
                    SpanKind.Internal,
                    1_700_000_000_000_000_000,
                    1_700_000_000_120_000_000,
                    listOf(
                        Attribute("vnote.client_batch_id", "batch_" + "a".repeat(32)),
                        Attribute("vnote.seq", 123_456L),
                    ),
                )
            }
        val body = tracesRequest(spans)
        assertTrue("uncompressed ${body.length} bytes", body.length < INGRESS_CAP_BYTES / 2)
        assertTrue("gzipped ${gzip(body).size} bytes", gzip(body).size < body.length / 4)
    }

    private companion object {
        const val INGRESS_CAP_BYTES = 1024 * 1024
    }
}
