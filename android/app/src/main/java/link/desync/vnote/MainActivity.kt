package link.desync.vnote

import android.app.PendingIntent
import android.content.Intent
import android.graphics.BitmapFactory
import android.os.Build
import android.os.Bundle
import android.view.InputDevice
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.browser.customtabs.CustomTabsIntent
import androidx.lifecycle.lifecycleScope
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.produceState
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.LibraryEvent
import link.desync.vnote.auth.LibraryEventListener
import link.desync.vnote.auth.MeProfile
import link.desync.vnote.auth.PageSummary
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.auth.ThumbnailMetadata
import link.desync.vnote.ink.PageCanvasScreen
import link.desync.vnote.ink.normalizedSamsungSpenAction
import link.desync.vnote.ui.displayTitle
import link.desync.vnote.ui.displayUpdatedAge
import link.desync.vnote.ui.hasDisplayTitle
import link.desync.vnote.ui.theme.VNoteTheme
import okhttp3.WebSocket

class MainActivity : ComponentActivity() {
    companion object {
        private const val AUTH_COMPLETED_ACTION = "link.desync.vnote.AUTH_COMPLETED"
        private const val AUTH_CANCELED_ACTION = "link.desync.vnote.AUTH_CANCELED"
        private const val AUTH_COMPLETED_REQUEST_CODE = 100
        private const val AUTH_CANCELED_REQUEST_CODE = 101

        @Volatile
        internal var apiClientFactory: ((TokenStore, AuthRepository) -> ApiClient)? = null
    }

    private lateinit var tokenStore: TokenStore
    private lateinit var authRepository: AuthRepository
    private lateinit var apiClient: ApiClient
    private var librarySocket: WebSocket? = null
    private var libraryConnectionGeneration = 0

    private val sessionState =
        androidx.compose.runtime.mutableStateOf<SessionState>(SessionState.Loading)
    private val pagesState = androidx.compose.runtime.mutableStateOf<List<PageSummary>>(emptyList())
    private val selectedPageState = androidx.compose.runtime.mutableStateOf<PageSummary?>(null)
    private val libraryErrorState = androidx.compose.runtime.mutableStateOf<String?>(null)

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        val normalizedAction = normalizedSamsungSpenAction(event.actionMasked)
        if (normalizedAction == event.actionMasked || !event.isFromSource(InputDevice.SOURCE_STYLUS)) {
            return super.dispatchTouchEvent(event)
        }

