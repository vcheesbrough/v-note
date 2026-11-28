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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.api.SessionExpiredException
import link.desync.vnote.api.hostOf
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.LocalPageSync
import link.desync.vnote.ink.PageCanvasScreen
import link.desync.vnote.ink.PageSyncHost
import link.desync.vnote.ink.Paper
import link.desync.vnote.ink.PaperPreferences
import link.desync.vnote.ink.normalizedSamsungSpenAction
import link.desync.vnote.library.LibraryLocal
import link.desync.vnote.library.LibraryStateHolder
import link.desync.vnote.library.ServerHealthProbe
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.net.Endpoints
import link.desync.vnote.store.LocalStore
import link.desync.vnote.sync.IdentityDecision
import link.desync.vnote.sync.SyncEngine
import link.desync.vnote.sync.SyncStatus
import link.desync.vnote.sync.decideIdentity
import link.desync.vnote.sync.resetInstallFor
import link.desync.vnote.telemetry.AppLog
import link.desync.vnote.ui.LocalSyncUi
import link.desync.vnote.ui.SyncActions
import link.desync.vnote.ui.SyncUi
import link.desync.vnote.ui.theme.VNoteTheme

class MainActivity : ComponentActivity() {
    companion object {
        private const val AUTH_COMPLETED_ACTION = "link.desync.vnote.AUTH_COMPLETED"
        private const val AUTH_CANCELED_ACTION = "link.desync.vnote.AUTH_CANCELED"
        private const val AUTH_COMPLETED_REQUEST_CODE = 100
        private const val AUTH_CANCELED_REQUEST_CODE = 101
        private const val TAG = "VNoteSession"

        @Volatile
        internal var apiClientFactory: ((TokenStore, AuthRepository) -> ApiClient)? = null

        // Tests shorten the `/health` retry backoff; null is the production one.
        @Volatile
        internal var healthRetryDelaysMillis: List<Long>? = null
    }

    private lateinit var tokenStore: TokenStore
    private lateinit var authRepository: AuthRepository
    private lateinit var apiClient: ApiClient
    private lateinit var library: LibraryStateHolder
    private lateinit var healthProbe: ServerHealthProbe

    // Local-first sync (#159): the device's copy, what the status icon shows,
    // and the engine that catches the device up with the server.
    private lateinit var store: LocalStore
    private lateinit var syncStatus: SyncStatus
    private lateinit var syncEngine: SyncEngine

    private val sessionState =
        androidx.compose.runtime.mutableStateOf<SessionState>(SessionState.Loading)

    // A sign-in as a different identity, waiting on the user because this
    // device still holds another account's unsynced changes (#159).
    private var identitySwitch by mutableStateOf<IdentitySwitch?>(null)

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
                ?: OkHttpApiClient(Endpoints.serverBaseUrl, tokenStore, authRepository)
        healthProbe = serverHealthProbe(apiClient)
        store = LocalStore.shared(applicationContext)
        syncStatus = SyncStatus(store)
        syncEngine = SyncEngine(apiClient, store, syncStatus, lifecycleScope)
        library =
            LibraryStateHolder(
                apiClient = apiClient,
                local =
                    LibraryLocal(
                        store = store,
                        status = syncStatus,
                        serverHost = hostOf(apiClient.baseUrl),
                        signInHost = apiClient.signInHost,
                        kick = syncEngine::kick,
                    ),
                reconnectScope = lifecycleScope,
                isSignedIn = { sessionState.value is SessionState.SignedIn },
                runOnUiThread = { block -> runOnUiThread { block() } },
            )
        // A page the server now holds after a create made here: as if its own
        // `page-created` had arrived.
        syncEngine.onPageCreated = { page -> library.applyLibraryEvent(LibraryEvent.PageCreated(page)) }
        // The status icon recounts whenever the store changes.
        lifecycleScope.launch { store.changes.collect { syncStatus.refresh() } }

