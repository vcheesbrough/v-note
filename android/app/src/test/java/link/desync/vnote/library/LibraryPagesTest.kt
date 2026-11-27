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

    // #631: a binned page leaves the library and joins the bin, most recently
    // binned first; restoring reverses both, back at its old library place.
    @Test
    fun binningMovesAPageFromTheLibraryToTheBinAndRestoringMovesItBack() {
        val library = listOf(page("new", "2026-07-22T12:00:00Z"), page("mid", "2026-07-22T11:00:00Z"), page("old", "2026-07-22T10:00:00Z"))
        val earlier = page("old", "2026-07-22T10:00:00Z").copy(binnedAt = "2026-11-27T20:00:00Z")
        val later = page("mid", "2026-07-22T11:00:00Z").copy(binnedAt = "2026-11-27T21:00:00Z")

        var pages = library.applying(LibraryEvent.PageBinned(earlier)).applying(LibraryEvent.PageBinned(later))
        var bin = emptyList<PageSummary>().binApplying(LibraryEvent.PageBinned(earlier)).binApplying(LibraryEvent.PageBinned(later))
        assertEquals(listOf("new"), pages.ids())
        assertEquals(listOf("mid", "old"), bin.ids())

        val restored = later.copy(binnedAt = null)
        pages = pages.applying(LibraryEvent.PageRestored(restored))
        bin = bin.binApplying(LibraryEvent.PageRestored(restored))
        assertEquals(listOf("new", "mid"), pages.ids())
        assertEquals(listOf("old"), bin.ids())

        bin = bin.binApplying(LibraryEvent.PageDeleted("old"))
        assertEquals(emptyList<String>(), bin.ids())
    }

    // A binned page's preview keeps updating, and live-page events leave the bin alone.
    @Test
    fun theBinFollowsThumbnailsAndIgnoresLivePageEvents() {
        val binned = page("b", "2026-07-22T10:00:00Z").copy(binnedAt = "2026-11-27T21:00:00Z")
        val bin = listOf(binned)

        val thumbnail = ThumbnailMetadata.Available(2, "/api/pages/b/thumbnails/2")
        assertEquals(thumbnail, bin.binApplying(LibraryEvent.PageThumbnailUpdated("b", thumbnail)).single().thumbnail)
        assertSame(bin, bin.binApplying(LibraryEvent.PageCreated(page("c", "2026-07-22T12:00:00Z"))))
        assertSame(bin, bin.binApplying(LibraryEvent.PageUpdated("b", "2026-07-22T13:00:00Z")))
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
