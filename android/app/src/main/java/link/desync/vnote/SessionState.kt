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
