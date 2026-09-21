package link.desync.vnote.telemetry

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class OtlpHttpTransportTest {
    private val now = 1_000_000L

    private fun transport(
        token: String?,
        expiry: Long?,
    ) = OtlpHttpTransport(
        baseUrl = "http://127.0.0.1:9",
        accessToken = { token },
        accessTokenExpiry = { expiry },
        nowEpochSeconds = { now },
    )

    @Test
    fun readyWithAnUnexpiredToken() {
        assertTrue(transport("token", now + 3600).isReady())
    }

    @Test
    fun readyWhenTheExpiryIsUnknown() {
        assertTrue(transport("token", null).isReady())
    }

    // An export with it would be a certain 401: the batch lost, and backoff
    // for the exports after it. Not ready keeps the items queued instead.
    @Test
    fun notReadyWithAnExpiredOrNearlyExpiredToken() {
        assertFalse(transport("token", now - 1).isReady())
        assertFalse(transport("token", now + 10).isReady())
    }

    @Test
    fun notReadyWithoutAToken() {
        assertFalse(transport(null, null).isReady())
        assertFalse(transport("", now + 3600).isReady())
    }
}
