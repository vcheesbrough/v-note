package link.desync.vnote.api

import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.Request
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test

/**
 * #651: the API client follows no redirect. v-note's API never redirects, so
 * a 3xx is a captive portal or a proxy answering in its place: it is a failed
 * request like any other status, and the request — with its bearer token —
 * must not go on to wherever the redirect points.
 */
class ApiHttpClientTest {
    private val server = MockWebServer()

    @Before
    fun start() = server.start()

    @After
    fun stop() = server.close()

    @Test
    fun aRedirectIsReturnedAsItIsAndNeverFollowed() {
        for (status in listOf(301, 302, 303, 307, 308)) {
            server.enqueue(
                MockResponse
                    .Builder()
                    .code(status)
                    .addHeader("Location", server.url("/portal/login").toString())
                    .build(),
            )
        }
        val client = apiHttpClient()

        for (status in listOf(301, 302, 303, 307, 308)) {
            val request =
                Request
                    .Builder()
                    .url(server.url("/api/pages"))
                    .header("Authorization", "Bearer secret")
                    .build()
            client.newCall(request).execute().use { response ->
                assertEquals("the $status is the answer", status, response.code)
            }
        }

        assertEquals("nothing was sent on to the redirect's target", 5, server.requestCount)
        repeat(5) { assertEquals("/api/pages", server.takeRequest().url.encodedPath) }
    }
}
