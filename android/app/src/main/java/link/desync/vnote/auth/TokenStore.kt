package link.desync.vnote.auth

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

class TokenStore(
    context: Context,
) {
    private val prefs =
        EncryptedSharedPreferences.create(
            context,
            PREFS_NAME,
            MasterKey.Builder(context).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build(),
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )

    fun saveTokens(
        accessToken: String,
        refreshToken: String?,
        accessTokenExpiryEpochSeconds: Long?,
    ) {
        prefs
            .edit()
            .putString(KEY_ACCESS_TOKEN, accessToken)
            .putString(KEY_REFRESH_TOKEN, refreshToken)
            .putLong(
                KEY_ACCESS_EXPIRY,
                accessTokenExpiryEpochSeconds ?: 0L,
            ).apply()
    }

    fun accessToken(): String? = prefs.getString(KEY_ACCESS_TOKEN, null)

    fun refreshToken(): String? = prefs.getString(KEY_REFRESH_TOKEN, null)

    fun accessTokenExpiryEpochSeconds(): Long? {
        val value = prefs.getLong(KEY_ACCESS_EXPIRY, 0L)
        return if (value > 0L) value else null
    }

    fun clear() {
        prefs.edit().clear().apply()
    }

    fun hasSession(): Boolean = !accessToken().isNullOrBlank()

    companion object {
        private const val PREFS_NAME = "vnote_auth_tokens"
        private const val KEY_ACCESS_TOKEN = "access_token"
        private const val KEY_REFRESH_TOKEN = "refresh_token"
        private const val KEY_ACCESS_EXPIRY = "access_token_expiry"
    }
}
