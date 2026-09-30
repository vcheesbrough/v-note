package link.desync.vnote.api

import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import kotlin.coroutines.cancellation.CancellationException

// One blocking `GET {baseUrl}/health`, as [OkHttpApiClient.checkHealth] runs it
// on the IO dispatcher. Any failure — an HTTP error, a refused connection, or an
// unexpected exception from OkHttp or an interceptor — is a failed [Result],
// never a throw: the probe loop and the screen above it must not crash the app
// over a health line (#186, PR #65 review). Coroutine cancellation is the one
// exception, and is rethrown.
//
// [decorate] adds what the app's own requests carry (request id, trace tag);
// [onFailure] reports each failed attempt with the response, when there was one.
internal class HealthCheck(
    private val http: OkHttpClient,
    private val baseUrl: String,
    private val decorate: (Request.Builder) -> Request.Builder = { it },
    private val onFailure: (reason: String, response: Response?) -> Unit = { _, _ -> },
) {
    fun check(): Result<Unit> {
        val request = decorate(Request.Builder().url("$baseUrl/health")).get().build()
        return runCatching {
            http.newCall(request).execute().use { response ->
                if (!response.isSuccessful) {
                    onFailure("HTTP ${response.code}", response)
                    throw HttpStatusFailure(response.code)
                }
            }
        }.onFailure { error ->
            if (error is CancellationException) throw error
            if (error !is HttpStatusFailure) onFailure(error.javaClass.simpleName, null)
        }
    }

    private class HttpStatusFailure(
        code: Int,
    ) : IllegalStateException("HTTP $code")
}
