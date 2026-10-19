package link.desync.vnote.net

import link.desync.vnote.BuildConfig

// The server the app talks to when nothing more specific is injected (#464):
// the app's default API client and client telemetry's config fetch.
//
// In the app this is always the flavor's `BuildConfig.BASE_URL`; nothing in
// `main` assigns it. Only the instrumentation runner does, before
// `Application.onCreate`, so the telemetry exporter the Application installs
// and any activity a test launches without its own API client talk to a
// loopback address instead of the real dev deployment.
object Endpoints {
    @Volatile
    var serverBaseUrl: String = BuildConfig.BASE_URL
        internal set
}
