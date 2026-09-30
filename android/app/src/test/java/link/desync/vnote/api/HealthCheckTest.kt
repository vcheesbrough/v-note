package link.desync.vnote.api

import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.SocketEffect
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import java.io.IOException
import kotlin.coroutines.cancellation.CancellationException

/**
 * [OkHttpApiClient.checkHealth]'s request (#186): every failure — an HTTP
 * error, a dropped or refused connection, or a non-IO exception from the
 * client — is a failed [Result] the probe can report, never a throw that
 * would crash the screen (PR #65 review). Only coroutine cancellation passes
 * through.
 */
class HealthCheckTest {
    private val server = MockWebServer()
    private val failures = mutableListOf<String>()

    @Before
    fun setUp() = server.start()

    @After
    fun tearDown() = server.close()

    private fun healthCheck(
        http: OkHttpClient = OkHttpClient(),
        baseUrl: String = server.url("/").toString().removeSuffix("/"),
    ) = HealthCheck(http, baseUrl, onFailure = { reason, _ -> failures += reason })

    @Test
    fun aSuccessfulResponseIsHealthy() {
        server.enqueue(MockResponse(code = 200, body = """{"status":"ok"}"""))

        assertTrue(healthCheck().check().isSuccess)
        assertEquals("/health", server.takeRequest().url.encodedPath)
        assertEquals(emptyList<String>(), failures)
    }

    @Test
    fun anHttpErrorFailsWithItsStatus() {
        server.enqueue(MockResponse(code = 502))

        val result = healthCheck().check()

        assertEquals("HTTP 502", result.exceptionOrNull()?.message)
        assertEquals(listOf("HTTP 502"), failures)
    }

    @Test
    fun aDroppedConnectionFailsInsteadOfThrowing() {
        server.enqueue(MockResponse.Builder().onRequestStart(SocketEffect.CloseSocket()).build())

        val result = healthCheck().check()

        assertTrue(result.exceptionOrNull() is IOException)
        assertEquals(1, failures.size)
    }

    @Test
    fun aRefusedConnectionFailsInsteadOfThrowing() {
        val baseUrl = server.url("/").toString().removeSuffix("/")
        server.close()

        val result = healthCheck(baseUrl = baseUrl).check()

        assertTrue(result.exceptionOrNull() is IOException)
        assertEquals(1, failures.size)
    }

    @Test
    fun aNonIoExceptionFailsInsteadOfThrowing() {
        val http =
            OkHttpClient
                .Builder()
                .addInterceptor { throw IllegalArgumentException("interceptor bug") }
                .build()

        val result = healthCheck(http = http).check()

        assertEquals("interceptor bug", result.exceptionOrNull()?.message)
        assertEquals(listOf("IllegalArgumentException"), failures)
    }

    @Test
    fun cancellationIsRethrownNotSwallowed() {
        val http =
            OkHttpClient
                .Builder()
                .addInterceptor { throw CancellationException("screen left") }
                .build()

        try {
            healthCheck(http = http).check()
            fail("cancellation was swallowed")
        } catch (expected: CancellationException) {
            assertEquals("screen left", expected.message)
        }
    }
}
