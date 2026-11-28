package link.desync.vnote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.Login
import androidx.compose.material.icons.outlined.CloudOff
import androidx.compose.material.icons.outlined.CloudSync
import androidx.compose.material.icons.outlined.SyncProblem
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import link.desync.vnote.store.Rejection
import link.desync.vnote.sync.SyncIndicator
import link.desync.vnote.sync.SyncStatus
import java.text.DateFormat
import java.util.Date

// What the status icon can do besides show (#159): sign in again, and the two
// ways out of a refused page.
class SyncActions(
    val onSignIn: () -> Unit = {},
    val onKeepAsNewPage: (String) -> Unit = {},
    val onDiscard: (String) -> Unit = {},
)

class SyncUi(
    val status: SyncStatus,
    val actions: SyncActions,
)

// Provided once, by the activity, so every screen's top bar wears the same
// icon without threading it through each screen's parameters. Null in a
// screen rendered on its own (a UI test), which then shows no icon.
val LocalSyncUi = compositionLocalOf<SyncUi?> { null }

// The top bar's sync status (#159), on every screen. Quiet means healthy:
// nothing is drawn while everything is synced. Otherwise an icon per state —
// offline or syncing (with the pending count), sign-in needed, or a refused
// edit — and tapping it says what is wrong and offers what can be done.
@Composable
fun SyncStatusIcon() {
    val ui = LocalSyncUi.current ?: return
    val status = ui.status
    val indicator = status.indicator
    if (indicator == SyncIndicator.Synced) return
    var open by remember { mutableStateOf(false) }
    val (icon, label) = iconFor(indicator, status)
    IconButton(
        modifier =
            Modifier
                .testTag("sync-status-${indicator.tag}")
                .semantics { contentDescription = label },
        onClick = { open = true },
    ) {
        BadgedBox(badge = { if (status.pendingCount > 0) Badge { Text("${status.pendingCount}") } }) {
            val tint = if (indicator == SyncIndicator.Rejected) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface
            Icon(icon, contentDescription = null, tint = tint)
        }
    }
    if (open) {
        SyncDetailsDialog(ui, onDismiss = { open = false })
    }
}

private fun iconFor(
    indicator: SyncIndicator,
    status: SyncStatus,
): Pair<ImageVector, String> =
    when (indicator) {
        SyncIndicator.Rejected -> Icons.Outlined.SyncProblem to "Changes refused by the server"
        SyncIndicator.SignInNeeded -> Icons.AutoMirrored.Outlined.Login to "Sign in to sync"
        else ->
            if (status.problem != null) {
                Icons.Outlined.CloudOff to "Offline — ${status.pendingCount} changes waiting"
            } else {
                Icons.Outlined.CloudSync to "Syncing ${status.pendingCount} changes"
            }
    }

@Composable
private fun SyncDetailsDialog(
    ui: SyncUi,
    onDismiss: () -> Unit,
) {
    val status = ui.status
    AlertDialog(
        onDismissRequest = onDismiss,
        modifier = Modifier.testTag("sync-status-details"),
        title = { Text(titleFor(status.indicator)) },
        text = {
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                SyncDiagnostics(status)
                status.rejections.forEach { rejection ->
                    RejectionRow(rejection, ui.actions, onDone = onDismiss)
                }
            }
        },
        confirmButton = {
            if (status.indicator == SyncIndicator.SignInNeeded) {
                TextButton(
                    modifier = Modifier.testTag("sync-status-sign-in"),
                    onClick = {
                        onDismiss()
                        ui.actions.onSignIn()
                    },
                ) { Text("Sign in") }
            } else {
                TextButton(onClick = onDismiss) { Text("Close") }
            }
        },
    )
}

private fun titleFor(indicator: SyncIndicator): String =
    when (indicator) {
        SyncIndicator.Rejected -> "Some changes were refused"
        SyncIndicator.SignInNeeded -> "Sign in to sync"
        SyncIndicator.Offline -> "Not synced yet"
        SyncIndicator.Synced -> "Synced"
    }

// Which host, what failed, when sync last worked, and the id support can
// find the request by. Ids and classes only, never content.
@Composable
private fun SyncDiagnostics(status: SyncStatus) {
    val pending = status.pendingCount
    DiagnosticLine(
        if (pending == 1) "1 change is waiting to sync" else "$pending changes are waiting to sync",
        "sync-status-pending",
    )
    Text(
        "Your changes are kept on this device and sync automatically.",
        style = MaterialTheme.typography.bodySmall,
    )
    status.problem?.let { problem ->
        DiagnosticLine("Can't reach ${problem.host}", "sync-status-host")
        DiagnosticLine("Error: ${problem.errorClass}", "sync-status-error-class")
        problem.correlationId?.let { DiagnosticLine("Correlation id: $it", "sync-status-correlation-id") }
    }
    val lastSync = status.lastSyncAt?.let { DateFormat.getDateTimeInstance().format(Date(it)) } ?: "never"
    DiagnosticLine("Last successful sync: $lastSync", "sync-status-last-sync")
}

@Composable
private fun DiagnosticLine(
    text: String,
    tag: String,
) {
    Text(text, modifier = Modifier.testTag(tag), style = MaterialTheme.typography.bodyMedium)
}

// A refused page: what the server said, and the two ways out. Keeping the
// strokes as a new page is first because it is the one that loses nothing.
@Composable
private fun RejectionRow(
    rejection: Rejection,
    actions: SyncActions,
    onDone: () -> Unit,
) {
    Box(modifier = Modifier.testTag("sync-rejection-${rejection.pageId}")) {
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            val strokes = if (rejection.strokeCount == 1) "1 stroke" else "${rejection.strokeCount} strokes"
            Text("$strokes refused: ${rejection.message}", style = MaterialTheme.typography.bodyMedium)
            Text(rejection.code, style = MaterialTheme.typography.labelSmall)
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                TextButton(
                    modifier = Modifier.testTag("sync-rejection-keep"),
                    onClick = {
                        onDone()
                        actions.onKeepAsNewPage(rejection.pageId)
                    },
                ) { Text("Keep as a new page") }
                TextButton(
                    modifier = Modifier.testTag("sync-rejection-discard"),
                    onClick = {
                        onDone()
                        actions.onDiscard(rejection.pageId)
                    },
                ) { Text("Discard") }
            }
        }
    }
}
