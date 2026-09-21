package link.desync.vnote.telemetry

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class TelemetryRuntimeTest {
    private class FakeTransport(
        var ready: Boolean = true,
        var outcome: ExportOutcome = ExportOutcome.Accepted,
    ) : Transport {
        val sent = mutableListOf<Pair<Signal, JSONObject>>()

        override fun isReady(): Boolean = ready

        override fun send(
            signal: Signal,
            body: String,
        ): ExportOutcome {
            sent += signal to JSONObject(body)
            return outcome
        }
    }

    private var clock = 1_000L
    private val transport = FakeTransport()
    private val runtime = TelemetryRuntime(transport) { clock }

    private fun JSONObject.spans() =
        getJSONArray("resourceSpans")
            .getJSONObject(0)
            .getJSONArray("scopeSpans")
            .getJSONObject(0)
            .getJSONArray("spans")
            .let { array -> (0 until array.length()).map(array::getJSONObject) }

    private fun JSONObject.logs() =
        getJSONArray("resourceLogs")
            .getJSONObject(0)
            .getJSONArray("scopeLogs")
            .getJSONObject(0)
            .getJSONArray("logRecords")
            .let { array -> (0 until array.length()).map(array::getJSONObject) }

    @Test
    fun withoutATransportNothingIsCollectedButContextsAreStillValid() {
        val off = TelemetryRuntime(transport = null)
        val span = off.span("http.client", off.screen())
        span.end()
        off.log(Severity.Error, "ignored")
        off.exportNow()
        assertFalse(off.isExporting)
        assertTrue(span.context.traceparent().matches(Regex("00-[0-9a-f]{32}-[0-9a-f]{16}-01")))
    }

    @Test
    fun exportsTracesThenLogs() {
        val screen = runtime.startScreen("screen.library")
        runtime.log(Severity.Info, "hello", screen)
        runtime.exportNow()

        assertEquals(listOf(Signal.Traces, Signal.Logs), transport.sent.map { it.first })
        val root =
            transport.sent[0]
                .second
                .spans()
                .single()
        assertEquals("screen.library", root.getString("name"))
        assertFalse(root.has("parentSpanId"))
        val log =
            transport.sent[1]
                .second
                .logs()
                .single()
        assertEquals(screen.traceId.hex, log.getString("traceId"))
        assertEquals(screen.spanId.hex, log.getString("spanId"))
    }

    @Test
    fun aChildSpanKeepsItsParentsTraceAndRecordsItsTimes() {
        val parent = runtime.startScreen("screen.page")
        clock = 2_000
        val child = runtime.span("realtime.connect", parent)
        clock = 5_000
        child.attr("vnote.channel", "page").end()
        runtime.exportNow()

        val encoded =
            transport.sent
                .single()
                .second
                .spans()
                .single { it.getString("name") == "realtime.connect" }
        assertEquals(parent.traceId.hex, encoded.getString("traceId"))
        assertEquals(parent.spanId.hex, encoded.getString("parentSpanId"))
        assertEquals("2000", encoded.get("startTimeUnixNano"))
        assertEquals("5000", encoded.get("endTimeUnixNano"))
    }

    @Test
    fun aSpanIsRecordedOnceWhicheverWayItEnds() {
        val span = runtime.span("ink.commit", runtime.screen())
        span.end()
        span.fail("late")
        runtime.exportNow()
        val encoded =
            transport.sent
                .single()
                .second
                .spans()
                .single()
        assertFalse(encoded.has("status"))
    }

    @Test
    fun eachScreenIsANewTrace() {
        val first = runtime.startScreen("screen.library")
        val second = runtime.startScreen("screen.page")
        assertNotEquals(first.traceId, second.traceId)
        assertEquals(second, runtime.screen())
    }

    @Test
    fun nothingIsSentOrLostWhileSignedOut() {
        transport.ready = false
        runtime.log(Severity.Warn, "queued")
        runtime.exportNow()
        assertTrue(transport.sent.isEmpty())

        transport.ready = true
        runtime.exportNow()
        assertEquals(
            "queued",
            transport.sent
                .single()
                .second
                .logs()
                .single()
                .getJSONObject("body")
                .getString("stringValue"),
        )
    }

    @Test
    fun a404SwitchesExportOffAndDiscardsTheQueue() {
        transport.outcome = ExportOutcome.SwitchedOff
        runtime.startScreen("screen.library")
        runtime.log(Severity.Info, "never sent")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)
        assertFalse(runtime.isExporting)

        transport.outcome = ExportOutcome.Accepted
        runtime.log(Severity.Info, "after")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun anUnavailableCollectorIsMetWithFewerRequests() {
        transport.outcome = ExportOutcome.Unavailable
        runtime.log(Severity.Info, "one")
        runtime.exportNow()
        runtime.log(Severity.Info, "two")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun droppedItemsAreReported() {
        transport.ready = false
        repeat(OUTBOX_CAPACITY + 3) { runtime.log(Severity.Debug, "flood") }
        transport.ready = true
        // The report joins the back of the queue, behind a full outbox.
        repeat(3) { runtime.exportNow() }
        val bodies =
            transport.sent
                .filter { it.first == Signal.Logs }
                .flatMap { it.second.logs() }
                .map { it.getJSONObject("body").getString("stringValue") }
        assertTrue(bodies.contains("telemetry outbox full; items dropped"))
    }

    @Test
    fun crashHandlerLogsTheCrashWithItsStackAndDefersToThePreviousHandler() {
        var delegated: Throwable? = null
        val handler = CrashHandler({ runtime }, { _, error -> delegated = error })
        val crash = IllegalStateException("boom")

        handler.uncaughtException(Thread.currentThread(), crash)

        assertSame(crash, delegated)
        val log =
            transport.sent
                .single { it.first == Signal.Logs }
                .second
                .logs()
                .single()
        assertEquals(17, log.getInt("severityNumber"))
        val attributes =
            log.getJSONArray("attributes").let { array ->
                (0 until array.length()).associate {
                    array.getJSONObject(it).getString("key") to array.getJSONObject(it).getJSONObject("value")
                }
            }
        assertEquals("java.lang.IllegalStateException", attributes.getValue("exception.type").getString("stringValue"))
        assertEquals("boom", attributes.getValue("exception.message").getString("stringValue"))
        assertTrue(attributes.getValue("exception.stacktrace").getString("stringValue").contains("boom"))
        assertEquals(runtime.screen().traceId.hex, log.getString("traceId"))
    }

    // The crash must go out even when the ordinary export would not send it:
    // mid-backoff, behind a full log outbox, with traces queued ahead of it.
    @Test
    fun theCrashIsSentAloneAndAtOnceEvenInBackoffBehindAFullOutbox() {
        transport.outcome = ExportOutcome.Unavailable
        runtime.startScreen("screen.page")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)
        repeat(OUTBOX_CAPACITY) { runtime.log(Severity.Debug, "older") }
        transport.sent.clear()

        CrashHandler({ runtime }, null).uncaughtException(Thread.currentThread(), IllegalStateException("boom"))

        val (signal, body) = transport.sent.single()
        assertEquals(Signal.Logs, signal)
        val record = body.logs().single()
        assertEquals("uncaught exception", record.getJSONObject("body").getString("stringValue"))
    }

    @Test
    fun theCrashIsNotSentOnceTheServerHasSwitchedTelemetryOff() {
        transport.outcome = ExportOutcome.SwitchedOff
        runtime.log(Severity.Info, "first")
        runtime.exportNow()
        transport.sent.clear()

        CrashHandler({ runtime }, null).uncaughtException(Thread.currentThread(), IllegalStateException("boom"))

        assertTrue(transport.sent.isEmpty())
    }

    @Test
    fun crashHandlerStillDefersWhenTelemetryIsOff() {
        var delegated = false
        val handler = CrashHandler({ TelemetryRuntime(transport = null) }, { _, _ -> delegated = true })
        handler.uncaughtException(Thread.currentThread(), RuntimeException())
        assertTrue(delegated)
    }

    @Test
    fun throwableAttributesOmitTheStackUnlessAsked() {
        val attributes = throwableAttributes(RuntimeException("x"))
        assertEquals(listOf("exception.type", "exception.message"), attributes.map { it.key })
        assertNull(throwableAttributes(null).firstOrNull())
    }
}
