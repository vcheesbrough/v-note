package link.desync.vnote.api

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import link.desync.vnote.api.codec.decodeLibraryEvent
import link.desync.vnote.api.codec.decodeMe
import link.desync.vnote.api.codec.decodePage
import link.desync.vnote.api.codec.decodePageEvent
import link.desync.vnote.api.codec.decodePageList
import link.desync.vnote.api.codec.encodeCreatePage
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

private const val REQUEST_ID_HEADER = "X-Request-Id"
private const val CORRELATION_ID_HEADER = "X-Correlation-Id"
private const val LOG_TAG = "VNoteApi"
private const val NORMAL_CLOSURE = 1000

// [ApiClient] over OkHttp: bearer-authenticated REST with one refresh-and-retry
// on 401, and the two realtime WebSockets. Wire formats live in `api.codec`.
class OkHttpApiClient(
    private val baseUrl: String,
    private val tokenStore: TokenStore,
    private val authRepository: AuthRepository,
) : ApiClient {
    private val http = OkHttpClient()
    private val jsonMediaType = "application/json".toMediaType()
    private val thumbnailCache = ConcurrentHashMap<String, ByteArray>()

    override suspend fun fetchMe(): Result<MeProfile> =
        withContext(Dispatchers.IO) {
            requestMe(retryOnUnauthorized = true)
        }

    override suspend fun listPages(): Result<List<PageSummary>> =
        withContext(Dispatchers.IO) {
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl/api/pages", token)
                    .get()
                    .build()
            }.mapCatching { body -> decodePageList(JSONObject(body)) }
        }

    override suspend fun createPage(
        title: String?,
        paper: Paper,
    ): Result<PageSummary> =
        withContext(Dispatchers.IO) {
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
                val body = encodeCreatePage(title, paper).toRequestBody(jsonMediaType)
                authorizedRequest("$baseUrl/api/pages", token)
                    .post(body)
                    .build()
            }.mapCatching { body -> decodePage(JSONObject(body).getJSONObject("page")) }
        }

    override suspend fun deletePage(pageId: String): Result<Unit> =
        withContext(Dispatchers.IO) {
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl/api/pages/$pageId", token)
                    .delete()
                    .build()
            }.map { }
        }

    override suspend fun fetchThumbnail(url: String): Result<ByteArray> =
        withContext(Dispatchers.IO) {
            thumbnailCache[url]?.let { return@withContext Result.success(it) }
            makeAuthorizedApiRequestForBytes(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl$url", token).get().build()
            }.onSuccess { bytes -> thumbnailCache[url] = bytes }
        }

    override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? {
        val token = tokenStore.accessToken() ?: return null
        val request =
            authorizedRequest("${wsBaseUrl()}/api/realtime", token)
                .build()
        return http.newWebSocket(
            request,
            object : WebSocketListener() {
                override fun onMessage(
                    webSocket: WebSocket,
                    text: String,
                ) {
                    runCatching { decodeLibraryEvent(JSONObject(text)) }
                        .onSuccess(listener::onEvent)
                        .onFailure { listener.onError(it.message ?: "Invalid realtime event") }
                }

                override fun onFailure(
                    webSocket: WebSocket,
                    t: Throwable,
                    response: Response?,
                ) {
                    logWebSocketFailure("library realtime", response, t)
                    listener.onError(
                        requestFailureMessage(
                            t.message ?: "Realtime connection failed",
                            response,
                        ),
                    )
                }

                // Same handshake reply as the page channel: unanswered, a
                // server-initiated close never reaches `onClosed`, so the
                // library would sit on a dead socket with no banner and no
                // reconnect — live page, thumbnail and re-sort events silently
                // stop arriving.
                override fun onClosing(
                    webSocket: WebSocket,
                    code: Int,
                    reason: String,
                ) {
                    webSocket.close(NORMAL_CLOSURE, null)
                }

                override fun onClosed(
                    webSocket: WebSocket,
                    code: Int,
                    reason: String,
                ) {
                    listener.onClosed()
                }
            },
        )
    }

    override fun openPageSocket(
        pageId: String,
        listener: PageEventListener,
    ): PageSocket? {
        val token = tokenStore.accessToken() ?: return null
        val request =
            authorizedRequest("${wsBaseUrl()}/api/pages/$pageId/realtime", token)
                .build()
        val webSocket =
            http.newWebSocket(
                request,
                object : WebSocketListener() {
                    override fun onMessage(
                        webSocket: WebSocket,
                        text: String,
                    ) {
                        runCatching { decodePageEvent(JSONObject(text)) }
                            .onSuccess { event -> event?.let(listener::onEvent) }
                            .onFailure { listener.onError(it.message ?: "Invalid page event") }
                    }

                    override fun onFailure(
                        webSocket: WebSocket,
                        t: Throwable,
                        response: Response?,
                    ) {
                        logWebSocketFailure("page realtime", response, t)
                        listener.onError(
                            requestFailureMessage(
                                t.message ?: "Page connection failed",
                                response,
                            ),
                        )
                    }

                    // A close the *server* initiates arrives here, and OkHttp
                    // reports `onClosed` only once the handshake is answered.
                    // Without this reply the channel would go quiet with no
                    // disconnect ever surfaced — and since a replay is painted
                    // only when it ends, the page would stay blank.
                    //
                    // Answering is all this needs to do: `onClosed` follows and
                    // notifies the listener, so there is one notification path
                    // rather than two that happen to be idempotent.
                    override fun onClosing(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        // Always 1000: `close` rejects most codes a peer may
                        // legitimately send back (1001, 1011, …).
                        webSocket.close(NORMAL_CLOSURE, null)
                    }

                    override fun onClosed(
                        webSocket: WebSocket,
                        code: Int,
                        reason: String,
                    ) {
                        listener.onClosed()
                    }
                },
            )
        return PageSocket(webSocket)
    }

    override fun shutdown() {
        http.dispatcher.executorService.shutdown()
        http.connectionPool.evictAll()
        http.cache?.close()
    }

    private fun wsBaseUrl(): String =
        when {
            baseUrl.startsWith("https://") -> baseUrl.replaceFirst("https://", "wss://")
            baseUrl.startsWith("http://") -> baseUrl.replaceFirst("http://", "ws://")
            else -> baseUrl
        }

    private suspend fun requestMe(retryOnUnauthorized: Boolean): Result<MeProfile> {
        val accessToken =
            tokenStore.accessToken()
                ?: return Result.failure(IllegalStateException("Not signed in"))

        val request =
            authorizedRequest("$baseUrl/api/me", accessToken)
                .get()
                .build()

        http.newCall(request).execute().use { response ->
            if (response.code == 401 && retryOnUnauthorized) {
                val refreshed = authRepository.refreshAccessTokenIfNeeded(force = true)
                if (refreshed) {
                    return requestMe(retryOnUnauthorized = false)
                }
                return Result.failure(IllegalStateException("Session expired"))
            }
            if (!response.isSuccessful) {
                logHttpFailure("GET /api/me", response)
                return Result.failure(
                    IllegalStateException(requestFailureMessage("GET /api/me failed", response)),
                )
            }
            val body = response.body.string()
            return Result.success(decodeMe(JSONObject(body)))
        }
    }

    private suspend fun makeAuthorizedApiRequest(
        retryOnUnauthorized: Boolean,
        buildRequest: (String) -> Request,
    ): Result<String> {
        val accessToken =
            tokenStore.accessToken()
                ?: return Result.failure(IllegalStateException("Not signed in"))

        http.newCall(buildRequest(accessToken)).execute().use { response ->
            if (response.code == 401 && retryOnUnauthorized) {
                val refreshed = authRepository.refreshAccessTokenIfNeeded(force = true)
                if (refreshed) {
                    return makeAuthorizedApiRequest(retryOnUnauthorized = false, buildRequest)
                }
                return Result.failure(IllegalStateException("Session expired"))
            }
            if (!response.isSuccessful) {
                logHttpFailure("API request", response)
                return Result.failure(
                    IllegalStateException(requestFailureMessage("API request failed", response)),
                )
            }
            return Result.success(response.body.string())
        }
    }

    private suspend fun makeAuthorizedApiRequestForBytes(
        retryOnUnauthorized: Boolean,
        buildRequest: (String) -> Request,
    ): Result<ByteArray> {
        val accessToken =
            tokenStore.accessToken()
                ?: return Result.failure(IllegalStateException("Not signed in"))
        http.newCall(buildRequest(accessToken)).execute().use { response ->
            if (response.code == 401 && retryOnUnauthorized) {
                if (authRepository.refreshAccessTokenIfNeeded(force = true)) {
                    return makeAuthorizedApiRequestForBytes(false, buildRequest)
                }
                return Result.failure(IllegalStateException("Session expired"))
            }
            if (!response.isSuccessful) {
                logHttpFailure("thumbnail request", response)
                return Result.failure(IllegalStateException(requestFailureMessage("Thumbnail request failed", response)))
            }
            return Result.success(response.body.bytes())
        }
    }

    private fun authorizedRequest(
        url: String,
        token: String,
    ): Request.Builder =
        Request
            .Builder()
            .url(url)
            .header("Authorization", "Bearer $token")
            .header(REQUEST_ID_HEADER, requestId())

    private fun requestFailureMessage(
        prefix: String,
        response: Response?,
    ): String {
        val status = response?.code?.let { ": HTTP $it" }.orEmpty()
        val requestId = response?.requestId()?.let { " request_id=$it" }.orEmpty()
        return "$prefix$status$requestId"
    }

    private fun logHttpFailure(
        context: String,
        response: Response,
    ) {
        Log.w(LOG_TAG, "$context failed: HTTP ${response.code} request_id=${response.requestId().orEmpty()}")
    }

    private fun logWebSocketFailure(
        context: String,
        response: Response?,
        throwable: Throwable,
    ) {
        Log.w(
            LOG_TAG,
            "$context failed request_id=${response?.requestId().orEmpty()}",
            throwable,
        )
    }

    private fun Response.requestId(): String? =
        header(REQUEST_ID_HEADER)
            ?: header(CORRELATION_ID_HEADER)

    private fun requestId(): String = "android_${UUID.randomUUID()}"
}