        val normalizedEvent = MotionEvent.obtain(event)
        normalizedEvent.action = normalizedAction
        return try {
            super.dispatchTouchEvent(normalizedEvent)
        } finally {
            normalizedEvent.recycle()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        tokenStore = TokenStore(applicationContext)
        val authConfig = AuthConfig.fromBuildConfig()
        authRepository = AuthRepository(applicationContext, authConfig, tokenStore)
        apiClient =
            apiClientFactory?.invoke(tokenStore, authRepository)
                ?: ApiClient(BuildConfig.BASE_URL, tokenStore, authRepository)

        setContent {
            VNoteTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    AppRoot {
                        val session = sessionState.value
                        val selectedPage = selectedPageState.value
                        if (session is SessionState.SignedIn && selectedPage != null) {
                            // An open page takes over the whole surface — the infinite ink canvas.
                            PageCanvasScreen(
                                apiClient = apiClient,
                                page = selectedPage,
                                userId = session.profile.sub,
                                onBack = { closePage() },
                            )
                        } else {
                            AppScreen(
                                apiClient = apiClient,
                                sessionState = session,
                                onSignIn = { signIn() },
                                onSignOut = { signOut() },
                                onReload = { reloadSession() },
                                pages = pagesState.value,
                                libraryError = libraryErrorState.value,
                                onCreatePage = { createPage() },
                                onOpenPage = { selectedPageState.value = it },
                                onDeletePage = { deletePage(it) },
                            )
                        }
                    }
                }
            }
        }

        if (!handleAuthorizationIntent(intent)) {
            reloadSession()
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleAuthorizationIntent(intent)
    }

    override fun onDestroy() {
        librarySocket?.close(1000, "activity destroyed")
        authRepository.shutdown()
        super.onDestroy()
    }

    private fun signIn() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            sessionState.value = SessionState.Loading
            runCatching {
                authRepository.beginLogin(
                    completedIntent = authPendingIntent(AUTH_COMPLETED_ACTION, AUTH_COMPLETED_REQUEST_CODE),
                    canceledIntent = authPendingIntent(AUTH_CANCELED_ACTION, AUTH_CANCELED_REQUEST_CODE),
                )
            }
                .onFailure { error ->
                    sessionState.value =
                        SessionState.Error(error.message ?: "Unable to start sign in")
                }
        }
    }

    private fun authPendingIntent(
        action: String,
        requestCode: Int,
    ): PendingIntent {
        val intent =
            Intent(this, MainActivity::class.java)
                .setAction(action)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        return PendingIntent.getActivity(
            this,
            requestCode,
            intent,
            authPendingIntentFlags(),
        )
    }

    private fun authPendingIntentFlags(): Int {
        val mutabilityFlag =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_MUTABLE
            } else {
                0
            }
        return PendingIntent.FLAG_UPDATE_CURRENT or mutabilityFlag
    }

    private fun handleAuthorizationIntent(intent: Intent?): Boolean {
        val action = intent?.action
        if (action != AUTH_COMPLETED_ACTION && action != AUTH_CANCELED_ACTION) {
            return false
        }
        if (action == AUTH_CANCELED_ACTION) {
            sessionState.value = SessionState.Error("Sign in was canceled")
            return true
        }

        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            authRepository.handleAuthorizationResponse(intent).fold(
                onSuccess = { reloadSession() },
                onFailure = { error ->
                    sessionState.value =
                        SessionState.Error(error.message ?: "Sign in failed")
                },
            )
        }
        return true
    }

    private fun signOut() {
        val endSessionIntent = authRepository.createEndSessionIntent()
        librarySocket?.close(1000, "signed out")
        librarySocket = null
        libraryConnectionGeneration += 1
        pagesState.value = emptyList()
        selectedPageState.value = null
        authRepository.signOutLocal()
        sessionState.value = SessionState.SignedOut
        endSessionIntent?.data?.let { uri ->
            CustomTabsIntent.Builder().build().launchUrl(this, uri)
        }
    }

    private fun reloadSession() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            sessionState.value = SessionState.Loading
            if (!tokenStore.hasSession()) {
                sessionState.value = SessionState.SignedOut
                return@launch
            }
            authRepository.refreshAccessTokenIfNeeded()
            apiClient.fetchMe().fold(
                onSuccess = { profile ->
                    sessionState.value = SessionState.SignedIn(profile)
                    loadPages()
                    connectLibrarySocket()
                },
                onFailure = { error ->
                    authRepository.signOutLocal()
                    sessionState.value =
                        SessionState.Error(error.message ?: "Session check failed")
                },
            )
        }
    }

    private fun loadPages() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            apiClient.listPages().fold(
                onSuccess = { pages ->
                    pagesState.value = pages
                    libraryErrorState.value = null
                },
                onFailure = { error ->
                    libraryErrorState.value = error.message ?: "Loading pages failed"
                },
            )
        }
    }

    private fun createPage() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            apiClient.createPage().fold(
                onSuccess = { page ->
                    upsertPage(page)
                    selectedPageState.value = page
                    libraryErrorState.value = null
                },
                onFailure = { error ->
                    libraryErrorState.value = error.message ?: "Creating page failed"
                },
            )
        }
    }

    private fun deletePage(page: PageSummary) {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            apiClient.deletePage(page.id).fold(
                onSuccess = {
                    removePage(page.id)
                    libraryErrorState.value = null
                },
                onFailure = { error ->
                    libraryErrorState.value = error.message ?: "Deleting page failed"
                },
            )
        }
    }

    private fun closePage() {
        selectedPageState.value = null
        loadPages()
    }

    private fun connectLibrarySocket() {
        val generation = ++libraryConnectionGeneration
        librarySocket?.close(1000, "reconnecting")
        librarySocket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onEvent(event: LibraryEvent) {
                        runOnUiThread {
                            when (event) {
                                is LibraryEvent.PageCreated -> upsertPage(event.page)
                                is LibraryEvent.PageDeleted -> removePage(event.pageId)
                                is LibraryEvent.PageThumbnailUpdated -> {
                                    pagesState.value = pagesState.value.map { page ->
                                        if (page.id == event.pageId && event.thumbnail.sourceSeq() >= page.thumbnail.sourceSeq()) {
                                            page.copy(thumbnail = event.thumbnail)
                                        } else page
                                    }
                                }
                                is LibraryEvent.PageUpdated -> {
                                    val current = pagesState.value.firstOrNull { it.id == event.pageId }
                                    // Ignore a stale/duplicate timestamp so re-sort stays idempotent.
                                    if (current != null && event.updatedAt > current.updatedAt) {
                                        pagesState.value =
                                            pagesState.value
                                                .map { page ->
                                                    if (page.id == event.pageId) {
                                                        page.copy(updatedAt = event.updatedAt)
                                                    } else page
                                                }
                                                .sortedByDescending { it.updatedAt }
                                    }
                                }
                            }
                            libraryErrorState.value = null
                        }
                    }

                    override fun onError(message: String) {
                        runOnUiThread { libraryErrorState.value = message }
                    }

                    override fun onClosed() {
                        if (generation != libraryConnectionGeneration || sessionState.value !is SessionState.SignedIn) return
                        runOnUiThread { libraryErrorState.value = "Realtime disconnected" }
                        lifecycleScope.launch {
                            delay(1_000)
                            if (generation != libraryConnectionGeneration || sessionState.value !is SessionState.SignedIn) return@launch
                            loadPages()
                            connectLibrarySocket()
                        }
                    }
                },
            )
    }

    private fun upsertPage(page: PageSummary) {
        pagesState.value =
            (pagesState.value.filterNot { it.id == page.id } + page)
                .sortedByDescending { it.updatedAt }
    }

    private fun removePage(pageId: String) {
        pagesState.value = pagesState.value.filterNot { it.id == pageId }
        if (selectedPageState.value?.id == pageId) {
            selectedPageState.value = null
        }
    }
}

