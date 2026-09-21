package link.desync.vnote.telemetry

import org.json.JSONArray
import org.json.JSONObject
import kotlin.random.Random

// The OTLP/HTTP **JSON** encoding of the two signals the app exports (#406):
// spans and log records. Pure data and org.json — no Android API — so it is
// unit-tested on the JVM.
//
// Hand-written rather than an OpenTelemetry SDK, as the SPA's is (#354), and for
// a measured reason: see the Q12 answer on card #406. The collector's receiver
// accepts JSON, and OkHttp and org.json are already in the APK.
//
// Three rules of the encoding are easy to get wrong, and each produces a request
// the collector rejects or, worse, silently misreads:
//
// - trace and span ids are **lowercase hex**, not base64;
// - 64-bit integers — every timestamp, and `intValue` — are **strings**;
// - enums (`kind`, `status.code`, `severityNumber`) are **numbers**.

// Version of this hand-rolled exporter, reported as `telemetry.sdk.version`. Bump
// when the encoding changes, not when the app does — `service.version` is the
// app's, and the sidecar supplies it.
internal const val SDK_VERSION = "1"

private const val SDK_NAME = "v-note-android-otlp"
private const val SCOPE_NAME = "v-note-android"
private const val TRACE_ID_BYTES = 16
private const val SPAN_ID_BYTES = 8

// A W3C trace id, as the 32 lowercase hex digits it is on the wire. All-zero is
// reserved as "invalid", so [random] repairs it.
@JvmInline
value class TraceId(
    val hex: String,
) {
    companion object {
        fun random(random: Random = Random.Default): TraceId = TraceId(randomHex(random, TRACE_ID_BYTES))
    }
}

// A W3C span id: 16 lowercase hex digits. All-zero is likewise invalid.
@JvmInline
value class SpanId(
    val hex: String,
) {
    companion object {
        fun random(random: Random = Random.Default): SpanId = SpanId(randomHex(random, SPAN_ID_BYTES))
    }
}

private fun randomHex(
    random: Random,
    size: Int,
): String {
    val bytes = random.nextBytes(size)
    if (bytes.all { it == 0.toByte() }) {
        bytes[size - 1] = 1
    }
    return bytes.joinToString("") { "%02x".format(it) }
}

// Where a span or a log line sits: the trace, and the span inside it that is its
// parent. Captured where work *starts* and passed down — never re-read from
// shared state later, or work that outlives a screen change moves traces
// halfway through (the bug #354's self-review found in the SPA).
data class SpanContext(
    val traceId: TraceId,
    val spanId: SpanId,
) {
    // The `traceparent` header for a request made inside this span.
    //
    // Always flagged sampled (`-01`): the app does not head-sample. Traefik's
    // tracer is parent-based, so a `-00` would not merely drop the app's span —
    // it would switch off the Traefik and server spans for that request too.
    fun traceparent(): String = "00-${traceId.hex}-${spanId.hex}-01"
}

enum class SpanKind(
    internal val wire: Int,
) {
    Internal(1),

    // An outbound request: the span a server's span is the child of.
    Client(3),
}

enum class Severity(
    internal val number: Int,
) {
    // OTLP's `severityNumber`: the first number of each level's range, as the
    // SDKs emit.
    Debug(5),
    Info(9),
    Warn(13),
    Error(17),
}

// An attribute value: String, Long, Int, Boolean or Double. Anything else is
// encoded as its string form rather than rejected — telemetry must not throw.
data class Attribute(
    val key: String,
    val value: Any,
)

data class FinishedSpan(
    val context: SpanContext,
    val parentSpanId: SpanId?,
    val name: String,
    val kind: SpanKind,
    val startUnixNanos: Long,
    val endUnixNanos: Long,
    val attributes: List<Attribute> = emptyList(),
    // Non-null marks the span failed. An unset status is the absence of the
    // field, which is also what "fine" looks like to every OTLP backend.
    val error: String? = null,
)

