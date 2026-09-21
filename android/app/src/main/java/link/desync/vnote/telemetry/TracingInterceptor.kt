package link.desync.vnote.telemetry

import okhttp3.Interceptor
import okhttp3.Request
import okhttp3.Response
import java.io.IOException

internal const val TRACEPARENT_HEADER = "traceparent"

private const val FIRST_ERROR_STATUS = 400

// What a request is part of, attached as an OkHttp tag where the request is
// built. [parent] is captured *there*, not when the call runs, so a request
// queued before a screen change stays in the trace it was made in.
//
// [route] is the *template* — `/api/pages/{page_id}`, never the concrete path:
// it is how spans are grouped, and an id in it would make every page its own
// operation.
data class TraceTag(
    val parent: SpanContext,
    val route: String,
)

fun Request.Builder.traced(
    route: String,
    parent: SpanContext = Telemetry.screen(),
): Request.Builder = tag(TraceTag::class.java, TraceTag(parent, route))

// Opens an `http.client` span per request and sends its `traceparent`, so the
// server's spans become its children — the same span the SPA opens (#354).
//
// This includes the two WebSocket upgrades. Unlike a browser, OkHttp can put a
// header on an upgrade, so the server parents the connection to the app's span
// directly and the realtime-ticket detour the SPA needs does not apply.
//
// Replaces nothing: `X-Request-Id` is still sent, and is still what a failure
// message quotes.
class TracingInterceptor(
    private val runtime: () -> TelemetryRuntime = { Telemetry.runtime },
) : Interceptor {
    override fun intercept(chain: Interceptor.Chain): Response {
        val request = chain.request()
        val tag = request.tag(TraceTag::class.java)
        val telemetry = runtime()
        val span =
            telemetry
                .span("http.client", tag?.parent ?: telemetry.screen(), SpanKind.Client)
                .attr("http.request.method", request.method)
                .attr("url.template", tag?.route ?: "unknown")
        val traced =
            request
                .newBuilder()
                .header(TRACEPARENT_HEADER, span.context.traceparent())
                .build()
        val response =
            try {
                chain.proceed(traced)
            } catch (error: IOException) {
                span.fail(error.javaClass.simpleName)
                throw error
            }
        span.attr("http.response.status_code", response.code)
        if (response.code >= FIRST_ERROR_STATUS) {
            span.fail("HTTP ${response.code}")
        } else {
            span.end()
        }
        return response
    }
}
