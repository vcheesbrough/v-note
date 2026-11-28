package link.desync.vnote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CloudOff
import androidx.compose.material3.AlertDialog
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
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import link.desync.vnote.sync.GonePage
import link.desync.vnote.sync.SyncIndicator
import link.desync.vnote.sync.SyncStatus
import java.text.DateFormat
import java.util.Date

// What the status icon can do besides show (#651): sign in again when there
// is no token, and keep a permanently deleted page's edits as a new page.
class SyncActions(
    val onSignIn: () -> Unit = {},
    val onKeepAsNewPage: (String) -> Unit = {},
)

class SyncUi(
    val status: SyncStatus,
    val actions: SyncActions,
)

// Provided once, by the activity, so every screen's top bar wears the same
// icon without threading it through each screen's parameters. Null in a
// screen rendered on its own (a UI test), which then shows no icon.
val LocalSyncUi = compositionLocalOf<SyncUi?> { null }

// The top bar's sync status (#159, #651), on every screen. Online shows
// nothing — not while edits are on their way either, so drawing never makes
// it flash. Everything that is not online wears the one icon, the cloud with
// a line through it: the last attempt failed, whatever the failure, or there
// is no token to sync with. Tapping it says what is wrong and what is
// waiting. It carries no count (#657): how much is waiting is the details'
// to say, not a number ticking up on the bar with every stroke drawn
// offline. The top bar reserves its place ([SyncStatusSlot]), so nothing
// moves when it appears or goes.
@Composable
fun SyncStatusIcon() {
    val ui = LocalSyncUi.current ?: return
    val status = ui.status
    val indicator = status.indicator
    if (indicator == SyncIndicator.Synced) return
    var open by remember { mutableStateOf(false) }
    val label = if (indicator == SyncIndicator.SignInNeeded) "Sign in to sync" else "Offline"
    IconButton(
        modifier =
            Modifier
                .testTag("sync-status-${indicator.tag}")
                .semantics { contentDescription = label },
        onClick = { open = true },
    ) {
        Icon(Icons.Outlined.CloudOff, contentDescription = null, tint = MaterialTheme.colorScheme.onSurface)
    }
    if (open) {
        SyncDetailsDialog(ui, onDismiss = { open = false })
    }
}

// The place the top bar keeps for the icon, shown or not: an icon button's
// touch target.
internal val SyncStatusSlot = 48.dp

@Composable
private fun SyncDetailsDialog(
    ui: SyncUi,
    onDismiss: () -> Unit,
) {
    val status = ui.status
    val signInNeeded = status.indicator == SyncIndicator.SignInNeeded
    AlertDialog(
        onDismissRequest = onDismiss,
        modifier = Modifier.testTag("sync-status-details"),
        title = { Text(if (signInNeeded) "Sign in to sync" else "Offline") },
        text = {
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                SyncDiagnostics(status)
                status.gonePages.forEach { page ->
                    GonePageRow(page, ui.actions, onDone = onDismiss)
                }
            }
        },
        confirmButton = {
            // Only having no token needs the user (#651); everything else
            // recovers by itself.
            if (signInNeeded) {
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

// A page permanently deleted elsewhere that this device still owes edits
// (#651). They stay owed and retried whatever the user does; this only
// offers to move them to a new page, where they can land. It never
// interrupts, and there is no way to discard them.
@Composable
private fun GonePageRow(
    page: GonePage,
    actions: SyncActions,
    onDone: () -> Unit,
) {
    Box(modifier = Modifier.testTag("sync-gone-${page.pageId}")) {
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            val strokes = if (page.strokeCount == 1) "1 stroke is" else "${page.strokeCount} strokes are"
            Text(
                "A page was permanently deleted on another device. $strokes waiting for it.",
                style = MaterialTheme.typography.bodyMedium,
            )
            TextButton(
                modifier = Modifier.testTag("sync-gone-keep"),
                onClick = {
                    onDone()
                    actions.onKeepAsNewPage(page.pageId)
                },
            ) { Text("Keep as a new page") }
        }
    }
}