data class LogRecord(
    val unixNanos: Long,
    val severity: Severity,
    val body: String,
    val attributes: List<Attribute> = emptyList(),
    // The span that was active, which is what makes Loki's line link to Tempo.
    val context: SpanContext? = null,
)

// An `ExportTraceServiceRequest` body.
fun tracesRequest(spans: List<FinishedSpan>): String =
    JSONObject()
        .put(
            "resourceSpans",
            JSONArray().put(
                JSONObject()
                    .put("resource", resource())
                    .put(
                        "scopeSpans",
                        JSONArray().put(
                            JSONObject()
                                .put("scope", scope())
                                .put("spans", JSONArray(spans.map(::encodeSpan))),
                        ),
                    ),
            ),
        ).toString()

// An `ExportLogsServiceRequest` body.
fun logsRequest(records: List<LogRecord>): String =
    JSONObject()
        .put(
            "resourceLogs",
            JSONArray().put(
                JSONObject()
                    .put("resource", resource())
                    .put(
                        "scopeLogs",
                        JSONArray().put(
                            JSONObject()
                                .put("scope", scope())
                                .put("logRecords", JSONArray(records.map(::encodeLog))),
                        ),
                    ),
            ),
        ).toString()

// What the app says about itself. **Every identity attribute is absent on
// purpose**: the sidecar drops whatever a client claims for `service.name`,
// `deployment.environment` and `service.version` and writes its own. What is here
// is exactly the sidecar's allow-list.
private fun resource(): JSONObject =
    JSONObject().put(
        "attributes",
        encodeAttributes(
            listOf(
                Attribute("telemetry.sdk.name", SDK_NAME),
                Attribute("telemetry.sdk.language", "kotlin"),
                Attribute("telemetry.sdk.version", SDK_VERSION),
            ),
        ),
    )

private fun scope(): JSONObject = JSONObject().put("name", SCOPE_NAME).put("version", SDK_VERSION)

private fun encodeSpan(span: FinishedSpan): JSONObject {
    val json =
        JSONObject()
            .put("traceId", span.context.traceId.hex)
            .put("spanId", span.context.spanId.hex)
            .put("name", span.name)
            .put("kind", span.kind.wire)
            .put("startTimeUnixNano", span.startUnixNanos.toString())
            .put("endTimeUnixNano", span.endUnixNanos.toString())
    span.parentSpanId?.let { json.put("parentSpanId", it.hex) }
    if (span.attributes.isNotEmpty()) {
        json.put("attributes", encodeAttributes(span.attributes))
    }
    span.error?.let { message ->
        json.put("status", JSONObject().put("code", STATUS_CODE_ERROR).put("message", message))
    }
    return json
}

private fun encodeLog(record: LogRecord): JSONObject {
    val json =
        JSONObject()
            .put("timeUnixNano", record.unixNanos.toString())
            .put("severityNumber", record.severity.number)
            .put("severityText", record.severity.name.uppercase())
            .put("body", JSONObject().put("stringValue", record.body))
    if (record.attributes.isNotEmpty()) {
        json.put("attributes", encodeAttributes(record.attributes))
    }
    record.context?.let { context ->
        json.put("traceId", context.traceId.hex)
        json.put("spanId", context.spanId.hex)
    }
    return json
}

private fun encodeAttributes(attributes: List<Attribute>): JSONArray =
    JSONArray(
        attributes.map { attribute ->
            JSONObject().put("key", attribute.key).put("value", encodeValue(attribute.value))
        },
    )

private fun encodeValue(value: Any): JSONObject =
    when (value) {
        is Boolean -> JSONObject().put("boolValue", value)
        is Int -> JSONObject().put("intValue", value.toString())
        is Long -> JSONObject().put("intValue", value.toString())
        is Double -> JSONObject().put("doubleValue", value)
        is Float -> JSONObject().put("doubleValue", value.toDouble())
        else -> JSONObject().put("stringValue", value.toString())
    }

private const val STATUS_CODE_ERROR = 2
