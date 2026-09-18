package link.desync.vnote

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.Menu
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import link.desync.vnote.api.ApiClient
import link.desync.vnote.library.PageTile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.ui.StatePanel
import link.desync.vnote.ui.StatusBanner
import link.desync.vnote.ui.VNoteTopBar
import link.desync.vnote.ui.displayTitle

// The library screen: one top bar (menu, brand, create/sign-in) over a body that
// holds the page grid or the state panel standing in for it. Signed out it is the
// same screen with no pages, not a separate auth screen (#317).

@Composable
internal fun AppScreen(
    apiClient: ApiClient,
    sessionState: SessionState,
    onSignIn: () -> Unit,
    onSignOut: () -> Unit,
    onReload: () -> Unit,
    pages: List<PageSummary>,
    libraryError: String?,
    onCreatePage: () -> Unit,
    onOpenPage: (PageSummary) -> Unit,
    onDeletePage: (PageSummary) -> Unit,
) {
    var pendingDelete by remember { mutableStateOf<PageSummary?>(null) }

    pendingDelete?.let { page ->
        DeletePageDialog(
            page = page,
            onDismiss = { pendingDelete = null },
            onConfirm = {
                pendingDelete = null
                onDeletePage(page)
            },
        )
    }

    Column(modifier = Modifier.fillMaxSize()) {
        LibraryTopBar(
            sessionState = sessionState,
            health = rememberServerHealth(),
            onSignIn = onSignIn,
            onSignOut = onSignOut,
            onCreatePage = onCreatePage,
        )
        LibraryBody(
            apiClient = apiClient,
            sessionState = sessionState,
            pages = pages,
            libraryError = libraryError,
            onReload = onReload,
            onOpenPage = onOpenPage,
            onRequestDelete = { pendingDelete = it },
            modifier = Modifier.weight(1f),
        )
    }
}

@Composable
private fun LibraryTopBar(
    sessionState: SessionState,
    health: String,
    onSignIn: () -> Unit,
    onSignOut: () -> Unit,
    onCreatePage: () -> Unit,
) {
    VNoteTopBar {
        LibraryMenu(
            sessionState = sessionState,
            health = health,
            onSignOut = onSignOut,
        )
        Text(
            "v-note",
            modifier = Modifier.weight(1f),
            style = MaterialTheme.typography.titleLarge,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        when (sessionState) {
            is SessionState.SignedIn ->
                IconButton(
                    modifier =
                        Modifier
                            .testTag("create-page-button")
                            .semantics { contentDescription = "New page" },
                    onClick = onCreatePage,
                ) {
                    Icon(Icons.Outlined.Add, contentDescription = null)
                }
            SessionState.Loading -> Unit
            // The library is reachable signed out; this is the way back in.
            SessionState.SignedOut, is SessionState.Error ->
                TextButton(onClick = onSignIn) { Text("Sign in") }
        }
    }
}

@Composable
private fun LibraryMenu(
    sessionState: SessionState,
    health: String,
    onSignOut: () -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    Box {
        IconButton(
            modifier =
                Modifier
                    .testTag("main-menu-button")
                    .semantics { contentDescription = "Open main menu" },
            onClick = { expanded = true },
        ) {
            Icon(Icons.Outlined.Menu, contentDescription = null)
        }
        DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            if (sessionState is SessionState.SignedIn) {
                MenuNote(sessionState.profile.email ?: sessionState.profile.sub)
            }
            MenuNote(health)
            if (sessionState is SessionState.SignedIn) {
                DropdownMenuItem(
                    text = { Text("Sign out") },
                    onClick = {
                        expanded = false
                        onSignOut()
                    },
                )
            }
        }
    }
}

// A read-only line in the menu — identity, server health.
@Composable
private fun MenuNote(text: String) {
    Text(
        text,
        modifier =
            Modifier
                .widthIn(max = 280.dp)
                .padding(horizontal = 16.dp, vertical = 10.dp),
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.secondary,
    )
}

@Composable
private fun LibraryBody(
    apiClient: ApiClient,
    sessionState: SessionState,
    pages: List<PageSummary>,
    libraryError: String?,
    onReload: () -> Unit,
    onOpenPage: (PageSummary) -> Unit,
    onRequestDelete: (PageSummary) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.fillMaxWidth().padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        libraryError?.let { StatusBanner(it, isError = true) }
        when (sessionState) {
            SessionState.Loading ->
                StatePanel(
                    eyebrow = "Session",
                    title = "Checking access",
                    body = "Loading your private v-note session.",
                )
            SessionState.SignedOut ->
                StatePanel(
                    eyebrow = "Private notes",
                    title = "Sign in to continue",
                    body = "Use Authentik to access your page library and ink canvas.",
                )
            is SessionState.Error -> {
                StatusBanner(sessionState.message, isError = true)
                Button(onClick = onReload) { Text("Retry") }
            }
            is SessionState.SignedIn ->
                if (pages.isEmpty()) {
                    StatePanel(
                        eyebrow = "No pages",
                        title = "Start with a blank ink page",
                        body = "Tap + to create a page, then write on it with the S Pen.",
                    )
                } else {
                    PageGrid(
                        apiClient = apiClient,
                        pages = pages,
                        thumbnailUnavailable = libraryError != null,
                        onOpenPage = onOpenPage,
                        onRequestDelete = onRequestDelete,
                        modifier = Modifier.weight(1f),
                    )
                }
        }
    }
}

@Composable
private fun PageGrid(
    apiClient: ApiClient,
    pages: List<PageSummary>,
    thumbnailUnavailable: Boolean,
    onOpenPage: (PageSummary) -> Unit,
    onRequestDelete: (PageSummary) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 210.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
        modifier = modifier,
    ) {
        items(pages, key = { it.id }) { page ->
            PageTile(
                apiClient = apiClient,
                page = page,
                thumbnailUnavailable = thumbnailUnavailable,
                onOpen = { onOpenPage(page) },
                onDelete = { onRequestDelete(page) },
            )
        }
    }
}

@Composable
private fun DeletePageDialog(
    page: PageSummary,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Delete page?") },
        text = { Text("This permanently removes ${page.displayTitle()}.") },
        confirmButton = { TextButton(onClick = onConfirm) { Text("Delete") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

// The `/health` probe reported in the menu. okhttp `execute()` is blocking, so
// it must run off the main thread or it throws `NetworkOnMainThreadException`
// before the request is even sent.
@Composable
private fun rememberServerHealth(): String {
    val health = remember { mutableStateOf("Checking server health…") }
    LaunchedEffect(Unit) {
        health.value =
            withContext(Dispatchers.IO) {
                runCatching {
                    val client = okhttp3.OkHttpClient()
                    val request =
                        okhttp3.Request
                            .Builder()
                            .url("${BuildConfig.BASE_URL}/health")
                            .get()
                            .build()
                    client.newCall(request).execute().use { response ->
                        if (!response.isSuccessful) {
                            "Health check failed: HTTP ${response.code}"
                        } else {
                            "Server healthy at ${BuildConfig.BASE_URL}"
                        }
                    }
                }.getOrElse { error ->
                    "Health check failed: ${error.message ?: error.javaClass.simpleName}"
                }
            }
    }
    return health.value
}
