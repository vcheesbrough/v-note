package link.desync.vnote.telemetry

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okio.Buffer
import okio.GzipSink
import okio.buffer
import java.io.IOException
import java.util.concurrent.TimeUnit

private const val EXPORT_CALL_TIMEOUT_SECONDS = 10L

// Sends OTLP/JSON to the app's own server: `POST {baseUrl}/otlp/android/v1/{signal}`,
// with the same bearer token every API call carries. The server authenticates
// it with the same `auth_middleware` and proxies the bytes, undecoded, to the
// sidecar's `android` receiver (#354) — so there is no second credential and no
// collector reachable from the device.
//
// Bodies are gzipped (`Content-Encoding: gzip`, which the ingress forwards) to
// cut radio time: OTLP/JSON repeats every key, and compresses about 10:1.
//
// Its own OkHttp client, with no tracing interceptor: an export must never
// produce a span, or every export would queue the next one.
internal class OtlpHttpTransport(
    baseUrl: String,
    private val accessToken: () -> String?,
    private val http: OkHttpClient =
        OkHttpClient
            .Builder()
            .callTimeout(EXPORT_CALL_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .followRedirects(false)
            .build(),
) : Transport {
    private val endpoint = "${baseUrl.trimEnd('/')}/otlp/android/v1"
    private val json = "application/json".toMediaType()

    override fun isReady(): Boolean = !accessToken().isNullOrBlank()

    override fun send(
        signal: Signal,
        body: String,
    ): ExportOutcome {
        val token = accessToken() ?: return ExportOutcome.Unavailable
        val request =
            Request
                .Builder()
                .url("$endpoint/${signal.path}")
                .header("Authorization", "Bearer $token")
                .header("Content-Encoding", "gzip")
                .post(gzip(body).toRequestBody(json))
                .build()
        return try {
            http.newCall(request).execute().use { response -> ExportOutcome.fromStatus(response.code) }
        } catch (_: IOException) {
            ExportOutcome.fromStatus(null)
        }
    }
}

internal fun gzip(body: String): ByteArray {
    val buffer = Buffer()
    GzipSink(buffer).buffer().use { it.writeUtf8(body) }
    return buffer.readByteArray()
}
