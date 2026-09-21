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
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.PageCanvasScreen
import link.desync.vnote.ink.Paper
import link.desync.vnote.ink.PaperPreferences
import link.desync.vnote.ink.normalizedSamsungSpenAction
import link.desync.vnote.library.LibraryStateHolder
import link.desync.vnote.telemetry.AppLog
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
    }

    private lateinit var tokenStore: TokenStore
    private lateinit var authRepository: AuthRepository
    private lateinit var apiClient: ApiClient
    private lateinit var library: LibraryStateHolder

    private val sessionState =
        androidx.compose.runtime.mutableStateOf<SessionState>(SessionState.Loading)

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
        library =
            LibraryStateHolder(
                apiClient = apiClient,
                reconnectScope = lifecycleScope,
                isSignedIn = { sessionState.value is SessionState.SignedIn },
                runOnUiThread = { block -> runOnUiThread { block() } },
            )

        setContent {
            VNoteTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    AppRoot {
                        val session = sessionState.value
                        val selectedPage = library.selectedPage
                        if (session is SessionState.SignedIn && selectedPage != null) {
                            // An open page takes over the whole surface — the infinite ink canvas.
                            PageCanvasScreen(
                                apiClient = apiClient,
                                page = selectedPage,
                                userId = session.profile.sub,
                                onBack = { library.closePage() },
                            )
                        } else {
                            AppScreen(
                                apiClient = apiClient,
                                sessionState = session,
                                onSignIn = { signIn() },
                                onSignOut = { signOut() },
                                onReload = { reloadSession() },
                                pages = library.pages,
                                libraryError = library.error,
                                onCreatePage = { createPage() },
                                onOpenPage = { library.openPage(it) },
                                onDeletePage = { library.deletePage(it) },
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
        library.close()
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
                AppLog.w(TAG, "sign in could not start", error)
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
            AppLog.i(TAG, "sign in canceled")
            sessionState.value = SessionState.Error("Sign in was canceled")
            return true
        }

        kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
            authRepository.handleAuthorizationResponse(intent).fold(
                onSuccess = { reloadSession() },
                onFailure = { error ->
                    AppLog.w(TAG, "sign in failed", error)
                    sessionState.value =
                        SessionState.Error(error.message ?: "Sign in failed")
                },
            )
        }
        return true
    }

    private fun signOut() {
        AppLog.i(TAG, "signed out")
        val endSessionIntent = authRepository.createEndSessionIntent()
        library.clear()
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
                    library.start()
                },
                onFailure = { error ->
                    AppLog.w(TAG, "session check failed; signing out locally", error)
                    authRepository.signOutLocal()
                    sessionState.value =
                        SessionState.Error(error.message ?: "Session check failed")
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
