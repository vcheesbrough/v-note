package link.desync.vnote.auth

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import link.desync.vnote.net.HttpClients
import link.desync.vnote.telemetry.AppLog
import link.desync.vnote.telemetry.Telemetry
import net.openid.appauth.AppAuthConfiguration
import net.openid.appauth.AuthorizationException
import net.openid.appauth.AuthorizationRequest
import net.openid.appauth.AuthorizationResponse
import net.openid.appauth.AuthorizationService
import net.openid.appauth.AuthorizationServiceConfiguration
import net.openid.appauth.ResponseTypeValues
import net.openid.appauth.TokenRequest
import okhttp3.FormBody
import okhttp3.Request
import org.json.JSONObject
import java.io.IOException
import kotlin.coroutines.resume
import kotlin.coroutines.suspendCoroutine

open class AuthRepository(
    private val context: Context,
    private val config: AuthConfig,
    private val tokenStore: TokenStore,
    private val serviceConfigurationOverride: AuthorizationServiceConfiguration? = null,
) {
    // Both through [HttpClients] (#464), so the instrumentation runner's
    // network guard covers the token endpoint and AppAuth's discovery fetch.
    // In the app there is no guard: a default `AuthorizationService` and a
    // plain client, as before.
    private val appAuthConnections = HttpClients.appAuthConnectionBuilder()
    private val authService =
        appAuthConnections?.let { connections ->
            AuthorizationService(context, AppAuthConfiguration.Builder().setConnectionBuilder(connections).build())
        } ?: AuthorizationService(context)
    private val http = HttpClients.build()

    // The identity provider's host, which a network may fail to reach while
    // the backend answers (#159): named in the sync diagnostics.
    val providerHost: String get() =
        link.desync.vnote.api
            .hostOf(config.issuerUrl)

    suspend fun discoverConfiguration(): AuthorizationServiceConfiguration =
        serviceConfigurationOverride
            ?: fetchConfiguration()

    private suspend fun fetchConfiguration(): AuthorizationServiceConfiguration =
        suspendCoroutine { continuation ->
            val callback =
                AuthorizationServiceConfiguration.RetrieveConfigurationCallback { configuration, error ->
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
            val issuer = Uri.parse(config.issuerUrl)
            val connections = appAuthConnections
            if (connections != null) {
                AuthorizationServiceConfiguration.fetchFromIssuer(issuer, callback, connections)
            } else {
                AuthorizationServiceConfiguration.fetchFromIssuer(issuer, callback)
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
        performTokenRequest(response.createTokenExchangeRequest(), "auth.sign_in")

    // True when a usable access token is now stored. Refreshes only when the
    // token has expired (or is about to) or [force] says the server refused
    // it — lazily, when a sync request needs one, never ahead of time (#159).
    open suspend fun refreshAccessTokenIfNeeded(force: Boolean = false): Boolean {
        if (tokenStore.refreshToken() == null) {
            lastRefreshOutcome = RefreshOutcome.Refused
            return false
        }
        val expiry = tokenStore.accessTokenExpiryEpochSeconds()
        val now = System.currentTimeMillis() / 1000
        if (!force && expiry != null && expiry > now + 60) {
            return true
        }
        return refresh() == RefreshOutcome.Refreshed
    }

    suspend fun refreshAccessToken(refreshToken: String = tokenStore.refreshToken().orEmpty()): Boolean =
        refresh(refreshToken) == RefreshOutcome.Refreshed

    // What the last refresh came to, for a caller that got `false` from
    // [refreshAccessTokenIfNeeded] and must tell a refused session from an
    // unreachable provider. A subclass that fakes the Boolean leaves it
    // [RefreshOutcome.Refused], which is what `false` meant before #159.
    @Volatile
    var lastRefreshOutcome: RefreshOutcome = RefreshOutcome.Refused
        private set

    // One refresh, and what came of it (#159). Network failures never end the
    // session: the backend and the identity provider are separate hosts, and
    // a network that reaches neither, or a provider restarting behind a 502,
    // or a captive portal answering HTML, is not a refused session. Only an
    // OAuth `invalid_grant` from the provider clears the tokens.
    suspend fun refresh(refreshToken: String = tokenStore.refreshToken().orEmpty()): RefreshOutcome =
        refreshOnce(refreshToken).also { lastRefreshOutcome = it }

    private suspend fun refreshOnce(refreshToken: String): RefreshOutcome {
        if (refreshToken.isBlank()) {
            return RefreshOutcome.Refused
        }
        // Discovery fails by throwing when the provider cannot be reached at
        // all (#150): that is the provider being unavailable, nothing more.
        val serviceConfig =
            try {
                discoverConfiguration()
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (
                @Suppress("TooGenericExceptionCaught") unreachable: Exception,
            ) {
                AppLog.w(TAG, "token refresh skipped: identity provider unreachable", unreachable)
                return RefreshOutcome.Unavailable
            }
        val request =
            TokenRequest
                .Builder(serviceConfig, config.clientId)
                .setGrantType("refresh_token")
                .setRefreshToken(refreshToken)
                .build()
        var result = performTokenRequest(request, "auth.token_refresh")
        // A response lost on the way back (a dropped connection, a timeout)
        // may still have been acted on. Retry once with the same refresh
        // token: the provider only replaces it in its last week (#159's
        // `refresh_token_threshold`), so the old one almost always still works.
        if (result.exceptionOrNull() is IOException) {
            AppLog.i(TAG, "token refresh response lost; retrying once with the same refresh token")
            result = performTokenRequest(request, "auth.token_refresh")
        }
        return result.fold(
            onSuccess = { RefreshOutcome.Refreshed },
            onFailure = { failure ->
                if (failure is TokenRefusedException) {
                    AppLog.w(TAG, "refresh token refused; the session has ended")
                    tokenStore.clear()
                    RefreshOutcome.Refused
                } else {
                    RefreshOutcome.Unavailable
                }
            },
        )
    }

    // One token-endpoint round trip, as a span named [spanName] (#406). The
    // request itself carries no `traceparent`: it goes to the identity
    // provider, which is not ours to put in our traces.
    private suspend fun performTokenRequest(
        tokenRequest: TokenRequest,
        spanName: String,
    ): Result<Unit> =
        withContext(Dispatchers.IO) {
            val span = Telemetry.span(spanName)
            runCatching {
                val bodyBuilder = FormBody.Builder()
                bodyBuilder.add("client_id", config.clientId)
                for ((key, value) in tokenRequest.requestParameters) {
                    bodyBuilder.add(key, value)
                }
                val request =
                    Request
                        .Builder()
                        .url(tokenRequest.configuration.tokenEndpoint.toString())
                        .header("Accept", "application/json")
                        .post(bodyBuilder.build())
                        .build()
                http.newCall(request).execute().use { response ->
                    val body = response.body.string()
                    // A captive portal answers with HTML, often with 200:
                    // anything that is not the provider's JSON is no answer.
                    val json = runCatching { JSONObject(body) }.getOrNull()
                    if (!response.isSuccessful) {
                        val error = json?.optString("error").orEmpty()
                        val description =
                            json?.optString("error_description")?.ifBlank { null }
                                ?: error.ifBlank { "HTTP ${response.code}" }
                        if (error == INVALID_GRANT) throw TokenRefusedException(description)
                        throw TokenEndpointException("Token request failed: $description")
                    }
                    if (json == null || json.optString("access_token").isBlank()) {
                        throw TokenEndpointException("Token request failed: HTTP ${response.code} without a token")
                    }
                    persistTokenResponse(json)
                }
            }.onSuccess {
                span.end()
            }.onFailure { error ->
                span.fail(error.javaClass.simpleName)
                // The message is the provider's `error_description`, or ours —
                // never a token: those are only ever in the request and response
                // bodies, which are not logged.
                AppLog.w(TAG, "$spanName failed", error, context = span.context)
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

    private companion object {
        const val TAG = "VNoteAuth"
        const val INVALID_GRANT = "invalid_grant"
    }
}

// What a refresh came to (#159). Only [Refused] means "sign in again".
enum class RefreshOutcome {
    Refreshed,

    // The provider answered `invalid_grant`, or there is no refresh token.
    Refused,

    // The provider could not be reached or gave no usable answer. The
    // session stands; try again later.
    Unavailable,
}

// The provider refused the grant: the one failure that ends a session.
class TokenRefusedException(
    message: String,
) : IllegalStateException(message)

// The provider was reached but gave no usable answer (5xx, HTML, no token).
class TokenEndpointException(
    message: String,
) : IllegalStateException(message)
