package link.desync.vnote.store

import android.content.ContentValues
import link.desync.vnote.model.MeProfile

// The install's one identity (#159): the `sub` every local record belongs to,
// so the app opens signed in from the device without asking the server. Also
// whether the user explicitly signed out (the only time, besides first use,
// the sign-in screen is shown), and when sync last succeeded.
class IdentityStore internal constructor(
    private val store: LocalStore,
) {
    fun current(): MeProfile? {
        val sub = read(KEY_SUB) ?: return null
        return MeProfile(sub = sub, email = read(KEY_EMAIL))
    }

    fun save(profile: MeProfile) {
        store.transaction {
            write(KEY_SUB, profile.sub)
            write(KEY_EMAIL, profile.email)
            write(KEY_SIGNED_OUT, null)
        }
    }

    // An explicit sign-out keeps every local record and the identity they
    // belong to; it only stops the app from opening signed in.
    fun markSignedOut() {
        store.transaction { write(KEY_SIGNED_OUT, "1") }
    }

    fun signedOut(): Boolean = read(KEY_SIGNED_OUT) == "1"

    fun lastSyncAt(): Long? = read(KEY_LAST_SYNC)?.toLongOrNull()

    fun markSynced(atMillis: Long) {
        store.transaction { write(KEY_LAST_SYNC, atMillis.toString()) }
    }

    private fun read(key: String): String? =
        store.db.rawQuery("SELECT value FROM $META WHERE key = ?", arrayOf(key)).use { cursor ->
            if (cursor.moveToFirst() && !cursor.isNull(0)) cursor.getString(0) else null
        }

    private fun write(
        key: String,
        value: String?,
    ) {
        val db = store.db
        if (value == null) {
            db.delete(META, "key = ?", arrayOf(key))
            return
        }
        val values =
            ContentValues().apply {
                put("key", key)
                put("value", value)
            }
        db.insertWithOnConflict(META, null, values, android.database.sqlite.SQLiteDatabase.CONFLICT_REPLACE)
    }

    private companion object {
        const val KEY_SUB = "identity.sub"
        const val KEY_EMAIL = "identity.email"
        const val KEY_SIGNED_OUT = "identity.signed_out"
        const val KEY_LAST_SYNC = "sync.last_success"
    }
}
