package link.desync.vnote.ink

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp

// The open page's persistent banner (#150), the Android counterpart of the
// SPA's `.sync-banner`. Tagged `page-status-<kind>` so a UI test asserts which
// kind of trouble is shown, not merely that some text is.
@Composable
internal fun PageStatusBanner(
    status: PageStatus,
    unsentStrokes: Int,
    onRetry: () -> Unit,
    onSignIn: () -> Unit,
    modifier: Modifier = Modifier,
    onKeepAsNewPage: () -> Unit = {},
    onDiscard: () -> Unit = {},
) {
    val container = if (status.isError) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.surfaceVariant
    val content = if (status.isError) MaterialTheme.colorScheme.onErrorContainer else MaterialTheme.colorScheme.onSurfaceVariant
    Surface(
        color = container,
        shape = RoundedCornerShape(7.dp),
        modifier =
            modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 4.dp)
                .testTag("page-status-${status.kind.tag}")
                .semantics { liveRegion = LiveRegionMode.Polite },
    ) {
        Row(
            modifier = Modifier.padding(start = 12.dp, end = 4.dp, top = 6.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(status.message, style = MaterialTheme.typography.bodyMedium, color = content)
                // Kept ink is not synced ink: say how much is waiting, so a
                // banner over a page full of fresh strokes cannot read as
                // "all synced except the connection".
                if (unsentStrokes > 0) {
                    val what = if (unsentStrokes == 1) "1 stroke" else "$unsentStrokes strokes"
                    Text(
                        "$what not yet synced",
                        modifier = Modifier.testTag("page-status-unsent"),
                        style = MaterialTheme.typography.bodySmall,
                        color = content,
                    )
                }
                status.detail?.let { detail ->
                    Text(
                        detail,
                        modifier = Modifier.testTag("page-status-detail"),
                        style = MaterialTheme.typography.labelSmall,
                        color = content,
                    )
                }
            }
            when (status.kind) {
                PageStatusKind.SessionExpired ->
                    TextButton(onClick = onSignIn, modifier = Modifier.testTag("page-status-sign-in")) { Text("Sign in") }
                PageStatusKind.Disconnected ->
                    TextButton(onClick = onRetry, modifier = Modifier.testTag("page-status-retry")) { Text("Retry") }
                // Refused for good (#159): nothing to retry. Keep the strokes
                // as a new page — the device may hold the only copy — or let
                // them go; never dropped without one of the two.
                PageStatusKind.Rejected ->
                    Column {
                        TextButton(onClick = onKeepAsNewPage, modifier = Modifier.testTag("page-status-keep")) {
                            Text("Keep as a new page")
                        }
                        TextButton(onClick = onDiscard, modifier = Modifier.testTag("page-status-discard")) { Text("Discard") }
                    }
                // Retrying already retries; connecting has nothing for the
                // user to do but wait.
                PageStatusKind.Connecting, PageStatusKind.Retrying -> Unit
            }
        }
    }
}
