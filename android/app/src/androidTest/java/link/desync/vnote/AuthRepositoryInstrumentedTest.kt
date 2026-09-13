package link.desync.vnote

import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import net.openid.appauth.AuthorizationRequest
import net.openid.appauth.AuthorizationResponse
import net.openid.appauth.AuthorizationServiceConfiguration
import net.openid.appauth.ResponseTypeValues
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
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
                        issuerUrl = "https://auth.example.test/application/o/v-note-android-dev/",
                        clientId = "v-note-android-dev",
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
        server.shutdown()
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
        assertEquals("/token", request.path)
        val body = request.body.readUtf8()
        assertTrue(body.contains("client_id=v-note-android-dev"))
        assertTrue(body.contains("grant_type=authorization_code"))
        assertTrue(body.contains("code=auth-code"))
        assertTrue(body.contains("redirect_uri=https%3A%2F%2Fv-notes-dev.desync.link%2Fauth%2Fmobile%2Fcallback"))
        assertTrue(body.contains("code_verifier="))
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

        val body = server.takeRequest().body.readUtf8()
        assertTrue(body.contains("client_id=v-note-android-dev"))
        assertTrue(body.contains("grant_type=refresh_token"))
        assertTrue(body.contains("refresh_token=old-refresh"))
    }

    private fun authorizationResponseIntent(code: String): android.content.Intent {
        val request =
            AuthorizationRequest
                .Builder(
                    serviceConfiguration,
                    "v-note-android-dev",
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

    private fun jsonResponse(body: String): MockResponse =
        MockResponse()
            .setResponseCode(200)
            .setBody(body)
            .addHeader("Content-Type", "application/json")
}
