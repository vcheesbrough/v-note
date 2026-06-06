package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.auth.TokenStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class AuthTokenStoreInstrumentedTest {
    private lateinit var tokenStore: TokenStore

    @Before
    fun setUp() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        tokenStore = TokenStore(context)
        tokenStore.clear()
    }

    @Test
    fun storesAndClearsTokens() {
        tokenStore.saveTokens(
            accessToken = "access-1",
            refreshToken = "refresh-1",
            accessTokenExpiryEpochSeconds = 4_102_444_800L,
        )

        assertTrue(tokenStore.hasSession())
        assertEquals("access-1", tokenStore.accessToken())
        assertEquals("refresh-1", tokenStore.refreshToken())
        assertEquals(4_102_444_800L, tokenStore.accessTokenExpiryEpochSeconds())

        tokenStore.clear()

        assertFalse(tokenStore.hasSession())
        assertNull(tokenStore.accessToken())
        assertNull(tokenStore.refreshToken())
        assertNull(tokenStore.accessTokenExpiryEpochSeconds())
    }
}
