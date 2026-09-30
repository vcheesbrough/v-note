package link.desync.vnote.telemetry

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Collections

// #453: every span and log record names the app code that produced it and the
// thread it ran on, not a frame inside this package.
class CallSiteTest {
    // Synchronized: the facade test installs the runtime, whose export thread
    // may send a batch while the test sends its own.
    private val bodies = Collections.synchronizedList(mutableListOf<Pair<Signal, JSONObject>>())
    private val runtime =
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
                    bodies += signal to JSONObject(body)
                    return ExportOutcome.Accepted
                }
            },
        )

    private fun frame(
        className: String,
        method: String,
        file: String?,
        line: Int,
    ) = StackTraceElement(className, method, file, line)

    // The line after the one that calls this.
    private fun nextLine(): Int = Throwable().stackTrace[1].lineNumber + 1

    private fun exported(signal: Signal): List<JSONObject> {
        runtime.exportNow()
        val (resources, scopes, items) =
            when (signal) {
                Signal.Traces -> Triple("resourceSpans", "scopeSpans", "spans")
                Signal.Logs -> Triple("resourceLogs", "scopeLogs", "logRecords")
            }
        val sent = synchronized(bodies) { bodies.filter { it.first == signal }.map { it.second } }
        return sent.flatMap { body ->
            val array =
                body
                    .getJSONArray(resources)
                    .getJSONObject(0)
                    .getJSONArray(scopes)
                    .getJSONObject(0)
                    .getJSONArray(items)
            (0 until array.length()).map(array::getJSONObject)
        }
    }

    private fun JSONObject.attributes(): Map<String, JSONObject> =
        getJSONArray("attributes").let { array ->
            (0 until array.length()).associate {
                array.getJSONObject(it).getString("key") to array.getJSONObject(it).getJSONObject("value")
            }
        }

    private fun assertCalledFromHere(
        attributes: Map<String, JSONObject>,
        method: String,
        line: Int,
    ) {
        assertEquals(
            "link.desync.vnote.telemetry.CallSiteTest.$method",
            attributes.getValue(CODE_FUNCTION_NAME).getString("stringValue"),
        )
        assertEquals(
            "link/desync/vnote/telemetry/CallSiteTest.kt",
            attributes.getValue(CODE_FILE_PATH).getString("stringValue"),
        )
        // 64-bit integers are strings on the wire.
        assertEquals(line.toString(), attributes.getValue(CODE_LINE_NUMBER).getString("intValue"))
        assertEquals(Thread.currentThread().name, attributes.getValue(THREAD_NAME).getString("stringValue"))
        @Suppress("DEPRECATION")
        assertEquals(Thread.currentThread().id.toString(), attributes.getValue(THREAD_ID).getString("intValue"))
    }

    // The stack under `OkHttpApiClient.listPages()` as the JVM reports it: the
    // runtime, the interceptor, OkHttp's chain, the shared request helper (and
    // Kotlin's synthetic accessor for it, since a lambda calls it — both under
    // the mangled name `javap` shows for a `Result`-returning function), then
    // the `withContext` body of listPages and the coroutine machinery below it.
    private fun listPagesStack(vararg top: StackTraceElement) =
        arrayOf(
            *top,
            frame("okhttp3.internal.http.RealInterceptorChain", "proceed", "RealInterceptorChain.kt", 126),
            frame("okhttp3.internal.connection.RealCall", "getResponseWithInterceptorChain", "RealCall.kt", 203),
            frame("okhttp3.internal.connection.RealCall", "execute", "RealCall.kt", 158),
            frame("link.desync.vnote.api.OkHttpApiClient", "makeAuthorizedApiRequest-0E7RQCE", "OkHttpApiClient.kt", 291),
            frame("link.desync.vnote.api.OkHttpApiClient", "access\$makeAuthorizedApiRequest-0E7RQCE", "OkHttpApiClient.kt", 36),
            frame("link.desync.vnote.api.OkHttpApiClient\$listPages\$2", "invokeSuspend", "OkHttpApiClient.kt", 83),
            frame("kotlin.coroutines.jvm.internal.BaseContinuationImpl", "resumeWith", "ContinuationImpl.kt", 33),
            frame("kotlinx.coroutines.DispatchedTask", "run", "DispatchedTask.kt", 100),
            frame("java.lang.Thread", "run", "Thread.java", 1012),
        )

    @Test
    fun aRequestIsAttributedToTheApiMethodNotTheSharedHelperThatRanIt() {
        val frames =
            listPagesStack(
                frame("link.desync.vnote.telemetry.CallSiteKt", "callSite", "CallSite.kt", 70),
                frame("link.desync.vnote.telemetry.TelemetryRuntime", "getLocated", "Telemetry.kt", 159),
                frame("link.desync.vnote.telemetry.TelemetryRuntime", "span", "Telemetry.kt", 138),
                frame("link.desync.vnote.telemetry.TracingInterceptor", "intercept", "TracingInterceptor.kt", 45),
            )

        val attributes = callSiteAttributes(frames, Thread("DefaultDispatcher-worker-1")).associate { it.key to it.value }

        assertEquals(
            "link.desync.vnote.api.OkHttpApiClient\$listPages\$2.invokeSuspend",
            attributes[CODE_FUNCTION_NAME],
        )
        assertEquals("link/desync/vnote/api/OkHttpApiClient.kt", attributes[CODE_FILE_PATH])
        assertEquals(83, attributes[CODE_LINE_NUMBER])
        assertEquals("DefaultDispatcher-worker-1", attributes[THREAD_NAME])
    }

    // The failure log the helper writes for that request lands on the same
    // method, not on `logHttpFailure`.
    @Test
    fun aFailureLogIsAttributedToTheApiMethodNotTheLoggingHelper() {
        val frames =
            arrayOf(
                frame("link.desync.vnote.telemetry.CallSiteKt", "callSite", "CallSite.kt", 70),
                frame("link.desync.vnote.telemetry.TelemetryRuntime", "log", "Telemetry.kt", 150),
                frame("link.desync.vnote.telemetry.Telemetry", "log", "Telemetry.kt", 445),
                frame("link.desync.vnote.telemetry.AppLog", "export", "AppLog.kt", 70),
                frame("link.desync.vnote.telemetry.AppLog", "w\$default", "AppLog.kt", 38),
                frame("link.desync.vnote.api.OkHttpApiClient", "logHttpFailure", "OkHttpApiClient.kt", 361),
                frame("link.desync.vnote.api.OkHttpApiClient", "makeAuthorizedApiRequest-0E7RQCE", "OkHttpApiClient.kt", 300),
                frame("link.desync.vnote.api.OkHttpApiClient", "access\$makeAuthorizedApiRequest-0E7RQCE", "OkHttpApiClient.kt", 36),
                frame("link.desync.vnote.api.OkHttpApiClient\$deletePage\$2", "invokeSuspend", "OkHttpApiClient.kt", 105),
                frame("kotlin.coroutines.jvm.internal.BaseContinuationImpl", "resumeWith", "ContinuationImpl.kt", 33),
            )

        assertEquals("link.desync.vnote.api.OkHttpApiClient\$deletePage\$2", selectCallSite(frames)?.className)
    }

    // Every helper named in the skip-list exists on its compiled class, under
    // the JVM names the matcher will see, so a rename cannot quietly turn the
    // skip off (and a name the matcher cannot normalise fails here first).
    @Test
    fun everyPlumbingMethodIsARealMethod() {
        for ((className, methods) in PLUMBING_METHODS) {
            val declared =
                Class
                    .forName(className)
                    .declaredMethods
                    .map { sourceMethodName(it.name) }
                    .toSet()
            for (method in methods) {
                assertTrue("$className.$method is not declared", method in declared)
            }
        }
    }

    @Test
    fun jvmMethodNamesAreReducedToTheirSourceNames() {
        assertEquals("makeAuthorizedApiRequest", sourceMethodName("makeAuthorizedApiRequest-0E7RQCE"))
        assertEquals("requestMe", sourceMethodName("access\$requestMe-gIAlu-s"))
        assertEquals("logWebSocketFailure", sourceMethodName("access\$logWebSocketFailure"))
        assertEquals("logHttpFailure", sourceMethodName("logHttpFailure"))
    }

    @Test
    fun anAppMethodOutsideTheSkipListIsTheCallSiteAsIs() {
        val frames =
            arrayOf(
                frame("link.desync.vnote.telemetry.TelemetryRuntime", "span", "Telemetry.kt", 138),
                frame("link.desync.vnote.ink.PageInkSession", "connect", "PageInkSession.kt", 110),
                frame("link.desync.vnote.library.LibraryStateHolder", "openPage", "LibraryStateHolder.kt", 61),
            )

        assertEquals("connect", selectCallSite(frames)?.methodName)
    }

    // A request OkHttp runs on its own dispatcher thread has no app frame: the
    // interceptor that opened the span is the next best thing, never the runtime.
    @Test
    fun withNoAppFrameTheCodeThatCalledTheRuntimeIsTheCallSite() {
        val frames =
            arrayOf(
                frame("link.desync.vnote.telemetry.CallSiteKt", "callSite", "CallSite.kt", 40),
                frame("link.desync.vnote.telemetry.TelemetryRuntime", "located", "Telemetry.kt", 156),
                frame("link.desync.vnote.telemetry.Telemetry", "span\$default", "Telemetry.kt", 430),
                frame("link.desync.vnote.telemetry.TracingInterceptor", "intercept", "TracingInterceptor.kt", 45),
                frame("okhttp3.internal.connection.RealCall\$AsyncCall", "run", "RealCall.kt", 537),
                frame("java.lang.Thread", "run", "Thread.java", 1012),
            )

        assertEquals("intercept", selectCallSite(frames)?.methodName)
    }

    @Test
    fun aFrameWithoutFileOrLineContributesOnlyWhatItHas() {
        val attributes =
            callSiteAttributes(arrayOf(frame("link.desync.vnote.Main", "run", null, -1)), Thread("main"))
                .map { it.key }

        assertEquals(listOf(CODE_FUNCTION_NAME, THREAD_NAME, THREAD_ID), attributes)
        assertNull(selectCallSite(emptyArray()))
    }

    @Test
    fun aSpanNamesWhereItWasOpened() {
        val line = nextLine()
        val span = runtime.span("realtime.connect", runtime.screen())
        span.attr("vnote.channel", "page").end()

        val exported = exported(Signal.Traces).single { it.getString("name") == "realtime.connect" }
        assertCalledFromHere(exported.attributes(), "aSpanNamesWhereItWasOpened", line)
        // The caller's own attributes still arrive, after the location.
        assertEquals("page", exported.attributes().getValue("vnote.channel").getString("stringValue"))
    }

    @Test
    fun aScreenRootNamesWhereTheScreenStarted() {
        val line = nextLine()
        runtime.startScreen("screen.page", listOf(Attribute("vnote.page_id", "p1")))

        val root = exported(Signal.Traces).single { it.getString("name") == "screen.page" }
        assertCalledFromHere(root.attributes(), "aScreenRootNamesWhereTheScreenStarted", line)
        assertTrue(root.attributes().containsKey("vnote.page_id"))
    }

    @Test
    fun aLogRecordNamesWhereItWasWrittenThroughTheFacadeToo() {
        val previous = Telemetry.runtime
        Telemetry.install(runtime)
        try {
            val line = nextLine()
            Telemetry.log(Severity.Warn, "page channel disconnected")

            val record = exported(Signal.Logs).single()
            assertCalledFromHere(record.attributes(), "aLogRecordNamesWhereItWasWrittenThroughTheFacadeToo", line)
        } finally {
            Telemetry.install(previous)
        }
    }
}
