package link.desync.vnote.library

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test

/**
 * The page library's list rules (#337). Until these moved out of MainActivity
 * they were exercised only by the instrumented library tests on an emulator.
 */
class LibraryPagesTest {
    @Test
    fun upsertAddsAPageAndKeepsTheListRecentFirst() {
        val pages = listOf(page("older", "2026-07-22T09:00:00Z"))

        val updated = pages.upsert(page("newer", "2026-07-22T10:00:00Z"))

        assertEquals(listOf("newer", "older"), updated.ids())
    }

    @Test
    fun upsertReplacesThePageWithTheSameId() {
        val pages = listOf(page("a", "2026-07-22T09:00:00Z"), page("b", "2026-07-22T08:00:00Z"))

        val updated = pages.upsert(page("b", "2026-07-22T11:00:00Z").copy(title = "renamed"))

        assertEquals(listOf("b", "a"), updated.ids())
        assertEquals("renamed", updated.first().title)
    }

    @Test
    fun pageCreatedArrivesWithItsPaper() {
        val created = page("new", "2026-07-22T12:00:00Z").copy(paper = Paper.RuledMarginWide)

        val updated = emptyList<PageSummary>().applying(LibraryEvent.PageCreated(created))

        assertEquals(listOf(created), updated)
    }

    @Test
    fun pageDeletedRemovesThePage() {
        val pages = listOf(page("a", "2026-07-22T10:00:00Z"), page("b", "2026-07-22T09:00:00Z"))

        assertEquals(listOf("b"), pages.applying(LibraryEvent.PageDeleted("a")).ids())
    }

    @Test
    fun pageUpdatedMovesTheEditedPageToTheFrontAndKeepsItsPaper() {
        val pages =
            listOf(
                page("newer", "2026-07-22T10:00:00Z"),
                page("older", "2026-07-22T09:00:00Z").copy(paper = Paper.SquaredSmall),
            )

        val updated = pages.applying(LibraryEvent.PageUpdated("older", "2026-07-22T11:00:00Z"))

        assertEquals(listOf("older", "newer"), updated.ids())
        assertEquals("2026-07-22T11:00:00Z", updated.first().updatedAt)
        assertEquals("a re-sort must not blank paper", Paper.SquaredSmall, updated.first().paper)
    }

    @Test
    fun pageUpdatedIgnoresAStaleOrEqualTimestamp() {
        val pages = listOf(page("a", "2026-07-22T10:00:00Z"), page("b", "2026-07-22T09:00:00Z"))

        assertSame(pages, pages.applying(LibraryEvent.PageUpdated("b", "2026-07-22T09:00:00Z")))
        assertSame(pages, pages.applying(LibraryEvent.PageUpdated("b", "2026-07-22T08:00:00Z")))
    }

    @Test
    fun pageUpdatedForAnUnknownPageIsIgnored() {
        val pages = listOf(page("a", "2026-07-22T10:00:00Z"))

        assertSame(pages, pages.applying(LibraryEvent.PageUpdated("missing", "2026-07-22T12:00:00Z")))
    }

    @Test
    fun aThumbnailUpdateAppliesUnlessItIsForAnOlderRevision() {
        val generating = ThumbnailMetadata.Generating(sourceSeq = 5)
        val pages = listOf(page("a", "2026-07-22T10:00:00Z").copy(thumbnail = generating))

        val stale = pages.applying(LibraryEvent.PageThumbnailUpdated("a", ThumbnailMetadata.Available(4, "/t/4")))
        assertEquals(generating, stale.single().thumbnail)

        val ready = ThumbnailMetadata.Available(5, "/t/5")
        assertEquals(ready, pages.applying(LibraryEvent.PageThumbnailUpdated("a", ready)).single().thumbnail)

        val newer = ThumbnailMetadata.Failed(6)
        assertEquals(newer, pages.applying(LibraryEvent.PageThumbnailUpdated("a", newer)).single().thumbnail)
    }

    @Test
    fun anEmptyThumbnailCountsAsRevisionZero() {
        assertEquals(0L, ThumbnailMetadata.Empty.sourceSeq())
        assertEquals(7L, ThumbnailMetadata.Available(7, "/t/7").sourceSeq())
    }

    private fun page(
        id: String,
        updatedAt: String,
    ): PageSummary =
        PageSummary(
            id = id,
            title = id,
            createdAt = "2026-07-22T00:00:00Z",
            updatedAt = updatedAt,
        )

    private fun List<PageSummary>.ids(): List<String> = map { it.id }
}