private fun ThumbnailMetadata.sourceSeq(): Long =
    when (this) {
        ThumbnailMetadata.Empty -> 0
        is ThumbnailMetadata.Generating -> sourceSeq
        is ThumbnailMetadata.Available -> sourceSeq
        is ThumbnailMetadata.Failed -> sourceSeq
    }

private sealed interface SessionState {
    data object Loading : SessionState

    data object SignedOut : SessionState

    data class SignedIn(val profile: MeProfile) : SessionState

    data class Error(val message: String) : SessionState
}

@Composable
private fun AppRoot(content: @Composable () -> Unit) {
    Box(modifier = Modifier.fillMaxSize()) {
        content()
        Text(
            text = "v${BuildConfig.VERSION_NAME} · ${BuildConfig.FLAVOR}",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.38f),
            modifier =
                Modifier
                    .align(Alignment.BottomEnd)
                    .testTag("version-watermark")
                    .padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}

@Composable
private fun AppScreen(
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
    val scope = rememberCoroutineScope()
    val healthState = remember { mutableStateOf("Checking server health…") }
    var pendingDelete by remember { mutableStateOf<PageSummary?>(null) }

    LaunchedEffect(Unit) {
        // okhttp execute() is blocking — must run off the main thread or it throws
        // NetworkOnMainThreadException before the request is even sent.
        healthState.value =
            withContext(Dispatchers.IO) {
                runCatching {
                    val client = okhttp3.OkHttpClient()
                    val request =
                        okhttp3.Request.Builder()
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

    pendingDelete?.let { page ->
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text("Delete page?") },
            text = { Text("This permanently removes ${page.displayTitle()}.") },
            confirmButton = {
                TextButton(
                    onClick = {
                        pendingDelete = null
                        onDeletePage(page)
                    },
                ) {
                    Text("Delete")
                }
            },
            dismissButton = {
                TextButton(onClick = { pendingDelete = null }) {
                    Text("Cancel")
                }
            },
        )
    }

    Box(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier.fillMaxSize().padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("v-note", style = MaterialTheme.typography.headlineMedium)
                if (sessionState is SessionState.SignedIn) {
                    val label = sessionState.profile.email ?: sessionState.profile.sub
                    var accountMenuExpanded by remember { mutableStateOf(false) }
                    Box {
                        IconButton(
                            modifier =
                                Modifier
                                    .testTag("account-menu-button")
                                    .semantics { contentDescription = "Open account menu" },
                            onClick = { accountMenuExpanded = true },
                        ) {
                            Text("☰", style = MaterialTheme.typography.headlineSmall)
                        }
                        DropdownMenu(
                            expanded = accountMenuExpanded,
                            onDismissRequest = { accountMenuExpanded = false },
                        ) {
                            Text(
                                label,
                                modifier =
                                    Modifier
                                        .widthIn(max = 280.dp)
                                        .padding(horizontal = 16.dp, vertical = 10.dp),
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.secondary,
                            )
                            Text(
                                healthState.value,
                                modifier =
                                    Modifier
                                        .widthIn(max = 280.dp)
                                        .padding(horizontal = 16.dp, vertical = 10.dp),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.secondary,
                            )
                            DropdownMenuItem(
                                text = { Text("Sign out") },
                                onClick = {
                                    accountMenuExpanded = false
                                    onSignOut()
                                },
                            )
                        }
                    }
                }
            }

            when (sessionState) {
                SessionState.Loading ->
                    StatePanel(
                        eyebrow = "Session",
                        title = "Checking access",
                        body = "Loading your private v-note session.",
                    )
                SessionState.SignedOut -> {
                    StatePanel(
                        eyebrow = "Private notes",
                        title = "Sign in to continue",
                        body = "Use Authentik to access your page library and ink canvas.",
                    )
                    Button(onClick = onSignIn) { Text("Sign in") }
                }
                is SessionState.SignedIn -> {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.End,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Button(onClick = onCreatePage) { Text("New page") }
                    }
                    libraryError?.let { StatusBanner(it, isError = true) }
                    if (pages.isEmpty()) {
                        StatePanel(
                            eyebrow = "No pages",
                            title = "Start with a blank ink page",
                            body = "Create a page, then write with the S Pen.",
                        )
                    } else {
                        LazyVerticalGrid(
                            columns = GridCells.Adaptive(minSize = 210.dp),
                            horizontalArrangement = Arrangement.spacedBy(10.dp),
                            verticalArrangement = Arrangement.spacedBy(10.dp),
                            modifier = Modifier.weight(1f),
                        ) {
                            items(pages, key = { it.id }) { page ->
                                PageTile(
                                    apiClient = apiClient,
                                    page = page,
                                    thumbnailUnavailable = libraryError != null,
                                    onOpen = { onOpenPage(page) },
                                    onDelete = { pendingDelete = page },
                                )
                            }
                        }
                    }
                }
                is SessionState.Error -> {
                    StatusBanner(sessionState.message, isError = true)
                    Button(onClick = { scope.launch { onReload() } }) { Text("Retry") }
                    OutlinedButton(onClick = onSignIn) { Text("Sign in") }
                }
            }
        }
    }
}

