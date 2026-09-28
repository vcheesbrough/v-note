package link.desync.vnote.telemetry

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okio.Buffer
import okio.GzipSink
import okio.buffer
import org.json.JSONException
import org.json.JSONObject
import java.io.IOException
import java.util.concurrent.TimeUnit

private const val EXPORT_CALL_TIMEOUT_SECONDS = 10L

// A token this close to expiry is treated as expired: it could lapse in flight,
// or on a device clock a little behind the identity provider's.
private const val EXPIRY_MARGIN_SECONDS = 30L

private const val HTTP_OK = 200
private const val HTTP_NO_CONTENT = 204
private const val HTTP_NOT_FOUND = 404

// The network side of client telemetry (#439):
//
// - `GET {serverBaseUrl}/api/telemetry/config` — where to send. The only
//   address compiled into the app is its own server's; the ingest's comes from
//   there, so an environment can move or switch off its ingest without a
//   release.
// - `POST {endpoint}/v1/{signal}` — the ingest itself (`otlp-collector-oidc`),
//   with the same bearer token every API call carries. There is no second
//   credential and no collector reachable from the device.
//
// The token is read **per request**, never bound at construction, so the one
// the app's OIDC stack refreshed is the one the next export uses. It never
// refreshes a token itself — two refreshers racing on one refresh token could
// sign the user out — so an expired token means "not ready": items stay queued
// until the app has a fresh one.
//
// Bodies are gzipped (`Content-Encoding: gzip`) to cut radio time: OTLP/JSON
// repeats every key and compresses about 10:1, and the ingest caps the
// *decompressed* size.
//
// Its own OkHttp client, with no tracing interceptor: an export must never
// produce a span, or every export would queue the next one.
internal class OtlpHttpTransport(
    serverBaseUrl: String,
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
    private val configUrl = "${serverBaseUrl.trimEnd('/')}/api/telemetry/config"
    private val json = "application/json".toMediaType()

    override fun isReady(): Boolean = usableToken() != null

    override fun credentialId(): Int? = usableToken()?.hashCode()

    override fun fetchConfig(): ConfigFetch {
        val token = usableToken() ?: return ConfigFetch.NotYet
        val request =
            Request
                .Builder()
                .url(configUrl)
                .header("Authorization", "Bearer $token")
                .get()
                .build()
        return try {
            http.newCall(request).execute().use { response ->
                when (response.code) {
                    HTTP_OK -> parseConfig(response.body.string())
                    HTTP_NO_CONTENT, HTTP_NOT_FOUND -> ConfigFetch.Absent
                    else -> ConfigFetch.NotYet
                }
            }
        } catch (_: IOException) {
            ConfigFetch.NotYet
        }
    }

    override fun send(
        endpoint: String,
        signal: Signal,
        body: String,
    ): ExportOutcome {
        // Lapsed between the readiness check and now: not a refusal by the
        // ingest, so not a 401 — try again once the app has refreshed.
        val token = usableToken() ?: return ExportOutcome.Retryable(null)
        val request =
            Request
                .Builder()
                .url("${endpoint.trimEnd('/')}/v1/${signal.path}")
                .header("Authorization", "Bearer $token")
                .header("Content-Encoding", "gzip")
                .post(gzip(body).toRequestBody(json))
                .build()
        return try {
            http.newCall(request).execute().use { response ->
                ExportOutcome.fromResponse(response.code, response.header("Retry-After"))
            }
        } catch (_: IOException) {
            ExportOutcome.fromResponse(null)
        } catch (_: IllegalArgumentException) {
            // A configured endpoint OkHttp cannot parse: nothing to retry.
            ExportOutcome.Rejected(0)
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

// The config route's `200` body. Android uses the endpoint only: it holds its
// own token, so the one in the body is ignored. A body it cannot read is "not
// answered yet", not "no configuration".
internal fun parseConfig(body: String): ConfigFetch =
    try {
        val endpoint = JSONObject(body).optString("endpoint")
        if (endpoint.startsWith("https://") || endpoint.startsWith("http://")) {
            ConfigFetch.Configured(endpoint.trimEnd('/'))
        } else {
            ConfigFetch.NotYet
        }
    } catch (_: JSONException) {
        ConfigFetch.NotYet
    }

internal fun gzip(body: String): ByteArray {
    val buffer = Buffer()
    GzipSink(buffer).buffer().use { it.writeUtf8(body) }
    return buffer.readByteArray()
}
