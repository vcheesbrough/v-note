package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.net.Endpoints
import link.desync.vnote.net.HttpClients
import link.desync.vnote.net.NonLoopbackHostException
import link.desync.vnote.net.isLoopbackHost
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.Call
import okhttp3.EventListener
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.junit.runner.RunWith
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Proxy
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

// #464: instrumented tests must never reach the real dev deployment. These
// prove the guard [VNoteTestRunner] installs refuses it before connecting, on
// every path the app opens connections through.
@RunWith(AndroidJUnit4::class)
class NetworkGuardInstrumentedTest {
    private val devHost = BuildConfig.BASE_URL.toHttpUrl().host

    // Every network step OkHttp reaches, so a refusal can be shown to come
    // before DNS and before any socket.
    private class Steps : EventListener() {
        val seen: MutableList<String> = Collections.synchronizedList(mutableListOf())

        override fun dnsStart(
            call: Call,
            domainName: String,
        ) {
            seen += "dns"
        }

        override fun connectStart(
            call: Call,
            inetSocketAddress: InetSocketAddress,
            proxy: Proxy,
        ) {
            seen += "connect"
        }
    }

    @Test
    fun theRunnerInstallsTheGuardAndALoopbackServerBeforeTheApplicationStarts() {
        assertSame(TestNetwork.guard, HttpClients.hostGuard)
        assertEquals(TestNetwork.SERVER_BASE_URL, Endpoints.serverBaseUrl)
        assertTrue(isLoopbackHost(Endpoints.serverBaseUrl.toHttpUrl().host))
        assertNotEquals(BuildConfig.BASE_URL, Endpoints.serverBaseUrl)
    }

    @Test
    fun aRequestToTheDevHostIsRefusedBeforeItResolvesOrConnects() {
        val steps = Steps()
        val client = HttpClients.build { eventListener(steps) }
        val violations =
            TestNetwork.collectViolations {
                try {
                    client.newCall(Request.Builder().url("${BuildConfig.BASE_URL}/health").build()).execute().close()
                    fail("a request to $devHost went through")
                } catch (refused: NonLoopbackHostException) {
                    assertEquals(devHost, refused.host)
                }
            }
        assertEquals(listOf(devHost), violations.map { it.host })
        assertTrue("nothing resolved or connected: ${steps.seen}", steps.seen.isEmpty())
    }

    @Test
    fun aWebSocketUpgradeToTheDevHostIsRefused() {
        val failure = AtomicReference<Throwable>()
        val failed = CountDownLatch(1)
        val client = HttpClients.build()
        val socketUrl = BuildConfig.BASE_URL.replaceFirst("http", "ws") + "/api/realtime"
        val violations =
            TestNetwork.collectViolations {
                client.newWebSocket(
                    Request.Builder().url(socketUrl).build(),
                    object : WebSocketListener() {
                        override fun onFailure(
                            webSocket: WebSocket,
                            t: Throwable,
                            response: Response?,
                        ) {
                            failure.set(t)
                            failed.countDown()
                        }
                    },
                )
                assertTrue("the upgrade failed", failed.await(5, TimeUnit.SECONDS))
            }
        assertTrue("refused by the guard: ${failure.get()}", failure.get() is NonLoopbackHostException)
        assertEquals(listOf(devHost), violations.map { it.host })
        client.dispatcher.executorService.shutdown()
    }

    // AppAuth's issuer discovery uses `HttpURLConnection`, not OkHttp.
    @Test
    fun oidcDiscoveryAgainstTheRealIssuerIsRefused() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val authRepository = AuthRepository(context, AuthConfig.fromBuildConfig(), TokenStore(context))
        val issuerHost = BuildConfig.OIDC_ISSUER_URL.toHttpUrl().host
        try {
            val violations =
                TestNetwork.collectViolations {
                    val result = runBlocking { runCatching { authRepository.discoverConfiguration() } }
                    assertTrue("discovery failed", result.isFailure)
                }
            assertEquals(listOf(issuerHost), violations.map { it.host })
        } finally {
            authRepository.shutdown()
        }
    }

    // The other side: the guard does not get in the way of what tests use.
    @Test
    fun aLoopbackMockServerIsReachable() {
        MockWebServer().use { server ->
            server.start(InetAddress.getByName("127.0.0.1"), 0)
            server.enqueue(MockResponse(code = 204))
            val client = HttpClients.build()
            val violations =
                TestNetwork.collectViolations {
                    client.newCall(Request.Builder().url(server.url("/health")).build()).execute().use { response ->
                        assertEquals(204, response.code)
                    }
                }
            assertTrue(violations.isEmpty())
        }
    }
}
