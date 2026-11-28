package link.desync.vnote.api.codec

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import org.json.JSONArray
import org.json.JSONObject

// JSON codecs for the REST resources and the library channel. `internal` rather
// than private so the JVM unit tests can hold every one against
// `contracts/fixtures/` (see ContractFixtureTest).

internal fun decodeMe(json: JSONObject): MeProfile =
    MeProfile(
        sub = json.getString("sub"),
        email = json.optString("email").ifBlank { null },
    )

internal fun decodePage(json: JSONObject): PageSummary =
    PageSummary(
        id = json.getString("id"),
        title = json.getString("title"),
        createdAt = json.getString("created_at"),
        updatedAt = json.getString("updated_at"),
        thumbnail = json.optJSONObject("thumbnail")?.let(::decodeThumbnail) ?: ThumbnailMetadata.Empty,
        // Absent on pre-v5 payloads, which render as blank pages.
        paper = Paper.fromWire(json.optString("paper").ifBlank { null }) ?: Paper.None,
        // Absent on a live page; the bin listing and `page-binned` carry it.
        binnedAt = if (json.isNull("binned_at")) null else json.getString("binned_at"),
    )

internal fun decodePageList(json: JSONObject): List<PageSummary> {
    val pages = json.getJSONArray("pages")
    return buildList {
        for (index in 0 until pages.length()) {
            add(decodePage(pages.getJSONObject(index)))
        }
    }
}

internal fun decodeThumbnail(json: JSONObject): ThumbnailMetadata =
    when (json.getString("status")) {
        "empty" -> ThumbnailMetadata.Empty
        "generating" -> ThumbnailMetadata.Generating(json.getLong("source_seq"))
        "available" -> ThumbnailMetadata.Available(json.getLong("source_seq"), json.getString("url"))
        "failed" -> ThumbnailMetadata.Failed(json.getLong("source_seq"))
        else -> error("Unknown thumbnail status")
    }

internal fun decodeLibraryEvent(json: JSONObject): LibraryEvent =
    when (val type = json.getString("type")) {
        "page-created" -> LibraryEvent.PageCreated(decodePage(json.getJSONObject("page")))
        "page-deleted" -> LibraryEvent.PageDeleted(json.getString("page_id"))
        "page-binned" -> LibraryEvent.PageBinned(decodePage(json.getJSONObject("page")))
        "page-restored" -> LibraryEvent.PageRestored(decodePage(json.getJSONObject("page")))
        "page-thumbnail-updated" ->
            LibraryEvent.PageThumbnailUpdated(
                json.getString("page_id"),
                decodeThumbnail(json.getJSONObject("thumbnail")),
            )
        "page-updated" ->
            LibraryEvent.PageUpdated(
                json.getString("page_id"),
                json.getString("updated_at"),
            )
        else -> error("Unknown realtime event type: $type")
    }

// `DELETE /api/bin`'s answer: the pages it permanently deleted (#631).
internal fun decodePageIds(json: JSONObject): List<String> {
    val ids = json.getJSONArray("page_ids")
    return List(ids.length()) { index -> ids.getString(index) }
}

// `DELETE /api/bin`'s body: the pages the user confirmed (#631).
internal fun encodeEmptyBin(pageIds: List<String>): String = JSONObject().put("page_ids", JSONArray(pageIds)).toString()

// The paper is sent on create rather than as a post-create set-paper: one round
// trip, no revision bump. A null title is omitted, not sent as `null`.
//
// [id] is the page id minted on this device (#159), omitted when the server
// is to choose one.
internal fun encodeCreatePage(
    title: String?,
    paper: Paper,
    id: String? = null,
): String =
    JSONObject()
        .apply {
            id?.let { put("id", it) }
            title?.let { put("title", it) }
            put("paper", paper.wireValue)
        }.toString()
