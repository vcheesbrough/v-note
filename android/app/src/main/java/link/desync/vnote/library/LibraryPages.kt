package link.desync.vnote.library

import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata

// The page library's list rules, pure over an immutable list so they are
// JVM-tested (LibraryPagesTest): recent-first order, create-or-replace,
// removal, and the library channel's events. Mirrors
// `apply_library_event_to_pages` in the SPA — two clients, one set of rules.

// Adds [page], or replaces the page with its id, keeping the list recent-first.
internal fun List<PageSummary>.upsert(page: PageSummary): List<PageSummary> =
    (filterNot { it.id == page.id } + page)
        .sortedByDescending { it.updatedAt }

internal fun List<PageSummary>.without(pageId: String): List<PageSummary> = filterNot { it.id == pageId }

// Applies one library-channel event. Page selection is not the list's concern;
// the caller clears it when the open page is deleted.
internal fun List<PageSummary>.applying(event: LibraryEvent): List<PageSummary> =
    when (event) {
        is LibraryEvent.PageCreated -> upsert(event.page)
        is LibraryEvent.PageDeleted -> without(event.pageId)
        is LibraryEvent.PageThumbnailUpdated ->
            map { page ->
                // A late update for an older revision never replaces a newer thumbnail.
                if (page.id == event.pageId && event.thumbnail.sourceSeq() >= page.thumbnail.sourceSeq()) {
                    page.copy(thumbnail = event.thumbnail)
                } else {
                    page
                }
            }
        is LibraryEvent.PageUpdated -> {
            val current = firstOrNull { it.id == event.pageId }
            // Ignore a stale/duplicate timestamp so re-sort stays idempotent.
            if (current != null && event.updatedAt > current.updatedAt) {
                map { page -> if (page.id == event.pageId) page.copy(updatedAt = event.updatedAt) else page }
                    .sortedByDescending { it.updatedAt }
            } else {
                this
            }
        }
    }

internal fun ThumbnailMetadata.sourceSeq(): Long =
    when (this) {
        ThumbnailMetadata.Empty -> 0
        is ThumbnailMetadata.Generating -> sourceSeq
        is ThumbnailMetadata.Available -> sourceSeq
        is ThumbnailMetadata.Failed -> sourceSeq
    }
