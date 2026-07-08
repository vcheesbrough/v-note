package link.desync.vnote.ui

import link.desync.vnote.auth.PageSummary
import java.time.Duration
import java.time.Instant
import java.time.OffsetDateTime

private const val UntitledPage = "Untitled page"

fun PageSummary.hasDisplayTitle(): Boolean = title.trim().isNotEmpty() && title.trim() != UntitledPage

fun PageSummary.displayTitle(): String =
    if (hasDisplayTitle()) {
        title
    } else {
        updatedAt.approximateRelativeTimestamp()
    }

private fun String.compactTimestamp(): String =
    trim()
        .removeSuffix("Z")
        .replace('T', ' ')
        .take(16)

private fun String.approximateRelativeTimestamp(): String {
    val instant = parseInstant() ?: return compactTimestamp()
    val elapsedSeconds = Duration.between(instant, Instant.now()).seconds.coerceAtLeast(0)
    val amountAndUnit =
        when (elapsedSeconds) {
            in 0..89 -> return "just now"
            in 90..5_399 -> Pair((elapsedSeconds + 30) / 60, "minute")
            in 5_400..129_599 -> Pair((elapsedSeconds + 1_800) / 3_600, "hour")
            in 129_600..3_887_999 -> Pair((elapsedSeconds + 43_200) / 86_400, "day")
            else -> Pair((elapsedSeconds + 1_296_000) / 2_592_000, "month")
        }
    val (amount, unit) = amountAndUnit
    val suffix = if (amount == 1L) "" else "s"
    return "$amount $unit$suffix ago"
}

private fun String.parseInstant(): Instant? =
    runCatching { Instant.parse(trim()) }
        .recoverCatching { OffsetDateTime.parse(trim()).toInstant() }
        .getOrNull()
