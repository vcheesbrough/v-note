package link.desync.vnote.ink

// Where an open page's sync stands, and why (#150). No status at all means
// sync is nominal. Nothing here is shown on the page (#651) — the top bar's
// status icon speaks for every screen — and no kind is final: all but
// [SessionExpired] are retried without the user, and that one resumes on a
// sign-in.
enum class PageStatusKind(
    // Stable name for logs and tests.
    val tag: String,
) {
    // The first connection to this page is being made. Loading, not trouble.
    Connecting("connecting"),

    // The server could not be reached or the socket dropped. Reconnecting; ink
    // drawn meanwhile is held in memory and sent once the page channel has
    // caught up again (#390).
    Disconnected("disconnected"),

    // Connected, but the server answered an edit with an error code — any
    // code. Everything owed is re-sent with backoff.
    Retrying("retrying"),

    // There is no token to sync with: the identity provider refused the
    // refresh, or none is stored. Only signing in again can fix it.
    SessionExpired("session-expired"),
}

data class PageStatus(
    val kind: PageStatusKind,
    // The line the user reads.
    val message: String,
    // Technical detail (HTTP status, close code, server error code) for the
    // power user and for support; never page content.
    val detail: String? = null,
)
