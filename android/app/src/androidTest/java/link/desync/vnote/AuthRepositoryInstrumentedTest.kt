package link.desync.vnote

import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.RefreshOutcome
import link.desync.vnote.auth.TokenStore
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.SocketEffect
import net.openid.appauth.AuthorizationRequest
import net.openid.appauth.AuthorizationResponse
import net.openid.appauth.AuthorizationServiceConfiguration
import net.openid.appauth.ResponseTypeValues
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.net.InetAddress

@RunWith(AndroidJUnit4::class)
class AuthRepositoryInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private lateinit var authRepository: AuthRepository
    private lateinit var serviceConfiguration: AuthorizationServiceConfiguration

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start(InetAddress.getByName("127.0.0.1"), 0)
        serviceConfiguration =
            AuthorizationServiceConfiguration(
                Uri.parse("https://auth.example.test/authorize"),
                Uri.parse(server.url("/token").toString()),
            )

        val context = InstrumentationRegistry.getInstrumentation().targetContext
        tokenStore = TokenStore(context)
        tokenStore.clear()
        authRepository =
            AuthRepository(
                context = context,
                config =
                    AuthConfig(
                        issuerUrl = "https://auth.example.test/application/o/v-note-dev/",
                        clientId = "v-note-dev",
                        redirectUri = "https://v-notes-dev.desync.link/auth/mobile/callback",
                        endSessionUrl = "https://auth.example.test/end-session/",
                        scopes = "openid profile email offline_access v-note:dev:access",
                    ),
                tokenStore = tokenStore,
                serviceConfigurationOverride = serviceConfiguration,
            )
    }

    @After
    fun tearDown() {
        authRepository.shutdown()
        tokenStore.clear()
        server.close()
    }

    @Test
    fun authorizationCallbackExchangesCodeEvenWhenIdTokenFailsAppAuthValidation() {
        server.enqueue(
            jsonResponse(
                """
                {
                  "access_token": "access-token",
                  "refresh_token": "refresh-token",
                  "expires_in": 3600,
                  "token_type": "Bearer",
                  "id_token": "not.a.jwt"
                }
                """.trimIndent(),
            ),
        )

        val result =
            runBlocking {
                authRepository.handleAuthorizationResponse(authorizationResponseIntent("auth-code"))
            }

        assertTrue(result.isSuccess)
        assertEquals("access-token", tokenStore.accessToken())
        assertEquals("refresh-token", tokenStore.refreshToken())

        val request = server.takeRequest()
        assertEquals("/token", request.target)
        val body = request.body?.utf8().orEmpty()
        assertTrue(body.contains("client_id=v-note-dev"))
        assertTrue(body.contains("grant_type=authorization_code"))
        assertTrue(body.contains("code=auth-code"))
        assertTrue(body.contains("redirect_uri=https%3A%2F%2Fv-notes-dev.desync.link%2Fauth%2Fmobile%2Fcallback"))
        // PKCE: the unified client is public (#274), so the verifier — not a
        // secret — is what binds this code to this app.
        assertTrue(body.contains("code_verifier="))
        assertTrue("a public client must never send a secret", !body.contains("client_secret"))
    }

    @Test
    fun refreshTokenUsesDirectTokenExchangeAndStoresRotatedRefreshToken() {
        tokenStore.saveTokens(
            accessToken = "stale-access",
            refreshToken = "old-refresh",
            accessTokenExpiryEpochSeconds = 1L,
        )
        server.enqueue(
            jsonResponse(
                """
                {
                  "access_token": "fresh-access",
                  "refresh_token": "new-refresh",
                  "expires_in": 3600,
                  "token_type": "Bearer",
                  "id_token": "not.a.jwt"
                }
                """.trimIndent(),
            ),
        )

        val refreshed = runBlocking { authRepository.refreshAccessTokenIfNeeded(force = true) }

        assertTrue(refreshed)
        assertEquals("fresh-access", tokenStore.accessToken())
        assertEquals("new-refresh", tokenStore.refreshToken())

        val body =
            server
                .takeRequest()
                .body
                ?.utf8()
                .orEmpty()
        assertTrue(body.contains("client_id=v-note-dev"))
        assertTrue(body.contains("grant_type=refresh_token"))
        assertTrue(body.contains("refresh_token=old-refresh"))
    }

    /** The provider answered and refused the refresh token: the session is over. */
    @Test
    fun aRefusedRefreshSignsOut() {
        tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "revoked", accessTokenExpiryEpochSeconds = 1L)
        server.enqueue(
            MockResponse
                .Builder()
                .code(400)
                .body("""{"error":"invalid_grant"}""")
                .addHeader("Content-Type", "application/json")
                .build(),
        )

        val refreshed = runBlocking { authRepository.refreshAccessTokenIfNeeded() }

        assertEquals(false, refreshed)
        assertNull("refused: tokens cleared", tokenStore.refreshToken())
    }

    /**
     * #159: only an OAuth `invalid_grant` ends the session. Everything else a
     * token endpoint can answer while the network or the provider is in
     * trouble — Traefik's 502/503 while Authentik restarts, a captive portal's
     * HTML (with an error status or a 200) — keeps the tokens, and the
     * refresh reports the provider unavailable rather than refused.
     */
    @Test
    fun noAnswerButInvalidGrantEndsTheSession() {
        val answers =
            mapOf(
                "502" to
                    MockResponse
                        .Builder()
                        .code(502)
                        .body("Bad Gateway")
                        .build(),
                "503" to
                    MockResponse
                        .Builder()
                        .code(503)
                        .body("Service Unavailable")
                        .build(),
                "captive portal, error status" to html(511),
                "captive portal, 200" to html(200),
                "another OAuth error" to
                    MockResponse
                        .Builder()
                        .code(400)
                        .body("""{"error":"temporarily_unavailable"}""")
                        .addHeader("Content-Type", "application/json")
                        .build(),
            )
        for ((what, answer) in answers) {
            tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "kept", accessTokenExpiryEpochSeconds = 1L)
            server.enqueue(answer)

            val outcome = runBlocking { authRepository.refresh() }

            assertEquals("$what: the provider is unavailable, not refusing", RefreshOutcome.Unavailable, outcome)
            assertEquals("$what: refresh token kept", "kept", tokenStore.refreshToken())
            assertEquals("$what: access token kept", "stale-access", tokenStore.accessToken())
        }
    }

    /**
     * #159: a refresh response lost on the way back (the connection drops
     * after the provider took the request) is retried once with the same
     * refresh token, which the provider only replaces in its last week.
     */
    @Test
    fun aLostRefreshResponseIsRetriedOnceWithTheSameToken() {
        tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "same-refresh", accessTokenExpiryEpochSeconds = 1L)
        server.enqueue(MockResponse.Builder().onResponseStart(SocketEffect.ShutdownConnection).build())
        server.enqueue(
            jsonResponse(
                """{"access_token":"fresh-access","token_type":"Bearer","expires_in":3600}""",
            ),
        )

        val outcome = runBlocking { authRepository.refresh() }

        assertEquals(RefreshOutcome.Refreshed, outcome)
        assertEquals("fresh-access", tokenStore.accessToken())
        assertEquals("the refresh token is kept when not replaced", "same-refresh", tokenStore.refreshToken())
        val first =
            server
                .takeRequest()
                .body
                ?.utf8()
                .orEmpty()
        val second =
            server
                .takeRequest()
                .body
                ?.utf8()
                .orEmpty()
        assertTrue("first try", first.contains("refresh_token=same-refresh"))
        assertTrue("retried with the same token", second.contains("refresh_token=same-refresh"))
    }

    /** The refused outcome is the one that clears the tokens. */
    @Test
    fun invalidGrantIsTheRefusedOutcome() {
        tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "revoked", accessTokenExpiryEpochSeconds = 1L)
        server.enqueue(
            MockResponse
                .Builder()
                .code(400)
                .body("""{"error":"invalid_grant","error_description":"Token is not valid"}""")
                .addHeader("Content-Type", "application/json")
                .build(),
        )

        assertEquals(RefreshOutcome.Refused, runBlocking { authRepository.refresh() })
        assertNull(tokenStore.accessToken())
    }

    /**
     * #150: offline (airplane mode) the token endpoint cannot be reached. That
     * is not a refusal — the session must survive until the network is back.
     */
    @Test
    fun anUnreachableTokenEndpointKeepsTheSession() {
        tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "kept", accessTokenExpiryEpochSeconds = 1L)
        val offline =
            repository(
                serviceConfigurationOverride =
                    AuthorizationServiceConfiguration(
                        Uri.parse("https://auth.example.test/authorize"),
                        Uri.parse("${TestNetwork.SERVER_BASE_URL}/token"),
                    ),
            )
        try {
            val refreshed = runBlocking { offline.refreshAccessTokenIfNeeded() }

            assertEquals(false, refreshed)
            assertEquals("kept", tokenStore.refreshToken())
            assertEquals("stale-access", tokenStore.accessToken())
        } finally {
            offline.shutdown()
        }
    }

    /**
     * #150: OIDC discovery fails by throwing when the provider is unreachable.
     * The refresh must report failure and keep the session, not throw: its
     * caller at startup is a bare coroutine, where a throw crashed the app.
     */
    @Test
    fun anUnreachableDiscoveryKeepsTheSessionWithoutThrowing() {
        tokenStore.saveTokens(accessToken = "stale-access", refreshToken = "kept", accessTokenExpiryEpochSeconds = 1L)
        // https, as on a device: AppAuth refuses an http issuer outright, before
        // any network, which is not what an offline device sees.
        val offline = repository(issuerUrl = "https://127.0.0.1:9/application/o/v-note-dev/")
        try {
            val refreshed = runBlocking { offline.refreshAccessTokenIfNeeded() }

            assertEquals(false, refreshed)
            assertEquals("kept", tokenStore.refreshToken())
        } finally {
            offline.shutdown()
        }
    }

    private fun repository(
        issuerUrl: String = "https://auth.example.test/application/o/v-note-dev/",
        serviceConfigurationOverride: AuthorizationServiceConfiguration? = null,
    ): AuthRepository =
        AuthRepository(
            context = InstrumentationRegistry.getInstrumentation().targetContext,
            config =
                AuthConfig(
                    issuerUrl = issuerUrl,
                    clientId = "v-note-dev",
                    redirectUri = "https://v-notes-dev.desync.link/auth/mobile/callback",
                    endSessionUrl = "https://auth.example.test/end-session/",
                    scopes = "openid profile email offline_access v-note:dev:access",
                ),
            tokenStore = tokenStore,
            serviceConfigurationOverride = serviceConfigurationOverride,
        )

    private fun authorizationResponseIntent(code: String): android.content.Intent {
        val request =
            AuthorizationRequest
                .Builder(
                    serviceConfiguration,
                    "v-note-dev",
                    ResponseTypeValues.CODE,
                    Uri.parse("https://v-notes-dev.desync.link/auth/mobile/callback"),
                ).setScopes("openid", "profile", "email", "offline_access", "v-note:dev:access")
                .build()
        return AuthorizationResponse
            .Builder(request)
            .setState(request.state)
            .setAuthorizationCode(code)
            .build()
            .toIntent()
    }

    private fun html(code: Int): MockResponse =
        MockResponse
            .Builder()
            .code(code)
            .body("<html><body>Sign in to the hotel Wi-Fi</body></html>")
            .addHeader("Content-Type", "text/html")
            .build()

    private fun jsonResponse(body: String): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body(body)
            .addHeader("Content-Type", "application/json")
            .build()
}
