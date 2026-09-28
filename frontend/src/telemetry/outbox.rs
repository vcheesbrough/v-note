//! What the SPA does with telemetry between producing it and the ingest
//! accepting it: a bounded queue, whether telemetry is configured at all, the
//! credential, and the rules for when to send and what to do with each answer.
//! Pure state — no browser API — so it is tested on the host.
//!
//! The rules come from the estate's client contract (`observability` skill,
//! `references/client-export.md`), and `android/.../telemetry/Outbox.kt` is the
//! same policy in Kotlin:
//!
//! - **No configuration, no telemetry** ([`Lifecycle`]). Until the server has
//!   said where to send, items wait in a small buffer for a short time; if it
//!   says nothing, they are discarded and OTLP is never initialised.
//! - **Retry only what is retryable** ([`ExportOutcome`]): `429`, `502`, `503`,
//!   `504` and transport failures, with jittered exponential backoff that
//!   honours `Retry-After`. Everything else drops the batch.
//! - **`401` gets one refresh** — a fresh token from the config route — and a
//!   second `401` stops telemetry for the page load.
//! - **Give up for the session** after repeated failure: a fleet of clients
//!   retrying in lockstep against a broken ingest is the failure to avoid.
//! - **Say so locally, on transitions only** ([`Transition`]) — never a line per
//!   batch, and never into the telemetry queue itself, which is the channel
//!   that is failing.

use std::collections::VecDeque;

/// How many finished items are held waiting for the next export, once
/// telemetry is configured. Sized for the unhealthy session, where exports are
/// failing and items pile up.
pub(crate) const CAPACITY: usize = 512;

/// How many items wait for configuration to arrive. Small on purpose: this is
/// the launch sequence, held in the hope the server says telemetry is on.
pub(crate) const PRE_CONFIG_CAPACITY: usize = 128;

/// How long items wait for configuration before they are discarded and
/// telemetry is given up for the page load (`client-export.md`: "never hold
/// them for the whole session in hope").
pub(crate) const PRE_CONFIG_MAX_MS: f64 = 60_000.0;

/// The most items sent in one request. Keeps a single export well under the
/// ingest's decompressed-body cap (4 MiB, `MAX_REQUEST_BODY_BYTES`) even when
/// every span carries attributes.
pub(crate) const MAX_BATCH: usize = 256;

const _: () = assert!(
    MAX_BATCH * 2 * 1024 <= (4 * 1024 * 1024) / 4,
    "MAX_BATCH must leave an export well under the ingest's 4 MiB cap"
);
const _: () = assert!(MAX_BATCH <= CAPACITY && PRE_CONFIG_CAPACITY <= CAPACITY);

/// First backoff after a retryable failure. Doubles per consecutive failure.
pub(crate) const BACKOFF_BASE_MS: f64 = 5_000.0;

/// The longest backoff. Five minutes: an ingest that is down for a while gets
/// one request per client every few minutes, not a crowd every few seconds.
pub(crate) const BACKOFF_MAX_MS: f64 = 300_000.0;

/// The longest a `Retry-After` is honoured for. A larger value is capped here
/// rather than obeyed — a page open all day still tries again eventually.
pub(crate) const RETRY_AFTER_MAX_MS: f64 = 900_000.0;

/// How many times one batch is sent before it is dropped.
pub(crate) const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Consecutive failed attempts after which telemetry stops for the page load.
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 6;

/// A token this close to expiry is refreshed before it is used: it could lapse
/// in flight, or on a device clock a little behind the provider's.
pub(crate) const EXPIRY_MARGIN_S: f64 = 30.0;

/// A FIFO that discards its **oldest** entry when full.
///
/// Oldest, not newest: when exports are failing the interesting telemetry is
/// what is happening *now*, and the newest items are the ones a recovery will
/// actually deliver.
#[derive(Debug)]
pub(crate) struct Outbox<T> {
    items: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> Outbox<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity.min(64)),
            capacity,
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, item: T) {
        if self.capacity == 0 {
            self.dropped += 1;
            return;
        }
        if self.items.len() == self.capacity {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }

    /// Raises (or lowers) the cap. Lowering drops the oldest to fit.
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.items.len() > capacity {
            self.items.pop_front();
            self.dropped += 1;
        }
    }

    /// Removes and returns up to `max` of the oldest items.
    pub(crate) fn take_batch(&mut self, max: usize) -> Vec<T> {
        let count = max.min(self.items.len());
        self.items.drain(..count).collect()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// How many items were discarded for lack of room, ever.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What `GET /api/telemetry/config` came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigFetch {
    /// `200`: where to send, and the credential to send with.
    Configured(Credentials),
    /// `204` (ingest off here) or `404` (a server that predates the route):
    /// the server's answer is "no configuration".
    Absent,
    /// `401`: there is no session to be configured for.
    SignedOut,
    /// No response, a `5xx`, any other status, or a body that did not parse:
    /// the server has not answered yet. Asked again on a later tick — before
    /// the first configuration within [`PRE_CONFIG_MAX_MS`], after it within
    /// [`MAX_CONSECUTIVE_FAILURES`] attempts.
    Unavailable,
}

