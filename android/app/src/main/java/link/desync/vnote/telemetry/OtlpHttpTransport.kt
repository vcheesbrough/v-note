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

// A token this close to expiry is treated as expired: it could lapse in flight,
// or on a device clock a little behind the identity provider's.
private const val EXPIRY_MARGIN_SECONDS = 30L

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
//
// It never refreshes a token itself — the app's API calls do that — so an
// expired one means "not ready": items stay queued until the app has a fresh
// token, rather than being spent on a certain 401 that would also back off.
internal class OtlpHttpTransport(
    baseUrl: String,
    private val accessToken: () -> String?,
    // Epoch seconds, as `TokenStore` keeps it; null when unknown, which is
    // treated as unexpired, as the app's own refresh check does.
    private val accessTokenExpiry: () -> Long? = { null },
    private val nowEpochSeconds: () -> Long = { System.currentTimeMillis() / MILLIS_PER_SECOND },
    private val http: OkHttpClient =
        OkHttpClient
            .Builder()
            .callTimeout(EXPORT_CALL_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .followRedirects(false)
            .build(),
) : Transport {
    private val endpoint = "${baseUrl.trimEnd('/')}/otlp/android/v1"
    private val json = "application/json".toMediaType()

    override fun isReady(): Boolean = usableToken() != null

    override fun send(
        signal: Signal,
        body: String,
    ): ExportOutcome {
        val token = usableToken() ?: return ExportOutcome.Unavailable
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

    private fun usableToken(): String? {
        val token = accessToken()?.takeIf { it.isNotBlank() } ?: return null
        val expiry = accessTokenExpiry() ?: return token
        return token.takeIf { expiry > nowEpochSeconds() + EXPIRY_MARGIN_SECONDS }
    }

    private companion object {
        const val MILLIS_PER_SECOND = 1000L
    }
}

internal fun gzip(body: String): ByteArray {
    val buffer = Buffer()
    GzipSink(buffer).buffer().use { it.writeUtf8(body) }
    return buffer.readByteArray()
}