        val syncUi =
            SyncUi(
                syncStatus,
                SyncActions(onSignIn = ::signIn, onKeepAsNewPage = ::keepAsNewPage, onDiscard = ::discardRefused),
            )
        val pageSync = PageSyncHost(store, syncStatus, syncEngine)
        setContent {
            VNoteTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    CompositionLocalProvider(LocalSyncUi provides syncUi, LocalPageSync provides pageSync) {
                        AppRoot { CurrentScreen() }
                        identitySwitch?.let { IdentitySwitchDialog(it) }
                    }
                }
            }
        }

        if (!handleAuthorizationIntent(intent)) {
            reloadSession()
        }
    }

    // Which screen fills the surface: an open page, the bin (#631), or the
    // library (which is also the signed-out screen).
    @Composable
    private fun CurrentScreen() {
        val session = sessionState.value
        val selectedPage = library.selectedPage
        if (session is SessionState.SignedIn && selectedPage != null) {
            // An open page takes over the whole surface — the infinite ink canvas.
            PageCanvasScreen(
                apiClient = apiClient,
                page = selectedPage,
                userId = session.profile.sub,
                onBack = { library.closePage() },
                onSignIn = {
                    library.closePage()
                    signIn()
                },
            )
        } else if (session is SessionState.SignedIn && library.bin.showing) {
            BinScreen(
                apiClient = apiClient,
                pages = library.bin.pages,
                error = library.error,
                onBack = library.bin::close,
                onRestore = library.bin::restore,
                onDeletePermanently = library.bin::deletePermanently,
                onEmptyBin = library.bin::empty,
            )
        } else {
            AppScreen(
                apiClient = apiClient,
                healthProbe = healthProbe,
                sessionState = session,
                onSignIn = { signIn() },
                onSignOut = { signOut() },
                onReload = { reloadSession() },
                pages = library.pages,
                libraryError = library.error,
                onCreatePage = { createPage() },
                onOpenPage = { library.openPage(it) },
                onDeletePage = { library.deletePage(it) },
                onOpenBin = library.bin::open,
            )
        }
    }

    private fun serverHealthProbe(client: ApiClient): ServerHealthProbe =
        ServerHealthProbe(
            baseUrl = client.baseUrl,
            check = client::checkHealth,
            retryDelaysMillis = healthRetryDelaysMillis ?: ServerHealthProbe.DEFAULT_RETRY_DELAYS_MILLIS,
        )

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleAuthorizationIntent(intent)
    }

    override fun onDestroy() {
        syncEngine.stop()
        library.close()
        authRepository.shutdown()
        super.onDestroy()
    }

    private fun signIn() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            // Signed in from the device with sync paused (#159): the library
            // stays on screen while the browser signs in again.
            if (sessionState.value !is SessionState.SignedIn) sessionState.value = SessionState.Loading
            runCatching {
                authRepository.beginLogin(
                    completedIntent = authPendingIntent(AUTH_COMPLETED_ACTION, AUTH_COMPLETED_REQUEST_CODE),
                    canceledIntent = authPendingIntent(AUTH_CANCELED_ACTION, AUTH_CANCELED_REQUEST_CODE),
                )
            }.onFailure { error ->
                AppLog.w(TAG, "sign in could not start", error)
                if (sessionState.value !is SessionState.SignedIn) {
                    sessionState.value = SessionState.Error(error.message ?: "Unable to start sign in")
                }
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
            AppLog.i(TAG, "sign in canceled")
            if (sessionState.value !is SessionState.SignedIn) sessionState.value = SessionState.Error("Sign in was canceled")
            return true
        }

        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            // The tokens before this sign-in, so backing out of an identity
            // switch puts them back and changes nothing (#159).
            val previousTokens = tokenStore.snapshot()
            authRepository.handleAuthorizationResponse(intent).fold(
                onSuccess = { adoptSignIn(previousTokens) },
                onFailure = { error ->
                    AppLog.w(TAG, "sign in failed", error)
                    if (sessionState.value !is SessionState.SignedIn) {
                        sessionState.value = SessionState.Error(error.message ?: "Sign in failed")
                    }
                },
            )
        }
        return true
    }

    // An explicit sign-out keeps every local record (#159): signing in again
    // as the same identity carries on where it left off. Only the sign-in
    // screen comes back, and sync stops until then.
    private fun signOut() {
        AppLog.i(TAG, "signed out")
        val endSessionIntent = authRepository.createEndSessionIntent()
        syncEngine.stop()
        library.clear()
        store.identity.markSignedOut()
        authRepository.signOutLocal()
        syncStatus.requireSignIn()
        sessionState.value = SessionState.SignedOut
        endSessionIntent?.data?.let { uri ->
            CustomTabsIntent.Builder().build().launchUrl(this, uri)
        }
    }

    // Start signed in from the device (#159): the stored identity opens the
    // library straight away, online or not, and `/me` runs in the background
    // as one more sync step. The sign-in screen is only for first use and
    // after an explicit sign-out.
    private fun reloadSession() {
        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            val stored = store.identity.current()
            if (stored != null && !store.identity.signedOut()) {
                enterSignedIn(stored)
                checkProfileInBackground(stored)
                return@launch
            }
            sessionState.value = SessionState.Loading
            if (!tokenStore.hasSession()) {
                sessionState.value = SessionState.SignedOut
                return@launch
            }
            // Tokens and no stored identity: an install from before #159, or
            // a sign-out followed by a sign-in. The server says who this is.
            apiClient.fetchMe().fold(
                onSuccess = { profile -> adoptIdentity(profile, previousTokens = null) },
                onFailure = { error ->
                    // Only a refused session signs the user out (#150). A
                    // failing or unreachable server keeps the stored tokens,
                    // so Retry picks up where the user was once it is back.
                    if (error is SessionExpiredException) {
                        AppLog.w(TAG, "session check refused; signing out locally", error)
                        authRepository.signOutLocal()
                    } else {
                        AppLog.w(TAG, "session check failed; keeping the session", error)
                    }
                    sessionState.value = SessionState.Error(error.message ?: "Session check failed")
                },
            )
        }
    }

    // A sign-in completed: who is it?
    private suspend fun adoptSignIn(previousTokens: TokenStore.Snapshot) {
        apiClient.fetchMe().fold(
            onSuccess = { profile -> adoptIdentity(profile, previousTokens) },
            onFailure = { error ->
                AppLog.w(TAG, "profile check after sign in failed", error)
                if (sessionState.value !is SessionState.SignedIn) {
                    sessionState.value = SessionState.Error(error.message ?: "Session check failed")
                }
            },
        )
    }

    // One identity per install (#159). The same identity carries on with
    // everything kept; a different one resets the install — silently when
    // nothing is owed to the server, after a warning when something is.
    @androidx.annotation.VisibleForTesting
    internal fun adoptIdentity(
        profile: MeProfile,
        previousTokens: TokenStore.Snapshot?,
    ) {
        when (val decision = decideIdentity(store, profile)) {
            IdentityDecision.Continue -> {
                store.identity.save(profile)
                enterSignedIn(profile)
            }
            IdentityDecision.Reset -> switchIdentity(profile)
            is IdentityDecision.Warn -> {
                AppLog.w(TAG, "a different identity signed in with unsynced changes on the device: count=${decision.unsynced}")
                // The new identity's tokens are already stored: nothing may
                // sync with them against the old identity's data while the
                // user decides. Cancel's reload starts everything again.
                syncEngine.stop()
                library.clear()
                identitySwitch = IdentitySwitch(profile, decision.unsynced, previousTokens)
            }
        }
    }

    // The library, signed in as [profile], with sync running if there is a
    // token to sync with and paused for a sign-in if not.
    private fun enterSignedIn(profile: MeProfile) {
        sessionState.value = SessionState.SignedIn(profile)
        if (tokenStore.hasSession()) syncStatus.signedIn() else syncStatus.requireSignIn()
        library.start()
        syncEngine.start()
    }

    // `/me` as a sync step: refreshes the email shown, and learns of an
    // expired session. Never signs out and never blocks the library.
    private suspend fun checkProfileInBackground(stored: MeProfile) {
        if (!tokenStore.hasSession()) return
        apiClient.fetchMe().fold(
            onSuccess = { profile -> if (profile.sub == stored.sub) store.identity.save(profile) },
            onFailure = { error -> if (error is SessionExpiredException) syncStatus.requireSignIn() },
        )
    }

    private fun switchIdentity(profile: MeProfile) {
        AppLog.i(TAG, "a different identity signed in; resetting the install")
        identitySwitch = null
        syncEngine.stop()
        library.clear()
        resetInstallFor(store, profile)
        enterSignedIn(profile)
    }

    // Cancel changes nothing: the new sign-in is undone, its tokens replaced
    // by the ones from before, and the previous account's data kept.
    private fun cancelIdentitySwitch(switch: IdentitySwitch) {
        AppLog.i(TAG, "identity switch canceled; keeping the device's data")
        identitySwitch = null
        switch.previousTokens?.let(tokenStore::restore) ?: authRepository.signOutLocal()
        reloadSession()
    }

    @Composable
    private fun IdentitySwitchDialog(switch: IdentitySwitch) {
        AlertDialog(
            onDismissRequest = { cancelIdentitySwitch(switch) },
            modifier = Modifier.testTag("identity-switch-dialog"),
            title = { Text("Different account") },
            text = {
                val what = if (switch.unsynced == 1) "1 unsynced change" else "${switch.unsynced} unsynced changes"
                Text("$what from another account will be deleted.")
            },
            confirmButton = {
                TextButton(
                    onClick = { switchIdentity(switch.profile) },
                    modifier = Modifier.testTag("identity-switch-delete"),
                ) { Text("Delete and continue") }
            },
            dismissButton = {
                TextButton(
                    onClick = { cancelIdentitySwitch(switch) },
                    modifier = Modifier.testTag("identity-switch-cancel"),
                ) { Text("Cancel") }
            },
        )
    }

    // A refused page's strokes, kept as a new page and opened (#159).
    private fun keepAsNewPage(pageId: String) {
        val madeHere = store.library.isLocalOnly(pageId)
        val page = syncEngine.recovery.keepAsNewPage(pageId)
        // A refused page that only ever existed here goes with its refusal.
        if (madeHere) library.applyLibraryEvent(LibraryEvent.PageDeleted(pageId))
        library.applyLibraryEvent(LibraryEvent.PageCreated(page))
        library.openPage(page)
        syncEngine.kick()
    }

    private fun discardRefused(pageId: String) {
        val madeHere = store.library.isLocalOnly(pageId)
        syncEngine.recovery.discard(pageId)
        if (library.selectedPage?.id == pageId) library.closePage()
        if (madeHere) library.applyLibraryEvent(LibraryEvent.PageDeleted(pageId))
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
        library.createPage(paper)
    }
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
