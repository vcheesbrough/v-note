package link.desync.vnote.sync

import link.desync.vnote.model.MeProfile
import link.desync.vnote.store.LocalStore

// What a sign-in as a given identity does to this install (#159: one
// identity per install).
sealed interface IdentityDecision {
    // The first identity, or the same one again: everything is kept.
    data object Continue : IdentityDecision

    // A different identity, and nothing is owed to the server: the install
    // starts over without asking.
    data object Reset : IdentityDecision

    // A different identity, with [unsynced] changes from the one the device
    // holds that starting over would delete: the user decides.
    data class Warn(
        val unsynced: Int,
    ) : IdentityDecision
}

fun decideIdentity(
    store: LocalStore,
    profile: MeProfile,
): IdentityDecision {
    val stored = store.identity.current()
    if (stored == null || stored.sub == profile.sub) return IdentityDecision.Continue
    val unsynced = store.outbox.read.unsyncedCount()
    return if (unsynced == 0) IdentityDecision.Reset else IdentityDecision.Warn(unsynced)
}

// Starts the install over for [profile]: every local record of the previous
// identity goes. Tokens are the caller's: they already belong to [profile].
fun resetInstallFor(
    store: LocalStore,
    profile: MeProfile,
) {
    store.transaction {
        store.resetAll()
        store.identity.save(profile)
    }
}
