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
import java.util.concurrent.ConcurrentHashMap

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
    private val thumbnailCache = ConcurrentHashMap<String, ByteArray>()

    suspend fun fetchMe(): Result<MeProfile> =
        withContext(Dispatchers.IO) {
            requestMe(retryOnUnauthorized = true)
        }

    suspend fun listPages(): Result<List<PageSummary>> =
        withContext(Dispatchers.IO) {
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
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
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
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
            makeAuthorizedApiRequest(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl/api/pages/$pageId", token)
                    .delete()
                    .build()
            }.map { }
        }

    suspend fun fetchThumbnail(url: String): Result<ByteArray> =
        withContext(Dispatchers.IO) {
            thumbnailCache[url]?.let { return@withContext Result.success(it) }
            makeAuthorizedApiRequestForBytes(retryOnUnauthorized = true) { token ->
                authorizedRequest("$baseUrl$url", token).get().build()
            }.onSuccess { bytes -> thumbnailCache[url] = bytes }
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

    private suspend fun makeAuthorizedApiRequest(
        retryOnUnauthorized: Boolean,
        buildRequest: (String) -> Request,
    ): Result<String> {
        val accessToken = tokenStore.accessToken()
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
            return Result.success(response.body?.string().orEmpty())
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
            return Result.success(response.body?.bytes() ?: byteArrayOf())
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
    val thumbnail: ThumbnailMetadata = ThumbnailMetadata.Empty,
)

sealed interface ThumbnailMetadata {
    data object Empty : ThumbnailMetadata
    data class Generating(val sourceSeq: Long) : ThumbnailMetadata
    data class Available(val sourceSeq: Long, val url: String) : ThumbnailMetadata
    data class Failed(val sourceSeq: Long) : ThumbnailMetadata
}

sealed interface LibraryEvent {
    data class PageCreated(val page: PageSummary) : LibraryEvent

    data class PageDeleted(val pageId: String) : LibraryEvent
    data class PageThumbnailUpdated(val pageId: String, val thumbnail: ThumbnailMetadata) : LibraryEvent
    data class PageUpdated(val pageId: String, val updatedAt: String) : LibraryEvent
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
        thumbnail = json.optJSONObject("thumbnail")?.let(::parseThumbnail) ?: ThumbnailMetadata.Empty,
    )

private fun parseThumbnail(json: JSONObject): ThumbnailMetadata =
    when (json.getString("status")) {
        "empty" -> ThumbnailMetadata.Empty
        "generating" -> ThumbnailMetadata.Generating(json.getLong("source_seq"))
        "available" -> ThumbnailMetadata.Available(json.getLong("source_seq"), json.getString("url"))
        "failed" -> ThumbnailMetadata.Failed(json.getLong("source_seq"))
        else -> error("Unknown thumbnail status")
    }

private fun parseLibraryEvent(json: JSONObject): LibraryEvent =
    when (val type = json.getString("type")) {
        "page-created" -> LibraryEvent.PageCreated(parsePage(json.getJSONObject("page")))
        "page-deleted" -> LibraryEvent.PageDeleted(json.getString("page_id"))
        "page-thumbnail-updated" -> LibraryEvent.PageThumbnailUpdated(
            json.getString("page_id"), parseThumbnail(json.getJSONObject("thumbnail")),
        )
        "page-updated" -> LibraryEvent.PageUpdated(
            json.getString("page_id"), json.getString("updated_at"),
        )
        else -> error("Unknown realtime event type: $type")
    }

// ---- Canonical ink (world-space strokes) ---------------------------------

// A single stroke sample in world/document coordinates. `t` is milliseconds
// relative to the start of the stroke. `pressure` is a normalised 0.0..1.0
// value present only on pressure-sensitive (solid_round v2) strokes; v1 strokes
// leave it null. A v2 point with null pressure renders at full width.
data class StrokePoint(
    val x: Double,
    val y: Double,
    val t: Long,
    val pressure: Double? = null,
)

// solid_round style discriminator versions, mirrored from `crates/protocol`.
const val SOLID_ROUND_TOOL = "solid_round"
const val SOLID_ROUND_STYLE_VERSION = 1
const val SOLID_ROUND_PRESSURE_STYLE_VERSION = 2

// Shared, cross-platform pressure→width curve constant (see
// `protocol::MIN_PRESSURE_WIDTH`). Absolute rendered nib diameter (world logical
// px) at zero pressure; the curve interpolates linearly from this floor up to
// the preset width, so a heavy pen still tapers to a thin line.
const val MIN_PRESSURE_WIDTH = 1.5

// One captured stroke uses an immutable style snapshot captured at stylus-down.
// The server accepts only the v3 solid_round style, but the discriminated shape
// keeps historical ink unambiguous when future tools arrive.
data class SolidRoundParameters(
    val color: String = "#006400",
    val width: Double = 4.0,
    val capStyle: String = "round",
    val joinStyle: String = "round",
)

data class StrokeStyle(
    val toolKind: String = SOLID_ROUND_TOOL,
    val styleVersion: Int = SOLID_ROUND_STYLE_VERSION,
    val parameters: SolidRoundParameters = SolidRoundParameters(),
) {
    // True when this style modulates rendered width by per-point pressure.
    val isPressureSensitive: Boolean
        get() = toolKind == SOLID_ROUND_TOOL && styleVersion == SOLID_ROUND_PRESSURE_STYLE_VERSION

    // Rendered nib diameter for a point carrying the given optional pressure.
    // Must stay identical to `protocol::StrokeStyle::rendered_width`. v1 is
    // constant; v2 interpolates linearly from an absolute MIN_PRESSURE_WIDTH
    // floor (capped at the preset) up to the preset, treating null as full width.
    fun renderedWidth(pressure: Double?): Double =
        if (isPressureSensitive) {
            val p = (pressure ?: 1.0).coerceIn(0.0, 1.0)
            val preset = parameters.width
            val floor = minOf(MIN_PRESSURE_WIDTH, preset)
            floor + (preset - floor) * p
        } else {
            parameters.width
        }
}

data class Stroke(
    val points: List<StrokePoint>,
    val id: String = "stroke_${UUID.randomUUID().toString().replace("-", "")}",
    val style: StrokeStyle = StrokeStyle(),
) {
    companion object {
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

    data class TombstoneBatch(
        val revision: Long,
        val clientMutationId: String,
        val strokeIds: List<String>,
    ) : PageEvent

    data object LeaseGranted : PageEvent

    data class LeaseDenied(val holder: String) : PageEvent

    data class LeaseChanged(val holder: String?) : PageEvent

    data class Failure(
        val code: String,
        val message: String,
        val clientMutationId: String?,
    ) : PageEvent
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

    fun commitTombstones(clientMutationId: String, strokeIds: List<String>) {
        webSocket.send(
            JSONObject()
                .put("type", "commit-tombstones")
                .put("client_mutation_id", clientMutationId)
                .put("stroke_ids", org.json.JSONArray(strokeIds))
                .toString(),
        )
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
        "tombstone-batch" ->
            PageEvent.TombstoneBatch(
                revision = json.getLong("revision"),
                clientMutationId = json.getString("client_mutation_id"),
                strokeIds =
                    json.getJSONArray("stroke_ids").let { ids ->
                        buildList { for (index in 0 until ids.length()) add(ids.getString(index)) }
                    },
            )
        "lease-granted" -> PageEvent.LeaseGranted
        "lease-denied" -> PageEvent.LeaseDenied(json.getString("holder"))
        "lease-changed" -> PageEvent.LeaseChanged(json.optString("holder").ifBlank { null })
        "error" ->
            PageEvent.Failure(
                code = json.optString("code"),
                message = json.optString("message"),
                clientMutationId = json.optString("client_mutation_id").ifBlank { null },
            )
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
                        pressure =
                            if (point.has("pressure") && !point.isNull("pressure")) {
                                point.getDouble("pressure")
                            } else {
                                null
                            },
                    ),
                )
            }
        }
    return Stroke(
        points = points,
        id = json.getString("id"),
        style = parseStrokeStyle(json.getJSONObject("style")),
    )
}

