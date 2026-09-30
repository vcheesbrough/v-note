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
        var config: ConfigFetch = ConfigFetch.Configured("https://ingest.example"),
        var token: String = "token-1",
    ) : Transport {
        val sent = mutableListOf<Pair<Signal, JSONObject>>()
        val endpoints = mutableListOf<String>()
        var configFetches = 0

        override fun isReady(): Boolean = ready

        override fun credentialId(): Int? = if (ready) token.hashCode() else null

        override fun fetchConfig(): ConfigFetch {
            configFetches += 1
            return config
        }

        override fun send(
            endpoint: String,
            signal: Signal,
            body: String,
        ): ExportOutcome {
            endpoints += endpoint
            sent += signal to JSONObject(body)
            return outcome
        }
    }

    private var clock = 1_000L
    private val transport = FakeTransport()
    private val local = mutableListOf<String>()
    private val runtime =
        TelemetryRuntime(transport, serviceVersion = "0.58.0", localLog = { local += it }, jitter = { 0.5 }) { clock }

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
        assertEquals("no session, no config fetch", 0, transport.configFetches)

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

    // #439: the configured endpoint is where exports go, and the resource names
    // the service and the build.
    @Test
    fun exportsGoToTheConfiguredEndpointAsThisService() {
        runtime.log(Severity.Info, "hello")
        runtime.exportNow()
        assertEquals(1, transport.configFetches)
        assertEquals(listOf("https://ingest.example"), transport.endpoints)
        assertTrue(runtime.isExporting)
        assertEquals(listOf("client telemetry: exporting to https://ingest.example"), local)
        val resource =
            transport.sent
                .single()
                .second
                .getJSONArray("resourceLogs")
                .getJSONObject(0)
                .getJSONObject("resource")
                .toString()
        assertTrue(resource, resource.contains("v-note-android") && resource.contains("0.58.0"))
    }

    // No configuration, no telemetry: nothing is sent, what waited is dropped,
    // and it is said once, locally.
    @Test
    fun withoutConfigurationOtlpIsNeverInitialised() {
        transport.config = ConfigFetch.Absent
        runtime.log(Severity.Info, "launch")
        runtime.exportNow()
        runtime.log(Severity.Info, "later")
        runtime.exportNow()

        assertTrue(transport.sent.isEmpty())
        assertEquals("absent is final: asked once", 1, transport.configFetches)
        assertFalse(runtime.isExporting)
        assertEquals(1, local.size)
        assertTrue(local.single(), local.single().contains(OffReason.NotConfigured.description))
    }

    // A launch on a flaky radio: the first config fetch is not answered, the
    // next one is — and telemetry comes on, with what waited in the buffer.
    @Test
    fun anUnansweredConfigFetchIsAskedAgainOnTheNextTick() {
        transport.config = ConfigFetch.NotYet
        runtime.log(Severity.Info, "launch")
        runtime.exportNow()
        assertTrue(transport.sent.isEmpty())
        assertFalse(runtime.isExporting)

        transport.config = ConfigFetch.Configured("https://ingest.example")
        runtime.exportNow()
        assertEquals(2, transport.configFetches)
        assertTrue(runtime.isExporting)
        assertEquals(
            "launch",
            transport.sent
                .single()
                .second
                .logs()
                .single()
                .getJSONObject("body")
                .getString("stringValue"),
        )
    }

    // …but only within the pre-config window: a server that never answers
    // leaves telemetry off, not retried for the life of the process.
    @Test
    fun anUnansweredConfigFetchStopsAtTheDeadline() {
        transport.config = ConfigFetch.NotYet
        runtime.exportNow()
        clock += (PRE_CONFIG_MAX_MS + 1) * 1_000_000
        runtime.exportNow()
        runtime.exportNow()
        assertEquals("no fetch after the deadline", 1, transport.configFetches)
        assertTrue(local.single().contains(OffReason.ConfigTimedOut.description))
    }

    // The pre-config buffer is short-lived: a session that never arrives means
    // the launch spans are discarded, not held for the process.
    @Test
    fun waitingForConfigurationTimesOut() {
        transport.ready = false
        runtime.log(Severity.Info, "launch")
        clock += (PRE_CONFIG_MAX_MS + 1) * 1_000_000
        runtime.exportNow()
        transport.ready = true
        runtime.exportNow()

        assertTrue(transport.sent.isEmpty())
        assertEquals(0, transport.configFetches)
        assertTrue(local.single().contains(OffReason.ConfigTimedOut.description))
    }

    // 401 → wait for the app to refresh the token → a second 401 stops.
    @Test
    fun unauthorizedWaitsForARefreshedTokenThenStopsOnASecond401() {
        transport.outcome = ExportOutcome.Unauthorized
        runtime.log(Severity.Info, "one")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)

        // Same token: nothing is sent.
        runtime.log(Severity.Info, "two")
        runtime.exportNow()
        assertEquals(1, transport.sent.size)

        // The app's OIDC stack refreshed: one more attempt, refused again.
        transport.token = "token-2"
        runtime.exportNow()
        assertEquals(2, transport.sent.size)
        assertFalse(runtime.isExporting)
        assertTrue(local.last(), local.last().contains(OffReason.Unauthorized.description))

        transport.outcome = ExportOutcome.Accepted
        transport.token = "token-3"
        runtime.log(Severity.Info, "three")
        runtime.exportNow()
        assertEquals("stopped for the process", 2, transport.sent.size)
    }

    // A retryable failure keeps the batch, backs off, and sends the same batch
    // again once the wait is over — with one line when it starts failing and
    // one when it recovers, none per batch.
    @Test
    fun aRetryableFailureRetriesTheSameBatchAfterBackoff() {
        transport.outcome = ExportOutcome.Retryable(503)
        runtime.log(Severity.Info, "kept")
        runtime.exportNow()
        runtime.exportNow()
        assertEquals("backing off", 1, transport.sent.size)

        transport.outcome = ExportOutcome.Accepted
        clock += BACKOFF_MAX_MS * 1_000_000
        runtime.exportNow()
        assertEquals(2, transport.sent.size)
        assertEquals(transport.sent[0].second.toString(), transport.sent[1].second.toString())
        assertEquals(3, local.size)
        assertTrue(local[1], local[1].startsWith("client telemetry export failing: status 503"))
        assertTrue(local[2], local[2].startsWith("client telemetry export recovered"))
    }

    // A permanent refusal drops the batch and does not slow the next one.
    @Test
    fun aRejectedBatchIsDroppedAndTheNextOneGoesOut() {
        transport.outcome = ExportOutcome.Rejected(400)
        runtime.log(Severity.Info, "bad")
        runtime.exportNow()
        transport.outcome = ExportOutcome.Accepted
        runtime.log(Severity.Info, "good")
        runtime.exportNow()
        val bodies =
            transport.sent.flatMap { it.second.logs() }.map { it.getJSONObject("body").getString("stringValue") }
        assertEquals(listOf("bad", "good"), bodies)
    }

    // Overflow is said locally — never pushed into the queue that is failing.
    @Test
    fun droppedItemsAreReportedLocallyNotExported() {
        transport.ready = false
        repeat(PRE_CONFIG_CAPACITY + 3) { runtime.log(Severity.Debug, "flood") }
        transport.ready = true
        repeat(3) { runtime.exportNow() }
        val bodies =
            transport.sent
                .filter { it.first == Signal.Logs }
                .flatMap { it.second.logs() }
                .map { it.getJSONObject("body").getString("stringValue") }
        assertTrue(bodies.all { it == "flood" })
        assertTrue(local.any { it.startsWith("client telemetry queue full; 3 item(s) dropped") })
    }

    @Test
    fun crashHandlerLogsTheCrashWithItsStackAndDefersToThePreviousHandler() {
        var delegated: Throwable? = null
        val handler = CrashHandler({ runtime }, { _, error -> delegated = error })
        val crash = IllegalStateException("boom")
        // Configured first: with no configuration there is nowhere to send it.
        runtime.exportNow()
        transport.sent.clear()

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
        // Located where it was thrown, on the thread that crashed (#453).
        assertEquals(
            "link.desync.vnote.telemetry.TelemetryRuntimeTest.crashHandlerLogsTheCrashWithItsStackAndDefersToThePreviousHandler",
            attributes.getValue(CODE_FUNCTION_NAME).getString("stringValue"),
        )
        assertEquals(Thread.currentThread().name, attributes.getValue(THREAD_NAME).getString("stringValue"))
        assertTrue(attributes.containsKey(THREAD_ID))
    }

    // The crash must go out even when the ordinary export would not send it:
    // mid-backoff, behind a full log outbox, with traces queued ahead of it.
    @Test
    fun theCrashIsSentAloneAndAtOnceEvenInBackoffBehindAFullOutbox() {
        transport.outcome = ExportOutcome.Retryable(503)
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
    fun theCrashIsNotSentWhenTheServerGaveNoConfiguration() {
        transport.config = ConfigFetch.Absent
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
