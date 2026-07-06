package link.desync.vnote.ui

import link.desync.vnote.auth.PageSummary

private const val UntitledPage = "Untitled page"

fun PageSummary.displayTitle(): String =
    if (title.trim().isNotEmpty() && title.trim() != UntitledPage) {
        title
    } else {
        "Page updated ${updatedAt.compactTimestamp()}"
    }

fun PageSummary.detailLine(): String =
    "Created ${createdAt.compactTimestamp()} · Updated ${updatedAt.compactTimestamp()}"

private fun String.compactTimestamp(): String =
    trim()
        .removeSuffix("Z")
        .replace('T', ' ')
        .take(16)
