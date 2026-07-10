package link.desync.vnote.auth

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.UUID

private const val REQUEST_ID_HEADER = "X-Request-Id"
private const val CORRELATION_ID_HEADER = "X-Correlation-Id"
private const val LOG_TAG = "VNoteApi"

class ApiClient(
    private val baseUrl: String,
    private val tokenStore: TokenStore,
    private val authRepository: AuthRepository,
) {
    private val http = OkHttpClient()
    private val jsonMediaType = "application/json".toMediaType()

    suspend fun fetchMe(): Result<MeProfile> =
        withContext(Dispatchers.IO) {
            requestMe(retryOnUnauthorized = true)
        }

    suspend fun listPages(): Result<List<PageSummary>> =
        withContext(Dispatchers.IO) {
            executeAuthorized(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl/api/pages", token)
                    .get()
                    .build()
            }.mapCatching { body ->
                val pages = JSONObject(body).getJSONArray("pages")
                buildList {
                    for (index in 0 until pages.length()) {
                        add(parsePage(pages.getJSONObject(index)))
                    }
                }
            }
        }

    suspend fun createPage(title: String? = null): Result<PageSummary> =
        withContext(Dispatchers.IO) {
            executeAuthorized(retryOnUnauthorized = true) { token ->
                val body =
                    JSONObject()
                        .apply { title?.let { put("title", it) } }
                        .toString()
                        .toRequestBody(jsonMediaType)
                authorizedRequest("$baseUrl/api/pages", token)
                    .post(body)
                    .build()
            }.mapCatching { body -> parsePage(JSONObject(body).getJSONObject("page")) }
        }

    suspend fun deletePage(pageId: String): Result<Unit> =
        withContext(Dispatchers.IO) {
            executeAuthorized(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl/api/pages/$pageId", token)
                    .delete()
                    .build()
            }.map { }
        }

    fun openLibrarySocket(listener: LibraryEventListener): WebSocket? {
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
                    runCatching { parseLibraryEvent(JSONObject(text)) }
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

    // Per-page ink channel. Bidirectional: the caller subscribes for gap-fill,
    // acquires the edit lease, and commits coalesced stroke batches via the
    // returned [PageSocket]; server events arrive on [listener].
    fun openPageSocket(
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
                        runCatching { parsePageEvent(JSONObject(text)) }
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

    fun shutdown() {
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
            val body = response.body?.string().orEmpty()
            val json = JSONObject(body)
            return Result.success(
                MeProfile(
                    sub = json.getString("sub"),
                    email = json.optString("email").ifBlank { null },
                ),
            )
        }
    }

    private suspend fun executeAuthorized(
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
                    return executeAuthorized(retryOnUnauthorized = false, buildRequest)
                }
                return Result.failure(IllegalStateException("Session expired"))
            }
            if (!response.isSuccessful) {
                logHttpFailure("API request", response)
                return Result.failure(
                    IllegalStateException(requestFailureMessage("API request failed", response)),
                )
            }
            return Result.success(response.body?.string().orEmpty())
        }
    }

    private fun authorizedRequest(
        url: String,
        token: String,
    ): Request.Builder =
        Request.Builder()
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

data class MeProfile(
    val sub: String,
    val email: String?,
)

data class PageSummary(
    val id: String,
    val title: String,
    val createdAt: String,
    val updatedAt: String,
)

sealed interface LibraryEvent {
    data class PageCreated(val page: PageSummary) : LibraryEvent

    data class PageDeleted(val pageId: String) : LibraryEvent
}

interface LibraryEventListener {
    fun onEvent(event: LibraryEvent)

    fun onError(message: String)

    fun onClosed()
}

private fun parsePage(json: JSONObject): PageSummary =
    PageSummary(
        id = json.getString("id"),
        title = json.getString("title"),
        createdAt = json.getString("created_at"),
        updatedAt = json.getString("updated_at"),
    )

private fun parseLibraryEvent(json: JSONObject): LibraryEvent =
    when (val type = json.getString("type")) {
        "page-created" -> LibraryEvent.PageCreated(parsePage(json.getJSONObject("page")))
        "page-deleted" -> LibraryEvent.PageDeleted(json.getString("page_id"))
        else -> error("Unknown realtime event type: $type")
    }

// ---- Canonical ink (world-space strokes) ---------------------------------

// A single stroke sample in world/document coordinates. `t` is milliseconds
// relative to the start of the stroke. MVP omits pressure.
data class StrokePoint(
    val x: Double,
    val y: Double,
    val t: Long,
)

