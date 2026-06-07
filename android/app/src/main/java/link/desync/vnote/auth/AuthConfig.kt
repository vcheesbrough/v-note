package link.desync.vnote.auth

import link.desync.vnote.BuildConfig

data class AuthConfig(
    val issuerUrl: String,
    val clientId: String,
    val redirectUri: String,
    val endSessionUrl: String,
    val scopes: String,
) {
    companion object {
        fun fromBuildConfig(): AuthConfig =
            AuthConfig(
                issuerUrl = BuildConfig.OIDC_ISSUER_URL,
                clientId = BuildConfig.OIDC_CLIENT_ID,
                redirectUri = BuildConfig.OIDC_REDIRECT_URI,
                endSessionUrl = BuildConfig.OIDC_END_SESSION_URL,
                scopes = BuildConfig.OIDC_SCOPES,
            )
    }
}
