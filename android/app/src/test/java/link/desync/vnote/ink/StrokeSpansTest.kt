package link.desync.vnote.ink

import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.telemetry.ExportOutcome
import link.desync.vnote.telemetry.Signal
import link.desync.vnote.telemetry.TelemetryRuntime
import link.desync.vnote.telemetry.Transport
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

class StrokeSpansTest {
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
        ) { clock }
    private var clock = 0L

    // A stroke whose last point is 250 ms after pen down.
    private val stroke = Stroke(points = listOf(StrokePoint(0.0, 0.0, 0), StrokePoint(1.0, 1.0, 250)))

    private fun exportedSpans(): Map<String, JSONObject> {
        runtime.exportNow()
        val spans =
            JSONObject(bodies.single())
                .getJSONArray("resourceSpans")
                .getJSONObject(0)
                .getJSONArray("scopeSpans")
                .getJSONObject(0)
                .getJSONArray("spans")
        return (0 until spans.length()).map(spans::getJSONObject).associateBy { it.getString("name") }
    }

    @Test
    fun aConfirmedStrokeSpansPenDownToEchoWithCaptureAndCommitBeneathIt() {
        val screen = runtime.startScreen("screen.page")
        clock = 1_000_000_000
        val spans = StrokeSpans.open(screen, "batch_1", stroke, runtime)
        clock = 1_080_000_000
        spans.confirmed(seq = 42)

        val exported = exportedSpans()
        val strokeSpan = exported.getValue("ink.stroke")
        val capture = exported.getValue("ink.capture")
        val commit = exported.getValue("ink.commit")

        assertEquals(screen.spanId.hex, strokeSpan.getString("parentSpanId"))
        assertEquals(strokeSpan.getString("spanId"), capture.getString("parentSpanId"))
        assertEquals(strokeSpan.getString("spanId"), commit.getString("parentSpanId"))
        // Backdated to pen down: 250 ms before the commit.
        assertEquals("750000000", strokeSpan.get("startTimeUnixNano"))
        assertEquals("750000000", capture.get("startTimeUnixNano"))
        assertEquals("1000000000", capture.get("endTimeUnixNano"))
        assertEquals("1000000000", commit.get("startTimeUnixNano"))
        assertEquals("1080000000", commit.get("endTimeUnixNano"))
        assertEquals("1080000000", strokeSpan.get("endTimeUnixNano"))
    }

    @Test
    fun aDiscardedStrokeFailsBothOpenSpans() {
        val spans = StrokeSpans.open(runtime.startScreen("screen.page"), "batch_1", stroke, runtime)
        spans.failed("discarded")

        val exported = exportedSpans()
        for (name in listOf("ink.stroke", "ink.commit")) {
            val status = exported.getValue(name).getJSONObject("status")
            assertEquals(2, status.getInt("code"))
            assertEquals("discarded", status.getString("message"))
        }
    }
}
