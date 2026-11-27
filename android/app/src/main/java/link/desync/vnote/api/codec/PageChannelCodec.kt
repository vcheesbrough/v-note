package link.desync.vnote.api.codec

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.Stroke
import org.json.JSONArray
import org.json.JSONObject

// JSON codecs for the per-page realtime channel: every server → client message
// the app understands, and every client → server message it sends.

internal fun decodePageEvent(json: JSONObject): PageEvent? =
    when (json.getString("type")) {
        "welcome" ->
            PageEvent.Welcome(
                sessionId = json.getString("session_id"),
                lastSeq = json.getLong("last_seq"),
                paper = Paper.fromWire(json.optString("paper").ifBlank { null }) ?: Paper.None,
            )
        "stroke-batch" -> decodeStrokeBatch(json)
        "page-replay" -> decodePageReplay(json)
        "synced" -> PageEvent.Synced(json.getLong("last_seq"))
        "tombstone-batch" -> decodeTombstoneBatch(json)
        "paper-changed" ->
            Paper.fromWire(json.getString("paper"))?.let { paper ->
                PageEvent.PaperChanged(paper = paper, revision = json.getLong("revision"))
            }
        "page-binned" -> PageEvent.PageBinned(json.getString("binned_at"))
        "page-restored" -> PageEvent.PageRestored
        "error" ->
            PageEvent.Failure(
                code = json.optString("code"),
                message = json.optString("message"),
                clientMutationId = json.optString("client_mutation_id").ifBlank { null },
            )
        // Unknown/forward-compatible message types are ignored.
        else -> null
    }

internal fun encodeSubscribe(fromSeq: Long): String = JSONObject().put("type", "subscribe").put("from_seq", fromSeq).toString()

internal fun encodeCommitBatch(
    clientBatchId: String,
    strokes: List<Stroke>,
): String {
    val strokesArray = JSONArray()
    for (stroke in strokes) {
        strokesArray.put(encodeStroke(stroke))
    }
    return JSONObject()
        .put("type", "commit-batch")
        .put("client_batch_id", clientBatchId)
        .put("strokes", strokesArray)
        .toString()
}

internal fun encodeSetPaper(
    clientMutationId: String,
    paper: Paper,
): String =
    JSONObject()
        .put("type", "set-paper")
        .put("client_mutation_id", clientMutationId)
        .put("paper", paper.wireValue)
        .toString()

internal fun encodeCommitTombstones(
    clientMutationId: String,
    strokeIds: List<String>,
): String =
    JSONObject()
        .put("type", "commit-tombstones")
        .put("client_mutation_id", clientMutationId)
        .put("stroke_ids", JSONArray(strokeIds))
        .toString()
