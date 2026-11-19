//! Sync transparency (#150): what a realtime channel's state means to the user.
//!
//! The SPA is online-only and has no stale-cache fallback, so a screen backed
//! by a channel shows its data only while that channel is [`SyncState::Live`].
//! Anything else hides the data and says why. Quiet chrome means healthy sync:
//! [`SyncBanner`] renders nothing when the channel is live, or while it is
//! still making its first connection, which is loading rather than trouble.

use leptos::prelude::*;

use crate::route;

/// Where a realtime channel stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SyncState {
    /// The first connection is in flight; nothing has been shown yet.
    Connecting,
    /// Connected and caught up with the server.
    Live,
    /// The server could not be reached or the socket dropped. Retrying.
    Disconnected(String),
    /// The server answered, but with an error. Retrying.
    Failed(String),
    /// The server refused the session (HTTP 401/403). Retrying cannot fix
    /// that, so the channel stops until the user signs in again.
    SessionExpired,
}

impl SyncState {
    pub(crate) fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

/// Why one connection attempt ended, sorted by what the user can do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelError {
    /// The request or socket never reached the server, or the socket dropped.
    Transport(String),
    /// The server answered with an error status or an error envelope, or sent
    /// something this build cannot read.
    Server(String),
    /// The server refused the session.
    Unauthorized(String),
}

impl ChannelError {
    /// Classify a non-2xx reply. 401 and 403 mean the session is gone, and
    /// anything else is the server failing.
    pub(crate) fn from_status(context: &str, status: u16) -> Self {
        let detail = format!("{context} failed: HTTP {status}");
        if status == 401 || status == 403 {
            Self::Unauthorized(detail)
        } else {
            Self::Server(detail)
        }
    }

    /// The technical detail, for the banner and for telemetry: HTTP statuses,
    /// close codes and reasons, the server's error code and fixed message, and
    /// parse positions. Never page content — a parse error is described by
    /// [`parse_error`], not by serde's message, which can quote the value.
    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::Transport(detail) | Self::Server(detail) | Self::Unauthorized(detail) => detail,
        }
    }

    /// The state the channel is in after this error.
    pub(crate) fn state(&self) -> SyncState {
        match self {
            Self::Transport(detail) => SyncState::Disconnected(detail.clone()),
            Self::Server(detail) => SyncState::Failed(detail.clone()),
            Self::Unauthorized(_) => SyncState::SessionExpired,
        }
    }
}

/// A message this build could not read. Only serde's category and position:
/// its `Display` can quote the offending value, and library events carry page
/// titles (the server's parser says the same of ink, `socket.rs`).
pub(crate) fn parse_error(context: &str, error: &serde_json::Error) -> ChannelError {
    ChannelError::Server(format!(
        "{context} did not parse ({:?} at line {} column {})",
        error.classify(),
        error.line(),
        error.column()
    ))
}

/// Map a socket stream error to a [`ChannelError`], keeping the close code and
/// reason when there is one. A `1013 lagged` close is the server's (#279).
pub(crate) fn socket_error(
    context: &str,
    error: gloo_net::websocket::WebSocketError,
) -> ChannelError {
    use gloo_net::websocket::WebSocketError;
    match error {
        WebSocketError::ConnectionClose(close) if close.reason.is_empty() => {
            ChannelError::Transport(format!("{context} closed (code {})", close.code))
        }
        WebSocketError::ConnectionClose(close) => ChannelError::Transport(format!(
            "{context} closed (code {} {})",
            close.code, close.reason
        )),
        other => ChannelError::Transport(format!("{context} failed: {other}")),
    }
}

/// The banner kind, as a stable `data-sync-state` value e2e can select on.
fn kind(state: &SyncState) -> Option<&'static str> {
    match state {
        SyncState::Connecting | SyncState::Live => None,
        SyncState::Disconnected(_) => Some("disconnected"),
        SyncState::Failed(_) => Some("sync-error"),
        SyncState::SessionExpired => Some("session-expired"),
    }
}

