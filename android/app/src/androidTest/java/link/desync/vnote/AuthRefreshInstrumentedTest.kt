package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import kotlinx.coroutines.runBlocking
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class AuthRefreshInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start()
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
        server.shutdown()
    }

    @Test
    fun retriesApiMeAfter401WhenRefreshSucceeds() {
        server.enqueue(MockResponse().setResponseCode(401))
        server.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"sub":"user-1","email":"user@example.com"}""")
                .addHeader("Content-Type", "application/json"),
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

        val apiClient = ApiClient(server.url("/").toString().removeSuffix("/"), tokenStore, authRepository)

        runBlocking {
            val result = apiClient.fetchMe()
            assertTrue(result.isSuccess)
            assertEquals("user-1", result.getOrNull()?.sub)
        }

        val first = server.takeRequest()
        assertEquals("Bearer stale-access", first.getHeader("Authorization"))
        val second = server.takeRequest()
        assertEquals("Bearer fresh-access", second.getHeader("Authorization"))
    }
}
