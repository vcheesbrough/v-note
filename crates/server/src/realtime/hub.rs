//! In-process realtime state: tickets and per-owner and per-page broadcast
//! channels. No transport, no SQL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};
use opentelemetry::trace::SpanContext;
use protocol::{LibraryEvent, PageServerMessage, RealtimeTicketResponse};
use rand::Rng;
use tokio::sync::broadcast;

const TICKET_TTL_SECONDS: i64 = 60;
pub(super) const LIBRARY_CHANNEL_CAPACITY: usize = 64;
/// How far one page subscriber may fall behind before it is closed (#279).
///
/// Kept at 256 rather than raised: a subscriber this far behind is one whose
/// socket has stopped draining (a stalled network, a frozen tab), and a larger
/// buffer only delays the close while every retained message — a stroke batch
/// is up to ~1 MiB inbound — is held per page. Recovery is a reconnect and a
/// gap-filling `subscribe`, which costs the same however far behind it was.
/// `v_note_realtime_events_total{result="lagged"}` is the signal for revisiting
/// this; the lag log line carries how many messages were skipped.
pub(super) const PAGE_CHANNEL_CAPACITY: usize = 256;

pub struct RealtimeHub {
    tickets: Mutex<HashMap<String, Ticket>>,
    library_channels: Channels<LibraryEvent>,
    page_channels: Channels<PageServerMessage>,
}

impl Default for RealtimeHub {
    fn default() -> Self {
        Self {
            tickets: Mutex::default(),
            library_channels: Channels::new("library", LIBRARY_CHANNEL_CAPACITY),
            page_channels: Channels::new("page", PAGE_CHANNEL_CAPACITY),
        }
    }
}

/// One kind of broadcast channel, keyed by page or owner id.
///
/// **A channel exists exactly while something is subscribed to it (#633).**
/// [`Channels::subscribe`] creates it, and the [`Subscription`] it returns
/// removes it when it drops as the channel's last receiver. Before #633 an
/// entry, and the buffer behind it, lived for the life of the process: every
/// page ever opened and every owner whose library ever connected.
///
/// Every operation that touches an entry — subscribing, publishing, pruning —
/// runs under the one mutex, which is what makes pruning safe: a subscriber
/// either joins before the prune looks (so the channel is kept) or after it
/// (so it creates a fresh one), and a publish sends either to the live channel
/// or to none. No sender is cloned out of the map, so no publish can land on a
/// channel that has already been pruned and replaced.
struct Channels<T> {
    kind: &'static str,
    capacity: usize,
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<Fanout<T>>>>>,
}

impl<T> Clone for Channels<T> {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            capacity: self.capacity,
            senders: Arc::clone(&self.senders),
        }
    }
}

