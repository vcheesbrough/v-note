package link.desync.vnote

import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.view.InputDevice
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.browser.customtabs.CustomTabsIntent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.PageCanvasScreen
import link.desync.vnote.ink.Paper
import link.desync.vnote.ink.PaperPreferences
import link.desync.vnote.ink.normalizedSamsungSpenAction
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
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
                ?: OkHttpApiClient(BuildConfig.BASE_URL, tokenStore, authRepository)

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
            }.onFailure { error ->
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
        // New pages inherit the last paper this user picked on this device. See
        // `PaperPreferences` — a deliberate interim store, tracked in #260.
        val session = sessionState.value
        val paper =
            if (session is SessionState.SignedIn) {
                PaperPreferences(this, session.profile.sub).load()
            } else {
                Paper.None
            }
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            apiClient.createPage(paper = paper).fold(
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
                                    pagesState.value =
                                        pagesState.value.map { page ->
                                            if (page.id == event.pageId && event.thumbnail.sourceSeq() >= page.thumbnail.sourceSeq()) {
                                                page.copy(thumbnail = event.thumbnail)
                                            } else {
                                                page
                                            }
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
                                                    } else {
                                                        page
                                                    }
                                                }.sortedByDescending { it.updatedAt }
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
