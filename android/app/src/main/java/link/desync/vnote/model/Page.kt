package link.desync.vnote.model

import link.desync.vnote.ink.Paper

// The page library as the app models it: the signed-in profile, page summaries
// with their thumbnail state, and the library channel's events. Mirrors the
// REST DTOs and `LibraryEvent` in `crates/protocol`.

data class MeProfile(
    val sub: String,
    val email: String?,
)

data class PageSummary(
    val id: String,
    val title: String,
    val createdAt: String,
    val updatedAt: String,
    val thumbnail: ThumbnailMetadata = ThumbnailMetadata.Empty,
    val paper: Paper = Paper.None,
    // When the page was moved to the bin (RFC 3339), or null for a live page
    // (#631). Only the bin listing and `page-binned` carry it.
    val binnedAt: String? = null,
)

sealed interface ThumbnailMetadata {
    data object Empty : ThumbnailMetadata

    data class Generating(
        val sourceSeq: Long,
    ) : ThumbnailMetadata

    data class Available(
        val sourceSeq: Long,
        val url: String,
    ) : ThumbnailMetadata

    data class Failed(
        val sourceSeq: Long,
    ) : ThumbnailMetadata
}

sealed interface LibraryEvent {
    data class PageCreated(
        val page: PageSummary,
    ) : LibraryEvent

    // Permanently deleted, from the bin: the page and its ink are gone (#631).
    data class PageDeleted(
        val pageId: String,
    ) : LibraryEvent

    // Moved to the bin: leaves the library, joins the bin. [page] carries
    // `binnedAt`.
    data class PageBinned(
        val page: PageSummary,
    ) : LibraryEvent

    // Back from the bin with all its ink: leaves the bin, rejoins the library.
    data class PageRestored(
        val page: PageSummary,
    ) : LibraryEvent

    data class PageThumbnailUpdated(
        val pageId: String,
        val thumbnail: ThumbnailMetadata,
    ) : LibraryEvent

    data class PageUpdated(
        val pageId: String,
        val updatedAt: String,
    ) : LibraryEvent
}
