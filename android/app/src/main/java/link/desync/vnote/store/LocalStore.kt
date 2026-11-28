package link.desync.vnote.store

import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update

// The on-device copy of everything the app shows (#159): the library, every
// page's confirmed ink, and the outbox of edits the server has not yet
// acknowledged. The app is local-first — screens read and write this store,
// and the server syncs into it — so being offline changes nothing the user can
// do. The server stays the canonical store; this is its durable cache plus
// the edits on their way to it.
//
// Plain SQLite rather than Room: four small tables need no code generation,
// and every write that must survive a crash is one transaction. Not encrypted
// by the app (deliberate, #159/#628): it relies on Android's storage
// encryption and the app sandbox, and is excluded from backup and device
// transfer (`res/xml/data_extraction_rules.xml`).
class LocalStore internal constructor(
    context: Context,
    name: String?,
) {
    private val helper = Helper(context.applicationContext, name)

    internal val db: SQLiteDatabase get() = helper.writableDatabase

    val identity = IdentityStore(this)
    val library = LibraryCache(this)
    val ink = InkCache(this)
    val outbox = SyncOutbox(this)

    // Bumped after every committed write, so the sync status can recount what
    // is pending without polling.
    private val version = MutableStateFlow(0L)
    val changes: StateFlow<Long> = version

    internal fun changed() {
        version.update { it + 1 }
    }

    // One transaction; nested calls join the outer one (SQLite on Android
    // counts them), so a caller can make two stores' writes atomic together.
    internal fun <T> transaction(block: (SQLiteDatabase) -> T): T {
        val database = db
        database.beginTransaction()
        try {
            val result = block(database)
            database.setTransactionSuccessful()
            return result
        } finally {
            database.endTransaction()
            if (!database.inTransaction()) changed()
        }
    }

    // A different identity signed in (#159: one identity per install): the
    // whole install starts over. Tokens are the caller's to clear.
    fun resetAll() {
        transaction { database -> TABLES.forEach { database.delete(it, null, null) } }
    }

    private class Helper(
        context: Context,
        name: String?,
    ) : SQLiteOpenHelper(context, name, null, SCHEMA_VERSION) {
        init {
            setWriteAheadLoggingEnabled(true)
        }

        override fun onConfigure(db: SQLiteDatabase) {
            // WAL defaults to `synchronous=NORMAL` on Android, which can drop
            // the last commits on power loss. An accepted stroke must survive
            // that too, so every commit is synced to storage.
            db.rawQuery("PRAGMA synchronous = FULL", null).use { it.moveToFirst() }
        }

        override fun onCreate(db: SQLiteDatabase) {
            SCHEMA.forEach(db::execSQL)
        }

        override fun onUpgrade(
            db: SQLiteDatabase,
            oldVersion: Int,
            newVersion: Int,
        ) {
            if (oldVersion < SCHEMA_NO_REFUSALS) db.execSQL(UNSTICK_REFUSED)
        }
    }

    companion object {
        private const val DB_NAME = "vnote-local.db"

        // 2 (#651): the outbox has no refused state. See [UNSTICK_REFUSED].
        private const val SCHEMA_NO_REFUSALS = 2
        private const val SCHEMA_VERSION = SCHEMA_NO_REFUSALS

        @Volatile
        private var shared: LocalStore? = null

        // The process's one store: the activity, the sync engine and every
        // instrumented test share it, as they share the token store.
        fun shared(context: Context): LocalStore =
            shared ?: synchronized(this) {
                shared ?: LocalStore(context, DB_NAME).also { shared = it }
            }

        // An in-memory store for tests.
        internal fun inMemory(context: Context): LocalStore = LocalStore(context, null)
    }
}

internal const val META = "meta"
internal const val PAGES = "pages"
internal const val PAGE_INK = "page_ink"
internal const val BATCHES = "confirmed_batches"
internal const val TOMBSTONES = "tombstones"
internal const val OUTBOX = "outbox"

private val TABLES = listOf(META, PAGES, PAGE_INK, BATCHES, TOMBSTONES, OUTBOX)

private val SCHEMA =
    listOf(
        "CREATE TABLE $META (key TEXT PRIMARY KEY, value TEXT)",
        // The library as last seen, plus pages created here that the server
        // has not yet acknowledged (`local_only`). `ink_synced_at` is the
        // page's `updated_at` when its ink was last fetched in full, so the
        // sync engine re-fetches a page only when it changed.
        "CREATE TABLE $PAGES (id TEXT PRIMARY KEY, title TEXT NOT NULL, created_at TEXT NOT NULL, " +
            "updated_at TEXT NOT NULL, paper TEXT NOT NULL, thumbnail TEXT, " +
            "local_only INTEGER NOT NULL DEFAULT 0, ink_synced_at TEXT)",
        // Per page: the replay cursor and the paper the server confirmed.
        "CREATE TABLE $PAGE_INK (page_id TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0, paper TEXT)",
        // Server-confirmed batches, in server order (`seq`).
        "CREATE TABLE $BATCHES (page_id TEXT NOT NULL, client_batch_id TEXT NOT NULL, seq INTEGER NOT NULL, " +
            "strokes TEXT NOT NULL, PRIMARY KEY (page_id, client_batch_id))",
        "CREATE TABLE $TOMBSTONES (page_id TEXT NOT NULL, stroke_id TEXT NOT NULL, PRIMARY KEY (page_id, stroke_id))",
        // Edits on their way to the server, in the order they were made
        // (`position`, which is also the per-page replay order). `op_id` is
        // the immutable id the server is idempotent on: `client_batch_id` or
        // `client_mutation_id`, or the page id for a create. `detail_code`
        // and `detail_message` held a refusal's reason until #651 and are
        // unused since: kept so both schema versions have one shape.
        "CREATE TABLE $OUTBOX (position INTEGER PRIMARY KEY AUTOINCREMENT, op_id TEXT NOT NULL UNIQUE, " +
            "page_id TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, state TEXT NOT NULL, " +
            "detail_code TEXT, detail_message TEXT)",
        "CREATE INDEX outbox_page_idx ON $OUTBOX (page_id, state)",
    )

// Schema 1 → 2 (#651). Before it, a failure the app took for a definitive
// refusal — a proxy's 404 with the server down, a 403 off the VPN — parked
// every edit the page owed as `rejected`, never to be sent again. Nothing is
// refused any more: those entries are owed like any other and replay by
// themselves. The server is idempotent on each entry's id, so one it had in
// fact stored is only re-echoed — nothing is lost and nothing duplicated.
private const val UNSTICK_REFUSED =
    "UPDATE $OUTBOX SET state = 'pending', detail_code = NULL, detail_message = NULL WHERE state = 'rejected'"