/// A persistent banner for a channel that is not live. `subject` says what is
/// being withheld ("This page", "Your library").
#[component]
pub(crate) fn SyncBanner(state: Signal<SyncState>, subject: &'static str) -> impl IntoView {
    move || {
        let state = state.get();
        let kind = kind(&state)?;
        let (title, body, detail) = match &state {
            SyncState::Disconnected(detail) => (
                "Disconnected from the server",
                format!("Reconnecting… {subject} is hidden until live data is back."),
                Some(detail.clone()),
            ),
            SyncState::Failed(detail) => (
                "Sync error",
                format!("Retrying… {subject} is hidden until the server answers correctly."),
                Some(detail.clone()),
            ),
            SyncState::SessionExpired => (
                "Your session has expired",
                format!("Sign in again to see it. {subject} is hidden until you do."),
                None,
            ),
            SyncState::Connecting | SyncState::Live => unreachable!("no banner for {state:?}"),
        };
        let sign_in = matches!(state, SyncState::SessionExpired).then(|| {
            view! {
                // A full-page hand-off to the IdP, which returns the browser
                // to `/`, so stash the deep link first.
                <a class="button primary" href="/auth/login" on:click=move |_| route::remember_return_route()>"Sign in"</a>
            }
        });
        Some(view! {
            <div class=format!("alert sync-banner {kind}") role="alert" data-testid="sync-banner" data-sync-state=kind>
                <p class="sync-banner-title">{title}</p>
                <p class="sync-banner-body">{body}</p>
                {detail.map(|detail| view! { <p class="sync-banner-detail"><code>{detail}</code></p> })}
                {sign_in}
            </div>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_session_stops_the_channel_and_a_server_error_retries() {
        assert_eq!(
            ChannelError::from_status("ticket", 401).state(),
            SyncState::SessionExpired
        );
        assert_eq!(
            ChannelError::from_status("ticket", 403).state(),
            SyncState::SessionExpired
        );
        assert_eq!(
            ChannelError::from_status("ticket", 503).state(),
            SyncState::Failed("ticket failed: HTTP 503".to_string())
        );
        assert_eq!(
            ChannelError::Transport("socket closed".to_string()).state(),
            SyncState::Disconnected("socket closed".to_string())
        );
    }

    #[test]
    fn only_trouble_gets_a_banner() {
        assert_eq!(kind(&SyncState::Connecting), None);
        assert_eq!(kind(&SyncState::Live), None);
        assert_eq!(
            kind(&SyncState::Disconnected(String::new())),
            Some("disconnected")
        );
        assert_eq!(kind(&SyncState::Failed(String::new())), Some("sync-error"));
        assert_eq!(kind(&SyncState::SessionExpired), Some("session-expired"));
    }

    #[test]
    fn a_parse_error_never_quotes_the_value() {
        // A string where a struct belongs: serde's own message quotes it.
        let error = serde_json::from_str::<protocol::LibraryEvent>(
            r#"{"type":"page-created","page":"my private title"}"#,
        )
        .expect_err("not a valid event");
        assert!(
            error.to_string().contains("private"),
            "the input must be one serde quotes, or this proves nothing: {error}"
        );
        let detail = parse_error("realtime event", &error).detail().to_string();
        assert!(!detail.contains("private"), "{detail}");
        assert!(
            detail.starts_with("realtime event did not parse ("),
            "{detail}"
        );
    }

    #[test]
    fn a_close_keeps_its_code_and_reason() {
        use gloo_net::websocket::{WebSocketError, events::CloseEvent};
        let lagged = WebSocketError::ConnectionClose(CloseEvent {
            code: 1013,
            reason: "lagged".to_string(),
            was_clean: true,
        });
        assert_eq!(
            socket_error("page realtime socket", lagged),
            ChannelError::Transport("page realtime socket closed (code 1013 lagged)".to_string())
        );
    }
}
