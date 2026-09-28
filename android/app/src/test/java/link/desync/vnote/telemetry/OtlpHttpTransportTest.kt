package link.desync.vnote.telemetry

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class OtlpHttpTransportTest {
    private val now = 1_000_000L

    private fun transport(
        token: () -> String?,
        expiry: Long?,
    ) = OtlpHttpTransport(
        serverBaseUrl = "http://127.0.0.1:9",
        accessToken = token,
        accessTokenExpiry = { expiry },
        nowEpochSeconds = { now },
    )

    private fun transport(
        token: String?,
        expiry: Long?,
    ) = transport({ token }, expiry)

    @Test
    fun readyWithAnUnexpiredToken() {
        assertTrue(transport("token", now + 3600).isReady())
    }

    @Test
    fun readyWhenTheExpiryIsUnknown() {
        assertTrue(transport("token", null).isReady())
    }

    // An export with it would be a certain 401: the batch lost, and a refresh
    // budget spent. Not ready keeps the items queued instead.
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

    // The token is read per request: a refresh by the app's OIDC stack is seen
    // on the next call, with no re-initialisation — which is also how the
    // runtime recognises that a 401 has been answered with a new token.
    @Test
    fun theTokenIsReadPerRequest() {
        var current = "first"
        val transport = transport({ current }, null)
        val before = transport.credentialId()
        current = "second"
        assertNotEquals(before, transport.credentialId())
        current = ""
        assertNull(transport.credentialId())
    }

    // Without a session the config route cannot be asked, and nothing is sent
    // to find out: not yet, rather than no.
    @Test
    fun noTokenMeansNotYet() {
        assertEquals(ConfigFetch.NotYet, transport(null, null).fetchConfig())
    }

    // Port 9 refuses at once: a transport failure is "not answered yet", asked
    // again on a later tick — never the server's "no configuration".
    @Test
    fun anUnreachableServerMeansNotYet() {
        assertEquals(ConfigFetch.NotYet, transport("token", null).fetchConfig())
    }

    @Test
    fun anUnreachableIngestIsRetryable() {
        assertEquals(
            ExportOutcome.Retryable(null),
            transport("token", null).send("http://127.0.0.1:9", Signal.Traces, "{}"),
        )
    }
}
