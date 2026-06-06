package link.desync.vnote.auth

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject

class ApiClient(
    private val baseUrl: String,
    private val tokenStore: TokenStore,
    private val authRepository: AuthRepository,
) {
    private val http = OkHttpClient()

    suspend fun fetchMe(): Result<MeProfile> =
        withContext(Dispatchers.IO) {
            requestMe(retryOnUnauthorized = true)
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
}

data class MeProfile(
    val sub: String,
    val email: String?,
)
