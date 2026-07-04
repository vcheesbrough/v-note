package link.desync.vnote.auth

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import net.openid.appauth.AuthorizationException
import net.openid.appauth.AuthorizationRequest
import net.openid.appauth.AuthorizationResponse
import net.openid.appauth.AuthorizationService
import net.openid.appauth.AuthorizationServiceConfiguration
import net.openid.appauth.ResponseTypeValues
import net.openid.appauth.TokenRequest
import okhttp3.FormBody
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import kotlin.coroutines.resume
import kotlin.coroutines.suspendCoroutine

open class AuthRepository(
    private val context: Context,
    private val config: AuthConfig,
    private val tokenStore: TokenStore,
    private val serviceConfigurationOverride: AuthorizationServiceConfiguration? = null,
) {
    private val authService = AuthorizationService(context)
    private val http = OkHttpClient()

    suspend fun discoverConfiguration(): AuthorizationServiceConfiguration =
        serviceConfigurationOverride
            ?: fetchConfiguration()

    private suspend fun fetchConfiguration(): AuthorizationServiceConfiguration =
        suspendCoroutine { continuation ->
            AuthorizationServiceConfiguration.fetchFromIssuer(
                Uri.parse(config.issuerUrl),
            ) { configuration, error ->
                when {
                    configuration != null -> continuation.resume(configuration)
                    else ->
                        continuation.resumeWith(
                            Result.failure(
                                error
                                    ?: IllegalStateException("OIDC discovery failed for ${config.issuerUrl}"),
                            ),
                        )
                }
            }
        }

    suspend fun beginLogin(
        completedIntent: PendingIntent,
        canceledIntent: PendingIntent,
    ) {
        val serviceConfig = discoverConfiguration()
        val request =
            AuthorizationRequest
                .Builder(
                    serviceConfig,
                    config.clientId,
                    ResponseTypeValues.CODE,
                    Uri.parse(config.redirectUri),
                ).setScopes(config.scopes.split(' ').filter { it.isNotBlank() })
                .build()
        authService.performAuthorizationRequest(request, completedIntent, canceledIntent)
    }

    suspend fun handleAuthorizationResponse(data: Intent?): Result<Unit> {
        val intent =
            data ?: return Result.failure(IllegalStateException("Missing authorization intent"))
        val response = AuthorizationResponse.fromIntent(intent)
        val error = AuthorizationException.fromIntent(intent)
        if (error != null) {
            return Result.failure(error)
        }
        if (response == null) {
            return Result.failure(IllegalStateException("Missing authorization response"))
        }

        return exchangeAuthorizationCode(response)
    }

    private suspend fun exchangeAuthorizationCode(response: AuthorizationResponse): Result<Unit> =
        performTokenRequest(response.createTokenExchangeRequest())

    open suspend fun refreshAccessTokenIfNeeded(force: Boolean = false): Boolean {
        val refreshToken = tokenStore.refreshToken() ?: return false
        val expiry = tokenStore.accessTokenExpiryEpochSeconds()
        val now = System.currentTimeMillis() / 1000
        if (!force && expiry != null && expiry > now + 60) {
            return true
        }
        return refreshAccessToken(refreshToken)
    }

    suspend fun refreshAccessToken(refreshToken: String = tokenStore.refreshToken().orEmpty()): Boolean {
        if (refreshToken.isBlank()) {
            return false
        }
        val serviceConfig = discoverConfiguration()
        val request =
            TokenRequest
                .Builder(serviceConfig, config.clientId)
                .setGrantType("refresh_token")
                .setRefreshToken(refreshToken)
                .build()
        return performTokenRequest(request).fold(
            onSuccess = { true },
            onFailure = {
                tokenStore.clear()
                false
            },
        )
    }

    private suspend fun performTokenRequest(tokenRequest: TokenRequest): Result<Unit> =
        withContext(Dispatchers.IO) {
            runCatching {
                val bodyBuilder = FormBody.Builder()
                bodyBuilder.add("client_id", config.clientId)
                for ((key, value) in tokenRequest.requestParameters) {
                    bodyBuilder.add(key, value)
                }
                val request =
                    Request.Builder()
                        .url(tokenRequest.configuration.tokenEndpoint.toString())
                        .header("Accept", "application/json")
                        .post(bodyBuilder.build())
                        .build()
                http.newCall(request).execute().use { response ->
                    val body = response.body?.string().orEmpty()
                    val json = if (body.isBlank()) JSONObject() else JSONObject(body)
                    if (!response.isSuccessful) {
                        val error =
                            json.optString("error_description")
                                .ifBlank { json.optString("error") }
                                .ifBlank { "HTTP ${response.code}" }
                        throw IllegalStateException("Token request failed: $error")
                    }
                    persistTokenResponse(json)
                }
            }
        }

    fun createEndSessionIntent(): Intent? =
        runCatching {
            Intent(Intent.ACTION_VIEW, Uri.parse(config.endSessionUrl))
        }.getOrNull()

    fun signOutLocal() {
        tokenStore.clear()
    }

    fun shutdown() {
        authService.dispose()
    }

    private fun persistTokenResponse(tokenResponse: JSONObject) {
        val expiry =
            if (tokenResponse.has("expires_in")) {
                System.currentTimeMillis() / 1000 + tokenResponse.getLong("expires_in")
            } else {
                null
            }
        val accessToken = tokenResponse.optString("access_token").ifBlank { return }
        val refreshToken = tokenResponse.optString("refresh_token").ifBlank { tokenStore.refreshToken() }
        tokenStore.saveTokens(
            accessToken = accessToken,
            refreshToken = refreshToken,
            accessTokenExpiryEpochSeconds = expiry,
        )
    }
}
