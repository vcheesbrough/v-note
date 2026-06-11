package link.desync.vnote

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.browser.customtabs.CustomTabsIntent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
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
import link.desync.vnote.ui.theme.VNoteTheme
import okhttp3.WebSocket

class MainActivity : ComponentActivity() {
    private lateinit var tokenStore: TokenStore
    private lateinit var authRepository: AuthRepository
    private lateinit var apiClient: ApiClient
    private var librarySocket: WebSocket? = null

    private val authLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            val scope = kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main)
            scope.launch {
                authRepository.handleAuthorizationResponse(result.data).fold(
                    onSuccess = { reloadSession() },
                    onFailure = { error ->
                        sessionState.value =
                            SessionState.Error(error.message ?: "Sign in failed")
                    },
                )
            }
        }

    private val sessionState =
        androidx.compose.runtime.mutableStateOf<SessionState>(SessionState.Loading)
    private val pagesState = androidx.compose.runtime.mutableStateOf<List<PageSummary>>(emptyList())
    private val selectedPageState = androidx.compose.runtime.mutableStateOf<PageSummary?>(null)
    private val libraryErrorState = androidx.compose.runtime.mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        tokenStore = TokenStore(applicationContext)
        val authConfig = AuthConfig.fromBuildConfig()
        authRepository = AuthRepository(applicationContext, authConfig, tokenStore)
        apiClient = ApiClient(BuildConfig.BASE_URL, tokenStore, authRepository)

        setContent {
            VNoteTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    AppScreen(
                        sessionState = sessionState.value,
                        onSignIn = { signIn() },
                        onSignOut = { signOut() },
                        onReload = { reloadSession() },
                        pages = pagesState.value,
                        selectedPage = selectedPageState.value,
                        libraryError = libraryErrorState.value,
                        onCreatePage = { createPage() },
                        onOpenPage = { selectedPageState.value = it },
                        onDeletePage = { deletePage(it) },
                    )
                }
            }
        }

        reloadSession()
    }

    override fun onDestroy() {
        librarySocket?.close(1000, "activity destroyed")
        authRepository.shutdown()
        super.onDestroy()
    }

    private fun signIn() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            sessionState.value = SessionState.Loading
            runCatching { authRepository.beginLogin(authLauncher) }
                .onFailure { error ->
                    sessionState.value =
                        SessionState.Error(error.message ?: "Unable to start sign in")
                }
        }
    }

    private fun signOut() {
        val endSessionIntent = authRepository.createEndSessionIntent()
        librarySocket?.close(1000, "signed out")
        librarySocket = null
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

    private fun connectLibrarySocket() {
        librarySocket?.close(1000, "reconnecting")
        librarySocket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onEvent(event: LibraryEvent) {
                        runOnUiThread {
                            when (event) {
                                is LibraryEvent.PageCreated -> upsertPage(event.page)
                                is LibraryEvent.PageDeleted -> removePage(event.pageId)
                            }
                            libraryErrorState.value = null
                        }
                    }

                    override fun onError(message: String) {
                        runOnUiThread { libraryErrorState.value = message }
                    }

                    override fun onClosed() {
                        runOnUiThread { libraryErrorState.value = "Realtime disconnected" }
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

private sealed interface SessionState {
    data object Loading : SessionState

    data object SignedOut : SessionState

    data class SignedIn(val profile: MeProfile) : SessionState

    data class Error(val message: String) : SessionState
}

@Composable
private fun AppScreen(
    sessionState: SessionState,
    onSignIn: () -> Unit,
    onSignOut: () -> Unit,
    onReload: () -> Unit,
    pages: List<PageSummary>,
    selectedPage: PageSummary?,
    libraryError: String?,
    onCreatePage: () -> Unit,
    onOpenPage: (PageSummary) -> Unit,
    onDeletePage: (PageSummary) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val healthState = remember { mutableStateOf("Checking server health…") }

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

    Box(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("v-note", style = MaterialTheme.typography.headlineMedium)
            Text(healthState.value, style = MaterialTheme.typography.bodyMedium)

            when (sessionState) {
                SessionState.Loading -> Text("Checking session…")
                SessionState.SignedOut -> {
                    Text("Sign in with Authentik to use v-note on this device.")
                    Button(onClick = onSignIn) { Text("Sign in") }
                }
                is SessionState.SignedIn -> {
                    val label = sessionState.profile.email ?: sessionState.profile.sub
                    Text("Signed in as $label", style = MaterialTheme.typography.bodyLarge)
                    Button(onClick = onCreatePage) { Text("New page") }
                    libraryError?.let { Text(it, style = MaterialTheme.typography.bodyMedium) }
                    if (pages.isEmpty()) {
                        Text("No pages yet.")
                    } else {
                        pages.forEach { page ->
                            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                Button(onClick = { onOpenPage(page) }) { Text(page.title) }
                                Text("updated ${page.updatedAt}")
                                Button(onClick = { onDeletePage(page) }) { Text("Delete") }
                            }
                        }
                    }
                    selectedPage?.let { page ->
                        Text("Open page: ${page.title}", style = MaterialTheme.typography.titleMedium)
                        Text("Empty canvas placeholder. Ink capture lands in the next iteration.")
                    }
                    Button(onClick = onSignOut) { Text("Sign out") }
                }
                is SessionState.Error -> {
                    Text(sessionState.message, style = MaterialTheme.typography.bodyMedium)
                    Button(onClick = { scope.launch { onReload() } }) { Text("Retry") }
                    Button(onClick = onSignIn) { Text("Sign in") }
                }
            }
        }

        // Version watermark — persistent build identity (release · flavor) for lockstep checks.
        Text(
            text = "v${BuildConfig.VERSION_NAME} · ${BuildConfig.FLAVOR}",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.38f),
            modifier =
                Modifier
                    .align(Alignment.BottomEnd)
                    .padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}