private fun parseStrokeStyle(json: JSONObject): StrokeStyle {
    val parameters = json.getJSONObject("parameters")
    return StrokeStyle(
        toolKind = json.getString("tool_kind"),
        styleVersion = json.getInt("style_version"),
        parameters = SolidRoundParameters(
            color = parameters.getString("color"),
            width = parameters.getDouble("width"),
            capStyle = parameters.getString("cap_style"),
            joinStyle = parameters.getString("join_style"),
        ),
    )
}

private fun encodeStrokeStyle(style: StrokeStyle): JSONObject =
    JSONObject()
        .put("tool_kind", style.toolKind)
        .put("style_version", style.styleVersion)
        .put(
            "parameters",
            JSONObject()
                .put("color", style.parameters.color)
                .put("width", style.parameters.width)
                .put("cap_style", style.parameters.capStyle)
                .put("join_style", style.parameters.joinStyle),
        )

private fun encodeCommitBatch(
    clientBatchId: String,
    strokes: List<Stroke>,
): String {
    val strokesArray = org.json.JSONArray()
    for (stroke in strokes) {
        val pointsArray = org.json.JSONArray()
        for (point in stroke.points) {
            val pointJson =
                JSONObject()
                    .put("x", point.x)
                    .put("y", point.y)
                    .put("t", point.t)
            // Emit pressure only when present, so v1 strokes stay byte-identical
            // on the wire (absent, never `null`).
            point.pressure?.let { pointJson.put("pressure", it) }
            pointsArray.put(pointJson)
        }
        strokesArray.put(
            JSONObject()
                .put("id", stroke.id)
                .put("style", encodeStrokeStyle(stroke.style))
                .put("points", pointsArray),
        )
    }
    return JSONObject()
        .put("type", "commit-batch")
        .put("client_batch_id", clientBatchId)
        .put("strokes", strokesArray)
        .toString()
}
