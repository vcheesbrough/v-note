package link.desync.vnote.ink

// What the open page's banner says, and why (#150). Each kind is a different
// thing for the user to do — wait, wait while ink is retried, sign in again,
// or edit somewhere else — so the kinds are kept apart rather than flattened
// into one error string. No status at all means sync is nominal.
enum class PageStatusKind(
    // Stable value for the banner's test tag, so a UI test can tell the kinds
    // apart without matching copy.
    val tag: String,
) {
    // The first connection to this page is being made. Loading, not trouble.
    Connecting("connecting"),

    // The server could not be reached or the socket dropped. Reconnecting; ink
    // drawn meanwhile is held in memory and sent once the lease is back (#390).
    Disconnected("disconnected"),

    // Connected, but the server failed to store ink (`commit_failed`). Ink is
    // blocked while the held batches are re-sent with backoff.
    Retrying("retrying"),

    // Another session holds the page's edit lease.
    LeaseHeld("lease-held"),

    // The server refused this device's session and a token refresh could not
    // fix it. Only signing in again can.
    SessionExpired("session-expired"),

    // The server definitively refused something this session sent. Not
    // retried; leaving and reopening the page starts afresh.
    Rejected("rejected"),
}

data class PageStatus(
    val kind: PageStatusKind,
    // The line the user reads.
    val message: String,
    // Technical detail (HTTP status, close code, server error code) for the
    // power user and for support; never page content.
    val detail: String? = null,
) {
    val isError: Boolean get() = kind != PageStatusKind.Connecting
}
