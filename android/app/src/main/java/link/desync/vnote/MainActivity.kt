package link.desync.vnote

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import link.desync.vnote.ui.theme.VNoteTheme
import okhttp3.OkHttpClient
import okhttp3.Request

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            VNoteTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    PlaceholderScreen(baseUrl = BuildConfig.BASE_URL)
                }
            }
        }
    }
}

@Composable
private fun PlaceholderScreen(baseUrl: String) {
    val healthState = remember { mutableStateOf("Checking server health…") }

    LaunchedEffect(baseUrl) {
        healthState.value =
            withContext(Dispatchers.IO) {
                runCatching {
                    val client = OkHttpClient()
                    val request =
                        Request.Builder()
                            .url("$baseUrl/health")
                            .get()
                            .build()
                    client.newCall(request).execute().use { response ->
                        if (!response.isSuccessful) {
                            "Health check failed: HTTP ${response.code}"
                        } else {
                            "Server healthy at $baseUrl"
                        }
                    }
                }.getOrElse { error ->
                    val detail = error.message ?: error.javaClass.simpleName
                    "Health check failed: $detail"
                }
            }
    }

    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("v-note", style = MaterialTheme.typography.headlineMedium)
        Text("Bootstrap placeholder", style = MaterialTheme.typography.bodyLarge)
        Text("BASE_URL: $baseUrl", style = MaterialTheme.typography.bodyMedium)
        Text(healthState.value, style = MaterialTheme.typography.bodyMedium)
    }
}
