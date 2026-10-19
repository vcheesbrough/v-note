package link.desync.vnote.net

import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.Call
import okhttp3.EventListener
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

// #464: the pure half of the instrumented-test network guard. The runner that
// installs it, and the app's clients under it, are covered by
// NetworkGuardInstrumentedTest.
class LoopbackOnlyGuardTest {
    private val refused = mutableListOf<NonLoopbackHostException>()
    private val guard = LoopbackOnlyGuard { refused += it }

    @After
    fun tearDown() {
        HttpClients.hostGuard = null
    }

    @Test
    fun loopbackIsLocalhostThe127BlockAndIpv6One() {
        for (host in listOf("localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "[::1]", "0:0:0:0:0:0:0:1")) {
            assertTrue(host, isLoopbackHost(host))
        }
        for (host in listOf(
            "v-notes-dev.desync.link",
            "auth.desync.link",
            "10.0.2.2",
            "128.0.0.1",
            "127.0.0.1.example.com",
            "localhost.example.com",
            "::2",
            "",
        )) {
            assertFalse(host, isLoopbackHost(host))
        }
    }

    @Test
    fun aNonLoopbackHostIsReportedThenRefused() {
        try {
            guard.check("v-notes-dev.desync.link", "https://v-notes-dev.desync.link/health")
            fail("not refused")
        } catch (refusal: NonLoopbackHostException) {
            assertEquals("v-notes-dev.desync.link", refusal.host)
            assertEquals(listOf(refusal), refused)
        }
    }

    @Test
    fun loopbackPassesAndTheMeasurementExemptionPassesEverything() {
        guard.check("127.0.0.1", "http://127.0.0.1:9/api/telemetry/config")
        guard.allowAnyHost = true
        guard.check("v-notes-dev.desync.link", "https://v-notes-dev.desync.link/health")
        assertTrue(refused.isEmpty())
    }

    // Production: no guard, and the builder is a plain OkHttp builder.
    @Test
    fun withoutAGuardTheBuilderAddsNothing() {
        HttpClients.hostGuard = null
        assertTrue(HttpClients.build().interceptors.isEmpty())
        assertNull(HttpClients.appAuthConnectionBuilder())
    }

    @Test
    fun aGuardedClientRefusesBeforeResolvingOrConnecting() {
        HttpClients.hostGuard = guard
        val steps = mutableListOf<String>()
        val client =
            HttpClients.build {
                eventListener(
                    object : EventListener() {
                        override fun dnsStart(
                            call: Call,
                            domainName: String,
                        ) {
                            steps += "dns"
                        }
                    },
                )
            }
        try {
            client.newCall(Request.Builder().url("https://dev.example.invalid/health?token=secret").build()).execute()
            fail("not refused")
        } catch (refusal: NonLoopbackHostException) {
            assertEquals("dev.example.invalid", refusal.host)
            assertFalse("the query is redacted: ${refusal.message}", refusal.message.orEmpty().contains("secret"))
        }
        assertTrue(steps.isEmpty())
    }

    @Test
    fun aGuardedClientStillReachesALoopbackServer() {
        HttpClients.hostGuard = guard
        MockWebServer().use { server ->
            server.start(InetAddress.getByName("127.0.0.1"), 0)
            server.enqueue(MockResponse(code = 204))
            val client = HttpClients.build()
            client.newCall(Request.Builder().url(server.url("/health")).build()).execute().use {
                assertEquals(204, it.code)
            }
        }
        assertTrue(refused.isEmpty())
    }

    @Test
    fun aGuardedWebSocketUpgradeIsRefused() {
        HttpClients.hostGuard = guard
        val failure = AtomicReference<Throwable>()
        val failed = CountDownLatch(1)
        val client = HttpClients.build()
        client.newWebSocket(
            Request.Builder().url("wss://dev.example.invalid/api/realtime").build(),
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
        assertTrue(failed.await(5, TimeUnit.SECONDS))
        assertTrue("${failure.get()}", failure.get() is NonLoopbackHostException)
        client.dispatcher.executorService.shutdown()
    }
}