/// What applying a [`ConfigFetch`] did, for the exporter to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigOutcome {
    /// The first configuration: telemetry is on.
    Configured,
    /// A later fetch replaced the credential: resume.
    Refreshed,
    /// Nothing decided yet; ask again on a later tick.
    Wait,
    /// Telemetry is off for the page load, for this reason.
    Off(OffReason),
}

/// The endpoint and the bearer it is sent with, as the config route returned
/// them. Read **per request** ([`Credentials::usable_token`]) so a refreshed
/// token takes effect on the very next export.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Credentials {
    pub(crate) endpoint: String,
    pub(crate) access_token: String,
    /// Unix seconds.
    pub(crate) expires_at: f64,
}

impl Credentials {
    /// `{endpoint}/v1/{signal}`.
    pub(crate) fn url(&self, signal: Signal) -> String {
        format!(
            "{}/v1/{}",
            self.endpoint.trim_end_matches('/'),
            signal.path()
        )
    }

    /// The token, unless it is expired or about to be.
    pub(crate) fn usable_token(&self, now_s: f64) -> Option<&str> {
        (self.expires_at > now_s + EXPIRY_MARGIN_S && !self.access_token.is_empty())
            .then_some(self.access_token.as_str())
    }
}

/// The two OTLP signals the SPA sends. Metrics are never sent from a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Traces,
    Logs,
}

impl Signal {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Logs => "logs",
        }
    }
}

/// Whether telemetry is on for this page load.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Lifecycle {
    /// No answer from the server yet. `since_ms` is when the page started
    /// waiting; past [`PRE_CONFIG_MAX_MS`] the wait is over.
    AwaitingConfig { since_ms: f64 },
    /// Configured: exporting. `failed_refreshes` counts consecutive refresh
    /// fetches the server did not answer.
    Configured {
        credentials: Credentials,
        failed_refreshes: u32,
    },
    /// Off for the rest of the page load, and why. Final.
    Off(OffReason),
}

/// Why telemetry is off. Each is said once, locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffReason {
    /// The server's answer was "no configuration": ingest is off here.
    NotConfigured,
    /// No session to fetch configuration with.
    SignedOut,
    /// Configuration did not arrive within [`PRE_CONFIG_MAX_MS`].
    ConfigTimedOut,
    /// The session's access token expired. The SPA cannot renew it — its
    /// session is the cookie login set — so there is nothing to refresh to.
    SessionExpired,
    /// A refreshed token was refused too: a `401` that will not change.
    Unauthorized,
    /// [`MAX_CONSECUTIVE_FAILURES`] in a row, of exports or of refreshes.
    GaveUp,
}

impl OffReason {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::NotConfigured => "the server gave no telemetry configuration",
            Self::SignedOut => "there is no signed-in session",
            Self::ConfigTimedOut => "no telemetry configuration arrived in time",
            Self::SessionExpired => "the session's access token expired",
            Self::Unauthorized => "the ingest refused a refreshed token (401)",
            Self::GaveUp => "the ingest or the server kept failing",
        }
    }
}

impl Lifecycle {
    pub(crate) fn new(now_ms: f64) -> Self {
        Self::AwaitingConfig { since_ms: now_ms }
    }

