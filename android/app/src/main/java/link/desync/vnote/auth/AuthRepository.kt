package link.desync.vnote.auth

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import net.openid.appauth.AuthorizationException
import net.openid.appauth.AuthorizationRequest
import net.openid.appauth.AuthorizationResponse
import net.openid.appauth.AuthorizationService
import net.openid.appauth.AuthorizationServiceConfiguration
import net.openid.appauth.ResponseTypeValues
import net.openid.appauth.TokenRequest
import net.openid.appauth.TokenResponse
import kotlin.coroutines.resume
import kotlin.coroutines.suspendCoroutine

open class AuthRepository(
    private val context: Context,
    private val config: AuthConfig,
    private val tokenStore: TokenStore,
) {
    private val authService = AuthorizationService(context)

    suspend fun discoverConfiguration(): AuthorizationServiceConfiguration =
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

    suspend fun handleAuthorizationResponse(
        data: Intent?,
    ): Result<Unit> {
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

    private suspend fun exchangeAuthorizationCode(
        response: AuthorizationResponse,
    ): Result<Unit> =
        suspendCoroutine { continuation ->
            val tokenRequest = response.createTokenExchangeRequest()
            authService.performTokenRequest(tokenRequest) { tokenResponse, tokenError ->
                when {
                    tokenResponse != null -> {
                        persistTokenResponse(tokenResponse)
                        continuation.resume(Result.success(Unit))
                    }
                    else ->
                        continuation.resume(
                            Result.failure(
                                tokenError
                                    ?: IllegalStateException("Token exchange failed"),
                            ),
                        )
                }
            }
        }

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
        return suspendCoroutine { continuation ->
            authService.performTokenRequest(request) { tokenResponse, tokenError ->
                when {
                    tokenResponse?.accessToken != null -> {
                        persistTokenResponse(tokenResponse)
                        continuation.resume(true)
                    }
                    else -> {
                        if (tokenError != null) {
                            tokenStore.clear()
                        }
                        continuation.resume(false)
                    }
                }
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

    private fun persistTokenResponse(tokenResponse: TokenResponse) {
        val expiry =
            tokenResponse.accessTokenExpirationTime?.let { millis ->
                millis / 1000
            }
        tokenStore.saveTokens(
            accessToken = tokenResponse.accessToken ?: return,
            refreshToken = tokenResponse.refreshToken ?: tokenStore.refreshToken(),
            accessTokenExpiryEpochSeconds = expiry,
        )
    }
}
