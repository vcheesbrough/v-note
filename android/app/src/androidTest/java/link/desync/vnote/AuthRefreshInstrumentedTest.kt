package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.net.InetAddress

@RunWith(AndroidJUnit4::class)
class AuthRefreshInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: ApiClient? = null

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start(InetAddress.getByName("127.0.0.1"), 0)
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        tokenStore = TokenStore(context)
        tokenStore.clear()
        tokenStore.saveTokens(
            accessToken = "stale-access",
            refreshToken = "refresh-token",
            accessTokenExpiryEpochSeconds = 1L,
        )
    }

    @After
    fun tearDown() {
        apiClient?.shutdown()
        server.close()
    }

    @Test
    fun retriesApiMeAfter401WhenRefreshSucceeds() {
        server.enqueue(MockResponse(code = 401))
        server.enqueue(
            MockResponse
                .Builder()
                .code(200)
                .body("""{"sub":"user-1","email":"user@example.com"}""")
                .addHeader("Content-Type", "application/json")
                .build(),
        )

        val authRepository =
            object : AuthRepository(
                InstrumentationRegistry.getInstrumentation().targetContext,
                AuthConfig.fromBuildConfig(),
                tokenStore,
            ) {
                override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean {
                    tokenStore.saveTokens(
                        accessToken = "fresh-access",
                        refreshToken = "refresh-token",
                        accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                    )
                    return true
                }
            }

        val client = OkHttpApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, authRepository)
        apiClient = client

        runBlocking {
            val result = client.fetchMe()
            assertTrue(result.isSuccess)
            assertEquals("user-1", result.getOrNull()?.sub)
        }

        val first = server.takeRequest()
        assertEquals("Bearer stale-access", first.headers["Authorization"])
        val second = server.takeRequest()
        assertEquals("Bearer fresh-access", second.headers["Authorization"])
    }
}
