package link.desync.vnote.auth

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
                Request.Builder()
                    .url("$baseUrl/api/pages")
                    .header("Authorization", "Bearer $token")
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

    suspend fun createPage(title: String = "Untitled page"): Result<PageSummary> =
        withContext(Dispatchers.IO) {
            executeAuthorized(retryOnUnauthorized = true) { token ->
                val body = JSONObject().put("title", title).toString().toRequestBody(jsonMediaType)
                Request.Builder()
                    .url("$baseUrl/api/pages")
                    .header("Authorization", "Bearer $token")
                    .post(body)
                    .build()
            }.mapCatching { body -> parsePage(JSONObject(body).getJSONObject("page")) }
        }

    suspend fun deletePage(pageId: String): Result<Unit> =
        withContext(Dispatchers.IO) {
            executeAuthorized(retryOnUnauthorized = true) { token ->
                Request.Builder()
                    .url("$baseUrl/api/pages/$pageId")
                    .header("Authorization", "Bearer $token")
                    .delete()
                    .build()
            }.map { }
        }

    fun openLibrarySocket(listener: LibraryEventListener): WebSocket? {
        val token = tokenStore.accessToken() ?: return null
        val wsBase =
            when {
                baseUrl.startsWith("https://") -> baseUrl.replaceFirst("https://", "wss://")
                baseUrl.startsWith("http://") -> baseUrl.replaceFirst("http://", "ws://")
                else -> baseUrl
            }
        val request =
            Request.Builder()
                .url("$wsBase/api/realtime")
                .header("Authorization", "Bearer $token")
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
                    listener.onError(t.message ?: "Realtime connection failed")
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

    private suspend fun requestMe(retryOnUnauthorized: Boolean): Result<MeProfile> {
        val accessToken = tokenStore.accessToken()
            ?: return Result.failure(IllegalStateException("Not signed in"))

        val request =
            Request.Builder()
                .url("$baseUrl/api/me")
                .header("Authorization", "Bearer $accessToken")
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
                return Result.failure(
                    IllegalStateException("GET /api/me failed: HTTP ${response.code}"),
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
        val accessToken = tokenStore.accessToken()
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
                return Result.failure(
                    IllegalStateException("API request failed: HTTP ${response.code}"),
                )
            }
            return Result.success(response.body?.string().orEmpty())
        }
    }
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