    /// Applies a config fetch made at `now_s` (Unix seconds).
    ///
    /// The first fetch decides whether telemetry is on at all: only a `200`
    /// turns it on, "no configuration" or no session turn it off, and a server
    /// that did not answer is asked again (bounded by [`Lifecycle::expire`]).
    ///
    /// A later fetch is a refresh. The newest configuration wins, including
    /// "off"; a token that is no longer usable ends it, since the SPA has no
    /// way to renew one; and an unanswered refresh keeps the old credentials
    /// until it has failed [`MAX_CONSECUTIVE_FAILURES`] times.
    pub(crate) fn on_config(&mut self, fetch: ConfigFetch, now_s: f64) -> ConfigOutcome {
        let configured = match self {
            Self::Off(reason) => return ConfigOutcome::Off(*reason),
            Self::AwaitingConfig { .. } => false,
            Self::Configured { .. } => true,
        };
        let outcome = match fetch {
            ConfigFetch::Configured(credentials) if credentials.usable_token(now_s).is_some() => {
                *self = Self::Configured {
                    credentials,
                    failed_refreshes: 0,
                };
                return if configured {
                    ConfigOutcome::Refreshed
                } else {
                    ConfigOutcome::Configured
                };
            }
            ConfigFetch::Configured(_) => ConfigOutcome::Off(OffReason::SessionExpired),
            ConfigFetch::Absent => ConfigOutcome::Off(OffReason::NotConfigured),
            ConfigFetch::SignedOut if configured => ConfigOutcome::Off(OffReason::SessionExpired),
            ConfigFetch::SignedOut => ConfigOutcome::Off(OffReason::SignedOut),
            ConfigFetch::Unavailable => {
                if let Self::Configured {
                    failed_refreshes, ..
                } = self
                {
                    *failed_refreshes += 1;
                    if *failed_refreshes >= MAX_CONSECUTIVE_FAILURES {
                        ConfigOutcome::Off(OffReason::GaveUp)
                    } else {
                        ConfigOutcome::Wait
                    }
                } else {
                    ConfigOutcome::Wait
                }
            }
        };
        if let ConfigOutcome::Off(reason) = outcome {
            *self = Self::Off(reason);
        }
        outcome
    }

    /// Called every tick: a wait for configuration that has gone on too long
    /// ends it. Returns `true` when this call is the one that turned it off.
    pub(crate) fn expire(&mut self, now_ms: f64) -> bool {
        match self {
            Self::AwaitingConfig { since_ms } if now_ms - *since_ms >= PRE_CONFIG_MAX_MS => {
                *self = Self::Off(OffReason::ConfigTimedOut);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn turn_off(&mut self, reason: OffReason) {
        if !matches!(self, Self::Off(_)) {
            *self = Self::Off(reason);
        }
    }

    pub(crate) fn is_off(&self) -> bool {
        matches!(self, Self::Off(_))
    }

    pub(crate) fn is_awaiting(&self) -> bool {
        matches!(self, Self::AwaitingConfig { .. })
    }

    pub(crate) fn credentials(&self) -> Option<&Credentials> {
        match self {
            Self::Configured { credentials, .. } => Some(credentials),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// What an export attempt came to, reduced to what the exporter does about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ExportOutcome {
    /// 2xx.
    Accepted,
    /// `401`: every refusal of the token — expired, invalid, missing a scope or
    /// a claim. One refresh, then stop.
    Unauthorized,
    /// Permanent for this payload: `400`, `403`, `413`, `500` and every other
    /// status OTLP does not name retryable. The batch is dropped.
    Rejected { status: u16 },
    /// `429`, `502`, `503`, `504`, or no response at all (`status: None`).
    /// Retried later, honouring `retry_after_ms` when the ingest sent one.
    Retryable {
        status: Option<u16>,
        retry_after_ms: Option<f64>,
    },
}

impl ExportOutcome {
    /// `status: None` is a transport failure. `retry_after` is the raw header.
    pub(crate) fn from_response(status: Option<u16>, retry_after: Option<&str>) -> Self {
        match status {
            Some(200..=299) => Self::Accepted,
            Some(401) => Self::Unauthorized,
            Some(status @ (429 | 502 | 503 | 504)) => Self::Retryable {
                status: Some(status),
                retry_after_ms: retry_after.and_then(parse_retry_after_ms),
            },
            Some(status) => Self::Rejected { status },
            None => Self::Retryable {
                status: None,
                retry_after_ms: None,
            },
        }
    }

    /// For the local log line: the status, or `transport error`.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Accepted => "accepted".to_string(),
            Self::Unauthorized => "status 401".to_string(),
            Self::Rejected { status } => format!("status {status}"),
            Self::Retryable {
                status: Some(status),
                ..
            } => format!("status {status}"),
            Self::Retryable { status: None, .. } => "transport error".to_string(),
        }
    }
}

/// `Retry-After` as delay-seconds. The HTTP-date form is not parsed: no clock
/// on a user's device is trusted enough to subtract from, and the backoff alone
/// is a safe answer.
pub(crate) fn parse_retry_after_ms(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .map(|seconds| f64::from(seconds) * 1_000.0)
}

/// A change of state worth one local log line. Nothing else is reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Transition {
    /// The first failure after a success (or since the start).
    StartedFailing,
    /// The first success after one or more failures.
    Recovered,
    /// Telemetry is off for the page load from here on.
    Stopped(OffReason),
}

/// What the exporter does with the batch it just sent, and what to tell the
/// console.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Decision {
    /// Keep the batch and send it again later. `false` means it is gone.
    pub(crate) retry_batch: bool,
    /// Fetch a fresh token before the next attempt.
    pub(crate) refresh_token: bool,
    pub(crate) transition: Option<Transition>,
}

/// When to send, and what each answer means for the next attempt.
///
/// Time is milliseconds from any monotonic-enough origin (`Date::now` in the
/// browser); `jitter` is a uniform draw in `[0, 1)`. Both are passed in, so the
/// policy is deterministic under test.
#[derive(Debug, Default)]
pub(crate) struct ExportPolicy {
    stopped: Option<OffReason>,
    /// Consecutive failed attempts (retryable ones), for backoff and give-up.
    failures: u32,
    /// Whether the last attempt failed, for the transition lines.
    failing: bool,
    /// A `401` was answered with a refresh; the next `401` is final.
    refreshed_after_401: bool,
    /// A refresh is owed before the next attempt.
    refresh_pending: bool,
    next_attempt_at_ms: f64,
}

impl ExportPolicy {
    pub(crate) fn stopped(&self) -> Option<OffReason> {
        self.stopped
    }

