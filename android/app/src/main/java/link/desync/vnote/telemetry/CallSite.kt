package link.desync.vnote.telemetry

// Where a span was opened or a log line written, as OpenTelemetry's `code.*`
// and `thread.*` attributes (#453), so Tempo and Loki can jump from a client
// item to the line of app code that produced it — as they already can for the
// server, whose `tracing` spans carry the same keys.
//
// Taken from a stack trace, so it costs one stack capture per span or log. That
// is fine at the rates this package is called — per screen, connection, request
// or stroke, never per ink point — and must stay that way: nothing hotter than
// a stroke may open a span.
//
// It depends on the frames keeping their real class names and line numbers,
// which they do because R8 minification is off (see `isMinifyEnabled` in
// `app/build.gradle.kts`). With minification on, these would name obfuscated
// frames until retraced against the build's mapping file.

internal const val CODE_FUNCTION_NAME = "code.function.name"
internal const val CODE_FILE_PATH = "code.file.path"
internal const val CODE_LINE_NUMBER = "code.line.number"
internal const val THREAD_NAME = "thread.name"
internal const val THREAD_ID = "thread.id"

private const val APP_PACKAGE = "link.desync.vnote."
private const val TELEMETRY_PACKAGE = "link.desync.vnote.telemetry."

// The classes a caller goes *through* to reach the export queue. A frame in one
// of these is never a call site. Kotlin puts this file's and Telemetry.kt's
// top-level functions in `CallSiteKt` and `TelemetryKt`, and a lambda or
// companion in `Outer$…`, so matching is on the class name before any `$`.
private val RUNTIME_CLASSES =
    setOf(
        "${TELEMETRY_PACKAGE}Telemetry",
        "${TELEMETRY_PACKAGE}TelemetryKt",
        "${TELEMETRY_PACKAGE}TelemetryRuntime",
        "${TELEMETRY_PACKAGE}OpenSpan",
        "${TELEMETRY_PACKAGE}AppLog",
        "${TELEMETRY_PACKAGE}CallSiteKt",
    )

// App methods that sit *between* the code that asked for a request or a log line
// and the telemetry call, shared by every caller: attributing to one of these
// would name the same helper for every request (`makeAuthorizedApiRequest` for
// listPages, createPage and deletePage alike) instead of the method that made
// it. Keyed by class, matched on the source name of the method: see
// [sourceMethodName] for the two ways the JVM name differs from it.
//
// Add a helper here when it both (a) is called from several app methods and
// (b) runs the request or writes the log on their behalf. A rename is caught
// by `CallSiteTest`, which checks every entry against the real class.
internal val PLUMBING_METHODS: Map<String, Set<String>> =
    mapOf(
        "link.desync.vnote.api.OkHttpApiClient" to
            setOf(
                "requestMe",
                "makeAuthorizedApiRequest",
                "makeAuthorizedApiRequestForBytes",
                "logHttpFailure",
                "logWebSocketFailure",
            ),
    )

private const val SYNTHETIC_ACCESSOR = "access$"

// A frame's method name as the source spells it. Kotlin compiles a private
// function a lambda calls with a synthetic `access$<name>` accessor, and a
// function taking or returning an inline class (`Result`, which every
// OkHttpApiClient helper returns) under a mangled name, `<name>-<hash>`, e.g.
// `makeAuthorizedApiRequest-0E7RQCE`. `-` cannot occur in a Kotlin name, so
// everything from it on is the mangling.
internal fun sourceMethodName(jvmName: String): String = jvmName.removePrefix(SYNTHETIC_ACCESSOR).substringBefore('-')

private fun StackTraceElement.isPlumbing(): Boolean = PLUMBING_METHODS[className]?.contains(sourceMethodName(methodName)) == true

// The calling thread and the frame that called into telemetry, as attributes.
internal fun callSite(thread: Thread = Thread.currentThread()): List<Attribute> =
    callSiteAttributes(Throwable("telemetry call site").stackTrace, thread)

internal fun callSiteAttributes(
    frames: Array<StackTraceElement>,
    thread: Thread,
): List<Attribute> =
    buildList {
        selectCallSite(frames)?.let { frame -> addAll(frameAttributes(frame)) }
        add(Attribute(THREAD_NAME, thread.name))
        // getId(), not the JDK 19 threadId(), which older Android lacks.
        @Suppress("DEPRECATION")
        add(Attribute(THREAD_ID, thread.id))
    }

// The frame to attribute to. First choice: the innermost frame of *app* code
// outside the telemetry package and outside [PLUMBING_METHODS] — the screen,
// session or API method that opened the span, even when OkHttp's interceptor
// chain and a shared request helper sit between it and [TracingInterceptor].
// A request run inside `withContext` is attributed to the coroutine body, whose
// frame is `Outer$method$N.invokeSuspend`: the name still says which method.
// Failing that (a request OkHttp runs on its own dispatcher thread, with no app
// frame on the stack; a test in this package), the innermost frame that is not
// the telemetry runtime itself, which is the code that actually opened the span.
internal fun selectCallSite(frames: Array<StackTraceElement>): StackTraceElement? =
    frames.firstOrNull { it.isApp() && !it.isPlumbing() }
        // Only a helper on the stack (nothing calls one like that today): still
        // better than OkHttp or the runtime.
        ?: frames.firstOrNull { it.isApp() }
        ?: frames.firstOrNull { it.className.substringBefore('$') !in RUNTIME_CLASSES }

private fun StackTraceElement.isApp(): Boolean = className.startsWith(APP_PACKAGE) && !className.startsWith(TELEMETRY_PACKAGE)

private fun frameAttributes(frame: StackTraceElement): List<Attribute> =
    buildList {
        add(Attribute(CODE_FUNCTION_NAME, "${frame.className}.${frame.methodName}"))
        frame.fileName?.let { add(Attribute(CODE_FILE_PATH, sourcePath(frame.className, it))) }
        if (frame.lineNumber > 0) add(Attribute(CODE_LINE_NUMBER, frame.lineNumber))
    }

// A JVM frame names only the file, not its directory. Sources here live in the
// directory their package names, so the package gives the path under
// `src/*/java/`, which is unique where the bare name might not be.
private fun sourcePath(
    className: String,
    fileName: String,
): String {
    val pkg = className.substringBeforeLast('.', missingDelimiterValue = "")
    return if (pkg.isEmpty()) fileName else "${pkg.replace('.', '/')}/$fileName"
}
