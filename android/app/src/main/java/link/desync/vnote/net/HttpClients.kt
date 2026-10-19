package link.desync.vnote.net

import android.net.Uri
import net.openid.appauth.connectivity.ConnectionBuilder
import net.openid.appauth.connectivity.DefaultConnectionBuilder
import okhttp3.Interceptor
import okhttp3.OkHttpClient
import okhttp3.Response
import java.io.IOException
import java.net.HttpURLConnection

// Decides, before anything connects, whether a request may go to [host]. Throws
// to refuse. [url] is for the report only.
fun interface HostGuard {
    @Throws(IOException::class)
    fun check(
        host: String,
        url: String,
    )
}

// Where every HTTP client in the app is built (#464): the API client (REST,
// both WebSockets, `/health`, thumbnails), the telemetry exporter and the
// token endpoint, plus AppAuth's discovery connections.
//
// In the app [hostGuard] is null and the builder is exactly
// `OkHttpClient.Builder()`, so production clients are unchanged. The
// instrumentation runner installs a guard before `Application.onCreate`; every
// client built after that refuses non-loopback hosts before it resolves or
// connects anything, so no test can reach the real dev deployment unnoticed.
object HttpClients {
    @Volatile
    var hostGuard: HostGuard? = null
        internal set

    // A client configured by [configure]. The guard, when installed, is the
    // **last** application interceptor: still before any DNS or connect, and
    // it also sees WebSocket upgrades, which OkHttp sends through the same
    // chain. Last, so it is never on the stack when an earlier interceptor
    // (`TracingInterceptor`) reads its caller's location for a span (#453).
    fun build(configure: OkHttpClient.Builder.() -> Unit = {}): OkHttpClient {
        val builder = OkHttpClient.Builder().apply(configure)
        hostGuard?.let { guard -> builder.addInterceptor(HostGuardInterceptor(guard)) }
        return builder.build()
    }

    // For AppAuth's own `HttpURLConnection`s (issuer discovery): null in the
    // app, which keeps AppAuth on its default connection builder.
    fun appAuthConnectionBuilder(): ConnectionBuilder? = hostGuard?.let(::GuardedConnectionBuilder)
}

internal class HostGuardInterceptor(
    private val guard: HostGuard,
) : Interceptor {
    override fun intercept(chain: Interceptor.Chain): Response {
        val url = chain.request().url
        guard.check(url.host, url.redact())
        return chain.proceed(chain.request())
    }
}

private class GuardedConnectionBuilder(
    private val guard: HostGuard,
) : ConnectionBuilder {
    override fun openConnection(uri: Uri): HttpURLConnection {
        val reported =
            uri
                .buildUpon()
                .clearQuery()
                .build()
                .toString()
        guard.check(uri.host.orEmpty(), reported)
        return DefaultConnectionBuilder.INSTANCE.openConnection(uri)
    }
}