    /// Whether to send now. A refresh that is owed must happen first.
    pub(crate) fn should_export(&self, now_ms: f64) -> bool {
        self.stopped.is_none() && !self.refresh_pending && now_ms >= self.next_attempt_at_ms
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        self.stopped.is_none() && self.refresh_pending
    }

    /// A refreshed token has arrived; exports may resume.
    pub(crate) fn refreshed(&mut self) {
        self.refresh_pending = false;
    }

    /// Stops the policy from outside — a refresh that found no configuration.
    pub(crate) fn stop(&mut self, reason: OffReason) -> Option<Transition> {
        if self.stopped.is_some() {
            return None;
        }
        self.stopped = Some(reason);
        Some(Transition::Stopped(reason))
    }

    /// Records one attempt's answer. `attempts` is how many times *this* batch
    /// has now been sent, this attempt included.
    pub(crate) fn record(
        &mut self,
        outcome: ExportOutcome,
        attempts: u32,
        now_ms: f64,
        jitter: f64,
    ) -> Decision {
        let mut decision = Decision {
            retry_batch: false,
            refresh_token: false,
            transition: None,
        };
        if self.stopped.is_some() {
            return decision;
        }
        match outcome {
            ExportOutcome::Accepted => {
                self.failures = 0;
                self.refreshed_after_401 = false;
                self.next_attempt_at_ms = now_ms;
                if self.failing {
                    self.failing = false;
                    decision.transition = Some(Transition::Recovered);
                }
            }
            ExportOutcome::Unauthorized => {
                // The batch is dropped either way: a 401 is permanent for the
                // payload that earned it.
                if self.refreshed_after_401 {
                    decision.transition = self.stop(OffReason::Unauthorized);
                } else {
                    self.refreshed_after_401 = true;
                    self.refresh_pending = true;
                    decision.refresh_token = true;
                    decision.transition = self.start_failing();
                }
            }
            ExportOutcome::Rejected { .. } => {
                // The batch's fault, not the ingest's health: no backoff, and
                // nothing suggests the next batch is doomed.
                decision.transition = self.start_failing();
            }
            ExportOutcome::Retryable { retry_after_ms, .. } => {
                self.failures += 1;
                if self.failures >= MAX_CONSECUTIVE_FAILURES {
                    decision.transition = self.stop(OffReason::GaveUp);
                    return decision;
                }
                let wait = backoff_ms(self.failures, jitter)
                    .max(retry_after_ms.unwrap_or(0.0).min(RETRY_AFTER_MAX_MS));
                self.next_attempt_at_ms = now_ms + wait;
                decision.retry_batch = attempts < MAX_BATCH_ATTEMPTS;
                decision.transition = self.start_failing();
            }
        }
        decision
    }

    fn start_failing(&mut self) -> Option<Transition> {
        if self.failing {
            None
        } else {
            self.failing = true;
            Some(Transition::StartedFailing)
        }
    }
}

/// Exponential backoff with "equal jitter": the `n`th consecutive failure waits
/// between half and all of `BASE * 2^(n-1)`, capped at [`BACKOFF_MAX_MS`]. Half
/// is fixed so a crowd still spreads out; half is random so it does not arrive
/// back in lockstep.
pub(crate) fn backoff_ms(failures: u32, jitter: f64) -> f64 {
    let exponent = failures.saturating_sub(1).min(16);
    let ceiling = (BACKOFF_BASE_MS * f64::from(1_u32 << exponent)).min(BACKOFF_MAX_MS);
    let jitter = jitter.clamp(0.0, 1.0);
    ceiling / 2.0 + ceiling / 2.0 * jitter
}

#[cfg(test)]
mod tests;