impl<T: Clone> Channels<T> {
    fn new(kind: &'static str, capacity: usize) -> Self {
        Self {
            kind,
            capacity,
            senders: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<Fanout<T>>>> {
        // Every critical section leaves the map consistent (an insert or a
        // remove), so a panic elsewhere while it was held leaves nothing to
        // repair. Recovering rather than panicking also keeps `Subscription`'s
        // `Drop` from panicking during an unwind, which would abort.
        self.senders.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn subscribe(&self, key: &str) -> Subscription<T> {
        let receiver = {
            let mut senders = self.lock();
            let sender = senders.entry(key.to_string()).or_insert_with(|| {
                crate::observability::metrics().realtime_channel_opened(self.kind);
                let (sender, _) = broadcast::channel(self.capacity);
                sender
            });
            sender.subscribe()
        };
        Subscription {
            receiver: Some(receiver),
            key: key.to_string(),
            channels: self.clone(),
        }
    }

    /// Sends to the channel's current receivers. `None` when the channel does
    /// not exist, i.e. nothing is subscribed: publishing never creates one.
    fn publish(&self, key: &str, fanout: Fanout<T>) -> Option<usize> {
        let senders = self.lock();
        let sender = senders.get(key)?;
        // `Err` only means every receiver dropped and its prune has not run
        // yet, which is the same as nobody being subscribed.
        Some(sender.send(fanout).unwrap_or(0))
    }

    /// Removes `key`'s channel if nothing receives from it any more. Called by
    /// every [`Subscription`] after its receiver has dropped; a channel with a
    /// receiver left, including one that joined a moment ago, is kept.
    fn prune(&self, key: &str) {
        let mut senders = self.lock();
        let unused = senders
            .get(key)
            .is_some_and(|sender| sender.receiver_count() == 0);
        if unused {
            senders.remove(key);
            drop(senders);
            crate::observability::metrics().realtime_channel_closed(self.kind);
            // Named like every other realtime line's id, so a Loki search on
            // `page_id` / `owner_id` finds the prune too. Field names must be
            // static, hence one arm per kind.
            match self.kind {
                "page" => tracing::debug!(
                    channel = self.kind,
                    page_id = key,
                    "realtime channel pruned: no subscribers left"
                ),
                _ => tracing::debug!(
                    channel = self.kind,
                    owner_id = key,
                    "realtime channel pruned: no subscribers left"
                ),
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }

    #[cfg(test)]
    fn receiver_count(&self, key: &str) -> usize {
        self.lock()
            .get(key)
            .map_or(0, broadcast::Sender::receiver_count)
    }
}

/// A receiver on one page or library channel that removes the channel when it
/// drops as the last one (#633). Deliberately not a `Deref` to the
/// `broadcast::Receiver`: `resubscribe` on that would make a receiver nothing
/// prunes after.
pub(super) struct Subscription<T: Clone> {
    /// `Some` until `drop`, which has to drop it before pruning: the channel's
    /// receiver count only falls once it is gone.
    receiver: Option<broadcast::Receiver<Fanout<T>>>,
    key: String,
    channels: Channels<T>,
}

impl<T: Clone> Subscription<T> {
    fn receiver(&mut self) -> &mut broadcast::Receiver<Fanout<T>> {
        self.receiver
            .as_mut()
            .expect("the receiver is only taken in drop")
    }

    pub(super) async fn recv(&mut self) -> Result<Fanout<T>, broadcast::error::RecvError> {
        self.receiver().recv().await
    }

    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<Fanout<T>, broadcast::error::TryRecvError> {
        self.receiver().try_recv()
    }
}

impl<T: Clone> Drop for Subscription<T> {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.channels.prune(&self.key);
    }
}

/// A broadcast message plus the trace context of whoever published it. Each
/// receiving socket sends it inside a delivery span that joins the publisher's
/// trace (see [`fanout_delivery_span`]), so a `commit-batch` trace shows the
/// send to every sibling session instead of the fan-out vanishing into the
/// receivers' hours-long connection spans.
#[derive(Clone)]
pub(super) struct Fanout<T> {
    pub(super) message: T,
    pub(super) origin: SpanContext,
}

impl<T> Fanout<T> {
    fn from_current_span(message: T) -> Self {
        Self {
            message,
            origin: crate::observability::current_span_context(),
        }
    }
}

/// The span for sending one fanned-out message to one socket: a child of the
/// publisher's trace and linked to the receiving connection, which is the
/// current span. Its own trace when the publisher had none (e.g. an event
/// published outside any request).
pub(super) fn fanout_delivery_span(
    channel: &'static str,
    message_type: &'static str,
    origin: &SpanContext,
) -> tracing::Span {
    let span = tracing::info_span!(
        parent: None,
        "realtime.fanout.deliver",
        channel,
        message_type,
        session_id = tracing::field::Empty,
        bytes = tracing::field::Empty,
    );
    span.follows_from(tracing::Span::current());
    crate::observability::set_remote_parent(&span, origin);
    span
}

#[derive(Clone)]
struct Ticket {
    owner_id: String,
    expires_at: DateTime<Utc>,
    /// The trace the ticket was requested in (#354). A browser cannot put a
    /// `traceparent` header on a WebSocket upgrade, so the ticket — which it
    /// *can* request with one — is what carries the client's trace across to the
    /// connection that redeems it. Invalid when nothing was tracing the request.
    origin: SpanContext,
}

/// What redeeming a ticket yields: who it was for, and the trace it was
/// requested in.
pub(super) struct RedeemedTicket {
    pub(super) owner_id: String,
    pub(super) origin: SpanContext,
}

impl RealtimeHub {
    pub fn issue_ticket(&self, owner_id: String, origin: SpanContext) -> RealtimeTicketResponse {
        let ticket = format!("ticket_{}", random_hex(32));
        let expires_at = Utc::now() + Duration::seconds(TICKET_TTL_SECONDS);
        // Never the ticket itself: it is a bearer credential until redeemed.
        tracing::debug!(owner_id = %owner_id, "realtime ticket issued");
        self.tickets.lock().expect("ticket mutex poisoned").insert(
            ticket.clone(),
            Ticket {
                owner_id,
                expires_at,
                origin,
            },
        );
        RealtimeTicketResponse {
            ticket,
            expires_at: expires_at.to_rfc3339(),
        }
    }

    pub(super) fn consume_ticket(&self, ticket: &str) -> Option<RedeemedTicket> {
        let now = Utc::now();
        let mut tickets = self.tickets.lock().expect("ticket mutex poisoned");
        tickets.retain(|_, value| value.expires_at > now);
        let Some(ticket) = tickets.remove(ticket) else {
            // Expired tickets were swept just above, so unknown and expired
            // are one case here. The caller answers 401 and logs the rejection.
            tracing::debug!("realtime ticket not found or expired");
            return None;
        };
        (ticket.expires_at > now).then_some(RedeemedTicket {
            owner_id: ticket.owner_id,
            origin: ticket.origin,
        })
    }

    pub(super) fn subscribe_library(&self, owner_id: &str) -> Subscription<LibraryEvent> {
        self.library_channels.subscribe(owner_id)
    }

    /// Fans `event` out to the owner's library sockets. An owner with none
    /// open has no channel, and publishing to it is a no-op (#633): nothing
    /// would have received it, and a channel created here would outlive every
    /// socket.
    pub fn publish_library_event(&self, owner_id: &str, event: LibraryEvent) {
        let event_type = event.message_type();
        let Some(receivers) = self
            .library_channels
            .publish(owner_id, Fanout::from_current_span(event))
        else {
            tracing::debug!(
                owner_id = %owner_id,
                event_type,
                "library event not published: no library socket is open"
            );
            return;
        };
        tracing::debug!(
            owner_id = %owner_id,
            event_type,
            receivers,
            "library event published"
        );
    }

    pub(super) fn subscribe_page(&self, page_id: &str) -> Subscription<PageServerMessage> {
        self.page_channels.subscribe(page_id)
    }

    /// Fans `message` out to every session on the page. A page nobody has
    /// open has no channel, and publishing to it is a no-op rather than one
    /// that creates a channel: the REST bin routes (#631) publish to pages
    /// that are usually closed.
    pub fn publish_page(&self, page_id: &str, message: PageServerMessage) {
        let message_type = message.message_type();
        let Some(receivers) = self
            .page_channels
            .publish(page_id, Fanout::from_current_span(message))
        else {
            tracing::debug!(
                page_id = %page_id,
                message_type,
                "page message not published: no session has the page open"
            );
            return;
        };
        tracing::debug!(
            page_id = %page_id,
            message_type,
            receivers,
            "page message published"
        );
    }

    /// How many page channels exist: one per page some session has open.
    #[cfg(test)]
    pub(crate) fn page_channel_count(&self) -> usize {
        self.page_channels.len()
    }

    /// How many library channels exist: one per owner with a library socket open.
    #[cfg(test)]
    pub(crate) fn library_channel_count(&self) -> usize {
        self.library_channels.len()
    }

    /// How many receivers `page_id`'s channel has; 0 when it has none.
    #[cfg(test)]
    pub(crate) fn page_receiver_count(&self, page_id: &str) -> usize {
        self.page_channels.receiver_count(page_id)
    }

    /// How many receivers `owner_id`'s library channel has; 0 when it has none.
    #[cfg(test)]
    pub(crate) fn library_receiver_count(&self, owner_id: &str) -> usize {
        self.library_channels.receiver_count(owner_id)
    }
}

pub(super) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `check` with an OpenTelemetry layer installed, as in production, so
    /// spans carry real trace context. The provider has no exporter.
    fn with_otel_layer(check: impl FnOnce()) {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, check);
    }

    fn trace_id(span: &tracing::Span) -> String {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        span.context().span().span_context().trace_id().to_string()
    }

    #[test]
    fn fanout_carries_the_publishers_trace_into_the_delivery_span() {
        with_otel_layer(|| {
            let hub = RealtimeHub::default();
            let mut receiver = hub.subscribe_page("page_1");
            let publisher = tracing::info_span!("commit-batch");
            let synced = PageServerMessage::Synced { last_seq: 1 };
            publisher.in_scope(|| hub.publish_page("page_1", synced.clone()));

            let Fanout { message, origin } = receiver
                .try_recv()
                .expect("the published message should reach the subscriber");
            assert_eq!(message, synced);
            assert_eq!(origin.trace_id().to_string(), trace_id(&publisher));

            let delivery = fanout_delivery_span("page", message.message_type(), &origin);
            assert_eq!(
                trace_id(&delivery),
                trace_id(&publisher),
                "the delivery should sit inside the publisher's trace"
            );
        });
    }

    /// #354: the browser cannot put `traceparent` on a WebSocket upgrade, so the
    /// ticket carries the trace instead. This is the hub's half of that — the
    /// context that comes out is the one that went in, and a connection span
    /// parented to it lands in the requesting trace.
    #[test]
    fn a_ticket_carries_the_requesting_trace_to_the_connection_that_redeems_it() {
        with_otel_layer(|| {
            let hub = RealtimeHub::default();
            let ticket_request = tracing::info_span!("http.request");
            let issued = ticket_request.in_scope(|| {
                hub.issue_ticket(
                    "owner_1".to_string(),
                    crate::observability::current_span_context(),
                )
            });

            let redeemed = hub
                .consume_ticket(&issued.ticket)
                .expect("a fresh ticket should redeem");
            assert_eq!(redeemed.owner_id, "owner_1");
            assert_eq!(
                redeemed.origin.trace_id().to_string(),
                trace_id(&ticket_request)
            );

            // What `page_socket` does with it, outside any ambient span — the
            // connection task is not running inside the ticket request.
            let connection = tracing::info_span!("handle_page_socket");
            crate::observability::set_remote_parent(&connection, &redeemed.origin);
            assert_eq!(
                trace_id(&connection),
                trace_id(&ticket_request),
                "the connection should sit inside the trace the ticket was requested in"
            );
        });
    }

    /// A ticket requested with nothing tracing it must not drag the connection
    /// anywhere: the context is invalid, `set_remote_parent` ignores it, and the
    /// span keeps whatever parent it already had. That is every ticket while
    /// trace export is off.
    #[test]
    fn a_ticket_issued_outside_a_trace_leaves_the_connection_where_it_was() {
        with_otel_layer(|| {
            let hub = RealtimeHub::default();
            let issued = hub.issue_ticket(
                "owner_1".to_string(),
                crate::observability::current_span_context(),
            );
            let redeemed = hub.consume_ticket(&issued.ticket).expect("redeems");
            assert!(!redeemed.origin.is_valid());

            let upgrade = tracing::info_span!("http.request");
            let connection = upgrade.in_scope(|| tracing::info_span!("handle_page_socket"));
            crate::observability::set_remote_parent(&connection, &redeemed.origin);
            assert_eq!(
                trace_id(&connection),
                trace_id(&upgrade),
                "with no origin the connection stays under the upgrade request"
            );
        });
    }

    /// #631: the REST bin routes publish to pages that are usually closed. That
    /// must not create a page channel, and a later subscriber starts from an
    /// empty channel rather than an old notice.
    #[test]
    fn publishing_to_a_page_nobody_has_open_creates_no_channel() {
        let hub = RealtimeHub::default();
        hub.publish_page("page_closed", PageServerMessage::PageRestored);
        assert_eq!(hub.page_channel_count(), 0);

        let mut receiver = hub.subscribe_page("page_closed");
        hub.publish_page("page_closed", PageServerMessage::PageRestored);
        assert_eq!(
            receiver
                .try_recv()
                .expect("an open page is published to")
                .message,
            PageServerMessage::PageRestored
        );
        assert!(
            receiver.try_recv().is_err(),
            "nothing from before it opened"
        );
    }

    /// #633: the same for the library channel — thumbnails and REST routes
    /// publish to owners whose library is not open.
    #[test]
    fn publishing_to_an_owner_with_no_library_open_creates_no_channel() {
        let hub = RealtimeHub::default();
        hub.publish_library_event("owner_closed", page_deleted("page_1"));
        assert_eq!(hub.library_channel_count(), 0);
    }

    fn page_deleted(page_id: &str) -> LibraryEvent {
        LibraryEvent::PageDeleted {
            page_id: page_id.to_string(),
        }
    }

    /// #633: N page sessions across a few pages, closed in any order, leave no
    /// page channel behind — and a page keeps its channel while any session on
    /// it is still open.
    #[test]
    fn closing_every_page_subscription_leaves_no_page_channel() {
        let hub = RealtimeHub::default();
        let mut subscriptions: Vec<_> = (0..12)
            .map(|n| hub.subscribe_page(&format!("page_{}", n % 3)))
            .collect();
        assert_eq!(hub.page_channel_count(), 3);
        assert_eq!(hub.page_receiver_count("page_0"), 4);

        // Every session on page_0 but one.
        for n in [9, 6, 3] {
            drop(subscriptions.remove(n));
        }
        assert_eq!(hub.page_channel_count(), 3, "page_0 still has a session");
        assert_eq!(hub.page_receiver_count("page_0"), 1);

        drop(subscriptions.remove(0));
        assert_eq!(hub.page_channel_count(), 2, "page_0's last session closed");
        assert_eq!(hub.page_receiver_count("page_0"), 0);

        subscriptions.clear();
        assert_eq!(hub.page_channel_count(), 0);
    }

    /// #633: the same for library sockets, keyed by owner.
    #[test]
    fn closing_every_library_subscription_leaves_no_library_channel() {
        let hub = RealtimeHub::default();
        let mut subscriptions: Vec<_> = (0..10)
            .map(|n| hub.subscribe_library(&format!("owner_{}", n % 2)))
            .collect();
        assert_eq!(hub.library_channel_count(), 2);
        assert_eq!(hub.library_receiver_count("owner_1"), 5);

        subscriptions.retain(|_| false);
        assert_eq!(hub.library_channel_count(), 0);
        assert_eq!(hub.library_receiver_count("owner_1"), 0);
    }

    /// #633: a session that opens while the page's last other session is
    /// closing keeps the channel — the prune sees its receiver — and hears
    /// what is published next.
    #[test]
    fn a_subscriber_that_joins_before_the_last_one_leaves_keeps_the_channel() {
        let hub = RealtimeHub::default();
        let leaving = hub.subscribe_page("page_1");
        let mut joining = hub.subscribe_page("page_1");
        drop(leaving);

        assert_eq!(hub.page_channel_count(), 1);
        hub.publish_page("page_1", PageServerMessage::PageRestored);
        assert_eq!(
            joining.try_recv().expect("the channel survived").message,
            PageServerMessage::PageRestored
        );
    }

    /// #633: once a channel is pruned, the next session gets a fresh one and is
    /// published to like any other.
    #[test]
    fn a_subscriber_after_a_prune_gets_a_fresh_channel() {
        let hub = RealtimeHub::default();
        drop(hub.subscribe_library("owner_1"));
        assert_eq!(hub.library_channel_count(), 0);

        let mut again = hub.subscribe_library("owner_1");
        hub.publish_library_event("owner_1", page_deleted("page_1"));
        assert_eq!(
            again
                .try_recv()
                .expect("the fresh channel is published to")
                .message,
            page_deleted("page_1")
        );
    }

    /// #633, the race the prune must not lose: the last session drops on one
    /// thread while a new one subscribes on another. Whichever reaches the
    /// mutex first, the new session must end up on the channel a publish
    /// reaches, and nothing is left behind once it closes too.
    #[test]
    fn a_subscriber_racing_the_last_ones_drop_still_receives_fan_out() {
        use std::sync::Barrier;

        let hub = Arc::new(RealtimeHub::default());
        for round in 0..2_000 {
            let leaving = hub.subscribe_page("page_1");
            let start = Arc::new(Barrier::new(2));
            let dropper = {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    drop(leaving);
                })
            };
            start.wait();
            let mut joining = hub.subscribe_page("page_1");
            dropper.join().expect("the dropping thread finishes");

            hub.publish_page("page_1", PageServerMessage::Synced { last_seq: round });
            assert_eq!(
                joining
                    .try_recv()
                    .unwrap_or_else(|error| panic!("round {round}: fan-out lost ({error})"))
                    .message,
                PageServerMessage::Synced { last_seq: round }
            );
            assert_eq!(hub.page_receiver_count("page_1"), 1, "round {round}");
            drop(joining);
            assert_eq!(hub.page_channel_count(), 0, "round {round}");
        }
    }

    /// Single use, now that a ticket carries a trace as well as an identity: a
    /// replayed ticket must yield neither.
    #[test]
    fn a_ticket_redeems_exactly_once() {
        let hub = RealtimeHub::default();
        let issued = hub.issue_ticket("owner_1".to_string(), SpanContext::empty_context());

        assert!(hub.consume_ticket(&issued.ticket).is_some());
        assert!(hub.consume_ticket(&issued.ticket).is_none());
        assert!(hub.consume_ticket("ticket_never_issued").is_none());
    }

    #[test]
    fn fanout_without_a_publisher_trace_delivers_in_its_own_trace() {
        with_otel_layer(|| {
            let hub = RealtimeHub::default();
            let mut receiver = hub.subscribe_library("owner_1");
            hub.publish_library_event("owner_1", page_deleted("page_1"));

            let Fanout { message, origin } = receiver
                .try_recv()
                .expect("the published event should reach the subscriber");
            assert!(!origin.is_valid());

            let delivery = fanout_delivery_span("library", message.message_type(), &origin);
            assert_ne!(trace_id(&delivery), "00000000000000000000000000000000");
        });
    }
}
