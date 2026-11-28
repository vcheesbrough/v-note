package link.desync.vnote

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import link.desync.vnote.api.codec.encodeStroke
import link.desync.vnote.model.Stroke
import org.json.JSONArray
import org.json.JSONObject

// The local store exactly as 0.85 (#159) created it — schema version 1 — for
// tests of what an upgrade does to an install that already has one (#651).
// The DDL is 0.85's, copied rather than shared with the app: a later schema
// change must not silently change what "an old install" means here.
private val VERSION_ONE_SCHEMA =
    listOf(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)",
        "CREATE TABLE pages (id TEXT PRIMARY KEY, title TEXT NOT NULL, created_at TEXT NOT NULL, " +
            "updated_at TEXT NOT NULL, paper TEXT NOT NULL, thumbnail TEXT, " +
            "local_only INTEGER NOT NULL DEFAULT 0, ink_synced_at TEXT)",
        "CREATE TABLE page_ink (page_id TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0, paper TEXT)",
        "CREATE TABLE confirmed_batches (page_id TEXT NOT NULL, client_batch_id TEXT NOT NULL, seq INTEGER NOT NULL, " +
            "strokes TEXT NOT NULL, PRIMARY KEY (page_id, client_batch_id))",
        "CREATE TABLE tombstones (page_id TEXT NOT NULL, stroke_id TEXT NOT NULL, PRIMARY KEY (page_id, stroke_id))",
        "CREATE TABLE outbox (position INTEGER PRIMARY KEY AUTOINCREMENT, op_id TEXT NOT NULL UNIQUE, " +
            "page_id TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, state TEXT NOT NULL, " +
            "detail_code TEXT, detail_message TEXT)",
        "CREATE INDEX outbox_page_idx ON outbox (page_id, state)",
    )

// One outbox row as 0.85 wrote it.
internal data class VersionOneOp(
    val opId: String,
    val pageId: String,
    val kind: String,
    val payload: JSONObject,
    val state: String,
    val detailCode: String? = null,
    val detailMessage: String? = null,
)

// A version-1 database file named [name] holding [ops], closed again so the
// app's own store opens — and upgrades — it.
internal fun seedVersionOneStore(
    context: Context,
    name: String,
    ops: List<VersionOneOp>,
) {
    val file = context.getDatabasePath(name)
    file.parentFile?.mkdirs()
    SQLiteDatabase.openOrCreateDatabase(file, null).use { db ->
        VERSION_ONE_SCHEMA.forEach(db::execSQL)
        ops.forEach { op ->
            db.insertOrThrow(
                "outbox",
                null,
                ContentValues().apply {
                    put("op_id", op.opId)
                    put("page_id", op.pageId)
                    put("kind", op.kind)
                    put("payload", op.payload.toString())
                    put("state", op.state)
                    put("detail_code", op.detailCode)
                    put("detail_message", op.detailMessage)
                },
            )
        }
        db.version = 1
    }
}

internal fun batchPayload(vararg strokes: Stroke): JSONObject =
    JSONObject().put("strokes", JSONArray().also { array -> strokes.forEach { array.put(encodeStroke(it)) } })

// The state the bug left behind: one batch on [pageId], parked as rejected
// by a proxy's 404 taken for the server's verdict.
internal fun seedVersionOneOutbox(
    context: Context,
    name: String,
    pageId: String,
    batchId: String,
    stroke: Stroke,
) = seedVersionOneStore(
    context,
    name,
    listOf(
        VersionOneOp(
            opId = batchId,
            pageId = pageId,
            kind = "commit_batch",
            payload = batchPayload(stroke),
            state = "rejected",
            detailCode = "forbidden",
            detailMessage = "Page connection failed: HTTP 404 (HTTP 404)",
        ),
    ),
)
