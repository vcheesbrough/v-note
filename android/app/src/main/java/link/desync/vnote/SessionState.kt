package link.desync.vnote

import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import link.desync.vnote.model.MeProfile

// Where the app is in the sign-in flow; drives which screen MainActivity shows.

internal sealed interface SessionState {
    data object Loading : SessionState

    data object SignedOut : SessionState

    data class SignedIn(
        val profile: MeProfile,
    ) : SessionState

    data class Error(
        val message: String,
    ) : SessionState
}

// A sign-in as [profile] waiting on the user: [unsynced] changes from the
// identity the device holds would be deleted by switching (#159).
internal data class IdentitySwitch(
    val profile: MeProfile,
    val unsynced: Int,
    // The tokens from before the sign-in, restored if the user cancels; null
    // when there were none to restore.
    val previousTokens: link.desync.vnote.auth.TokenStore.Snapshot?,
)