@Composable
private fun StatePanel(
    eyebrow: String,
    title: String,
    body: String,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        shape = RoundedCornerShape(8.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(eyebrow, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.secondary)
            Text(title, style = MaterialTheme.typography.titleLarge)
            Text(body, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.secondary)
        }
    }
}

@Composable
private fun StatusBanner(
    message: String,
    isError: Boolean = false,
) {
    val color = if (isError) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.secondary
    Surface(
        color = if (isError) Color(0xFFF5E3DF) else MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(7.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            message,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 9.dp),
            style = MaterialTheme.typography.bodySmall,
            color = color,
        )
    }
}

@Composable
private fun PageTile(
    apiClient: ApiClient,
    page: PageSummary,
    thumbnailUnavailable: Boolean,
    onOpen: () -> Unit,
    onDelete: () -> Unit,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        border = BorderStroke(1.dp, Color(0xFFDDE2DB)),
        shape = RoundedCornerShape(8.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.fillMaxWidth(),
        ) {
            Box(
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .aspectRatio(1.5f)
                        .testTag("page-tile-${page.id}")
                        .clickable(onClick = onOpen),
            ) {
                PagePreview(
                    apiClient = apiClient,
                    thumbnail = page.thumbnail,
                    unavailable = thumbnailUnavailable,
                    modifier = Modifier.fillMaxSize(),
                )
                if (page.hasDisplayTitle()) {
                    Surface(
                        color = Color.White.copy(alpha = 0.92f),
                        shape = RoundedCornerShape(4.dp),
                        modifier = Modifier.align(Alignment.TopStart).padding(8.dp).widthIn(max = 180.dp),
                    ) {
                        Text(
                            page.title,
                            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                            style = MaterialTheme.typography.bodyMedium,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 10.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    page.displayUpdatedAge(),
                    modifier = Modifier.weight(1f),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.secondary,
                )
                IconButton(
                    modifier =
                        Modifier
                            .size(32.dp)
                            .semantics {
                                contentDescription = if (page.hasDisplayTitle()) "Delete ${page.title}" else "Delete page"
                            },
                    onClick = onDelete,
                ) {
                    Text(
                        "×",
                        style = MaterialTheme.typography.titleMedium,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        }
    }
}

@Composable
private fun PagePreview(
    apiClient: ApiClient,
    thumbnail: ThumbnailMetadata,
    unavailable: Boolean,
    modifier: Modifier = Modifier,
) {
    val bitmap by produceState<android.graphics.Bitmap?>(initialValue = null, key1 = thumbnail, key2 = unavailable) {
        value = if (!unavailable && thumbnail is ThumbnailMetadata.Available) {
            apiClient.fetchThumbnail(thumbnail.url).getOrNull()?.let { bytes ->
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
            }
        } else null
    }
    Box(
        modifier =
            modifier
                .drawBehind {
                    drawRect(if (unavailable || thumbnail is ThumbnailMetadata.Failed) Color(0xFFF5E3DF) else Color.White)
                    val step = 8.dp.toPx()
                    var y = step
                    while (y < size.height) {
                        drawLine(
                            color = Color(0xFFE2E7E1),
                            start = Offset(6.dp.toPx(), y),
                            end = Offset(size.width - 6.dp.toPx(), y),
                            strokeWidth = 1.dp.toPx(),
                        )
                        y += step
                    }
                    drawLine(
                        color = Color(0xFFD2D9D1),
                        start = Offset.Zero,
                        end = Offset(size.width, 0f),
                        strokeWidth = 1.dp.toPx(),
                    )
                    drawLine(
                        color = Color(0xFFD7DDD5),
                        start = Offset(0f, size.height),
                        end = Offset(size.width, size.height),
                        strokeWidth = 1.dp.toPx(),
                    )
                },
        contentAlignment = Alignment.Center,
    ) {
        if (bitmap != null) {
            Image(bitmap = bitmap!!.asImageBitmap(), contentDescription = null, modifier = Modifier.fillMaxSize())
        } else if (thumbnail is ThumbnailMetadata.Generating) {
            Text("...", style = MaterialTheme.typography.bodySmall, color = Color(0xFF5F6B62))
        }
    }
}