// One captured stroke. MVP uses a single hardcoded pen (see companion).
data class Stroke(
    val points: List<StrokePoint>,
    val tool: String = PEN_TOOL,
    val color: String = PEN_COLOR,
    val width: Double = PEN_WIDTH,
) {
    companion object {
        const val PEN_TOOL = "pen"
        const val PEN_COLOR = "#006400"
        const val PEN_WIDTH = 4.0
    }
}

// Server → client messages on the page channel.
sealed interface PageEvent {
    data class Welcome(
        val sessionId: String,
        val lastSeq: Long,
        val leaseHolder: String?,
    ) : PageEvent

    data class StrokeBatch(
        val seq: Long,
        val clientBatchId: String,
        val strokes: List<Stroke>,
    ) : PageEvent

    data class Synced(val lastSeq: Long) : PageEvent

    data object LeaseGranted : PageEvent

    data class LeaseDenied(val holder: String) : PageEvent

    data class LeaseChanged(val holder: String?) : PageEvent

    data class Failure(val code: String, val message: String) : PageEvent
}

interface PageEventListener {
    fun onEvent(event: PageEvent)

    fun onError(message: String)

    fun onClosed()
}

// Handle for sending client → server messages on an open page channel.
class PageSocket(private val webSocket: WebSocket) {
    fun subscribe(fromSeq: Long) {
        webSocket.send(JSONObject().put("type", "subscribe").put("from_seq", fromSeq).toString())
    }

    fun acquireLease() {
        webSocket.send(JSONObject().put("type", "acquire-lease").toString())
    }

    fun renewLease() {
        webSocket.send(JSONObject().put("type", "renew-lease").toString())
    }

    fun releaseLease() {
        webSocket.send(JSONObject().put("type", "release-lease").toString())
    }

    fun commitBatch(
        clientBatchId: String,
        strokes: List<Stroke>,
    ) {
        webSocket.send(encodeCommitBatch(clientBatchId, strokes))
    }

    fun close() {
        webSocket.close(1000, "page closed")
    }
}

private fun parsePageEvent(json: JSONObject): PageEvent? =
    when (json.getString("type")) {
        "welcome" ->
            PageEvent.Welcome(
                sessionId = json.getString("session_id"),
                lastSeq = json.getLong("last_seq"),
                leaseHolder = json.optString("lease_holder").ifBlank { null },
            )
        "stroke-batch" ->
            PageEvent.StrokeBatch(
                seq = json.getLong("seq"),
                clientBatchId = json.getString("client_batch_id"),
                strokes = parseStrokes(json.getJSONArray("strokes")),
            )
        "synced" -> PageEvent.Synced(json.getLong("last_seq"))
        "lease-granted" -> PageEvent.LeaseGranted
        "lease-denied" -> PageEvent.LeaseDenied(json.getString("holder"))
        "lease-changed" -> PageEvent.LeaseChanged(json.optString("holder").ifBlank { null })
        "error" -> PageEvent.Failure(json.optString("code"), json.optString("message"))
        // Unknown/forward-compatible message types are ignored.
        else -> null
    }

private fun parseStrokes(array: org.json.JSONArray): List<Stroke> =
    buildList {
        for (index in 0 until array.length()) {
            add(parseStroke(array.getJSONObject(index)))
        }
    }

private fun parseStroke(json: JSONObject): Stroke {
    val pointsArray = json.getJSONArray("points")
    val points =
        buildList {
            for (index in 0 until pointsArray.length()) {
                val point = pointsArray.getJSONObject(index)
                add(
                    StrokePoint(
                        x = point.getDouble("x"),
                        y = point.getDouble("y"),
                        t = point.getLong("t"),
                    ),
                )
            }
        }
    return Stroke(
        points = points,
        tool = json.optString("tool", Stroke.PEN_TOOL),
        color = json.optString("color", Stroke.PEN_COLOR),
        width = json.optDouble("width", Stroke.PEN_WIDTH),
    )
}

private fun encodeCommitBatch(
    clientBatchId: String,
    strokes: List<Stroke>,
): String {
    val strokesArray = org.json.JSONArray()
    for (stroke in strokes) {
        val pointsArray = org.json.JSONArray()
        for (point in stroke.points) {
            pointsArray.put(
                JSONObject()
                    .put("x", point.x)
                    .put("y", point.y)
                    .put("t", point.t),
            )
        }
        strokesArray.put(
            JSONObject()
                .put("tool", stroke.tool)
                .put("color", stroke.color)
                .put("width", stroke.width)
                .put("points", pointsArray),
        )
    }
    return JSONObject()
        .put("type", "commit-batch")
        .put("client_batch_id", clientBatchId)
        .put("strokes", strokesArray)
        .toString()
}
