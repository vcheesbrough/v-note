//! What the SPA does with telemetry between producing it and the sidecar
//! accepting it (#354): a bounded queue, and the rules for when to send. Pure
//! state — no browser API — so it is tested on the host.
//!
//! The rule everything here serves: **telemetry is best effort and must never
//! cost the product anything**. So nothing is retried, nothing grows without
//! bound, and a collector that is down, off or unhappy is met with *fewer*
//! requests, not more.

use std::collections::VecDeque;

/// How many finished items are held waiting for the next export. At one tick
/// every five seconds this is far more than a healthy session produces; it is
/// sized for the unhealthy one, where exports are failing and items pile up.
pub(crate) const CAPACITY: usize = 512;

/// The most items sent in one request. Keeps a single export comfortably under
/// the ingress's 1 MiB cap even when every span carries attributes.
pub(crate) const MAX_BATCH: usize = 256;

/// A batch that exceeded the ingress cap would be answered 413 and thrown away,
/// so the relationship between the two numbers is checked at compile time
/// rather than discovered in production. 2 KiB per item is far above what a span
/// with a handful of attributes encodes to.
const _: () = assert!(
    MAX_BATCH * 2 * 1024 <= (1024 * 1024) / 2,
    "MAX_BATCH must leave an export well under the ingress's 1 MiB cap"
);
const _: () = assert!(MAX_BATCH <= CAPACITY);

/// The longest the exporter waits between attempts while they keep failing, in
/// ticks. Twelve five-second ticks: a dead sidecar costs one request a minute.
const MAX_BACKOFF_TICKS: u32 = 12;

/// A FIFO that discards its **oldest** entry when full.
///
/// Oldest, not newest: when exports are failing the interesting telemetry is
/// what is happening *now* — the failure that started it is long gone from a
/// five-hundred-item window either way, and the newest items are the ones a
/// recovery will actually deliver.
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

/// What an export attempt came to, reduced to what the exporter does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportOutcome {
    /// 2xx.
    Accepted,
    /// 404: the server has client telemetry switched off (or is a build from
    /// before it existed). Final for this page load.
    SwitchedOff,
    /// The batch itself was refused — malformed, or over the size cap. Sending
    /// it again cannot help, and nothing suggests the *next* batch is doomed.
    BatchRefused,
    /// Anything else: not signed in, the sidecar down or shedding load, the
    /// network gone. Worth trying again, later and less often.
    Unavailable,
}

impl ExportOutcome {
    /// `None` is a transport failure — no response at all.
    pub(crate) fn from_status(status: Option<u16>) -> Self {
        match status {
            Some(200..=299) => Self::Accepted,
            Some(404) => Self::SwitchedOff,
            Some(400 | 413 | 415) => Self::BatchRefused,
            _ => Self::Unavailable,
        }
    }
}

/// Whether to export on a given tick.
///
/// Note what is *not* here: a retry queue. A batch that fails is gone. Holding
/// it would mean re-sending spans the sidecar may already have accepted before
/// the response was lost, and would let one poisoned batch block the queue.
#[derive(Debug, Default)]
pub(crate) struct ExportPolicy {
    switched_off: bool,
    authenticated: bool,
    /// Ticks still to sit out before the next attempt.
    wait: u32,
    /// What `wait` is reset to on the next failure; doubles each time.
    backoff: u32,
}

impl ExportPolicy {
    /// Final: once the server has said telemetry is off, nothing is queued or
    /// sent again until the page is reloaded.
    pub(crate) fn is_switched_off(&self) -> bool {
        self.switched_off
    }

    /// The ingress authenticates with the session cookie, so an export from a
    /// signed-out page can only be a 401. Knowing that in advance is cheaper
    /// than finding it out every tick.
    pub(crate) fn set_authenticated(&mut self, authenticated: bool) {
        self.authenticated = authenticated;
    }

    /// Called once per tick. `true` means "send now"; a `false` during backoff
    /// consumes one tick of the wait.
    pub(crate) fn should_export(&mut self) -> bool {
        if self.switched_off || !self.authenticated {
            return false;
        }
        if self.wait > 0 {
            self.wait -= 1;
            return false;
        }
        true
    }

    pub(crate) fn record(&mut self, outcome: ExportOutcome) {
        match outcome {
            ExportOutcome::Accepted | ExportOutcome::BatchRefused => {
                self.wait = 0;
                self.backoff = 0;
            }
            ExportOutcome::SwitchedOff => self.switched_off = true,
            ExportOutcome::Unavailable => {
                self.backoff = (self.backoff * 2).clamp(1, MAX_BACKOFF_TICKS);
                self.wait = self.backoff;
            }
        }
    }
}

#[cfg(test)]
mod tests;
