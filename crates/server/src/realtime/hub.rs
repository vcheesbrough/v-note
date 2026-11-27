//! In-process realtime state: tickets and per-owner and per-page broadcast
//! channels. No transport, no SQL.

use std::collections::HashMap;
use std::sync::Mutex;

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

#[derive(Default)]
pub struct RealtimeHub {
    tickets: Mutex<HashMap<String, Ticket>>,
    library_channels: Mutex<HashMap<String, broadcast::Sender<Fanout<LibraryEvent>>>>,
    page_channels: Mutex<HashMap<String, broadcast::Sender<Fanout<PageServerMessage>>>>,
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

    pub(super) fn subscribe_library(
        &self,
        owner_id: &str,
    ) -> broadcast::Receiver<Fanout<LibraryEvent>> {
        let mut channels = self
            .library_channels
            .lock()
            .expect("library channel mutex poisoned");
        channels
            .entry(owner_id.to_string())
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(LIBRARY_CHANNEL_CAPACITY);
                sender
            })
            .subscribe()
    }

    pub fn publish_library_event(&self, owner_id: &str, event: LibraryEvent) {
        let sender = {
            let mut channels = self
                .library_channels
                .lock()
                .expect("library channel mutex poisoned");
            channels
                .entry(owner_id.to_string())
                .or_insert_with(|| {
                    let (sender, _) = broadcast::channel(LIBRARY_CHANNEL_CAPACITY);
                    sender
                })
                .clone()
        };
        let event_type = event.message_type();
        // `Err` only means nobody is subscribed right now, which is normal.
        let receivers = sender.send(Fanout::from_current_span(event)).unwrap_or(0);
        tracing::debug!(
            owner_id = %owner_id,
            event_type,
            receivers,
            "library event published"
        );
    }

    pub(super) fn subscribe_page(
        &self,
        page_id: &str,
    ) -> broadcast::Receiver<Fanout<PageServerMessage>> {
        let mut channels = self
            .page_channels
            .lock()
            .expect("page channel mutex poisoned");
        channels
            .entry(page_id.to_string())
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(PAGE_CHANNEL_CAPACITY);
                sender
            })
            .subscribe()
    }

    /// Fans `message` out to every session on the page. A page nobody has
    /// open has no channel, and publishing to it is a no-op rather than one
    /// that creates a channel: the REST bin routes (#631) publish to pages
    /// that are usually closed, and a created channel is never pruned.
    pub fn publish_page(&self, page_id: &str, message: PageServerMessage) {
        let Some(sender) = self
            .page_channels
            .lock()
            .expect("page channel mutex poisoned")
            .get(page_id)
            .cloned()
        else {
            tracing::debug!(
                page_id = %page_id,
                message_type = message.message_type(),
                "page message not published: no session has the page open"
            );
            return;
        };
        let message_type = message.message_type();
        let receivers = sender.send(Fanout::from_current_span(message)).unwrap_or(0);
        tracing::debug!(
            page_id = %page_id,
            message_type,
            receivers,
            "page message published"
        );
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

    /// Single use, now that a ticket carries a trace as well as an identity: a
    /// replayed ticket must yield neither.
    /// #631: the REST bin routes publish to pages that are usually closed. That
    /// must not create a page channel (nothing prunes one), and a later
    /// subscriber starts from an empty channel rather than an old notice.
    #[test]
    fn publishing_to_a_page_nobody_has_open_creates_no_channel() {
        let hub = RealtimeHub::default();
        hub.publish_page("page_closed", PageServerMessage::PageRestored);
        assert!(
            hub.page_channels
                .lock()
                .expect("page channel mutex poisoned")
                .is_empty()
        );

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
            hub.publish_library_event(
                "owner_1",
                LibraryEvent::PageDeleted {
                    page_id: "page_1".to_string(),
                },
            );

            let Fanout { message, origin } = receiver
                .try_recv()
                .expect("the published event should reach the subscriber");
            assert!(!origin.is_valid());

            let delivery = fanout_delivery_span("library", message.message_type(), &origin);
            assert_ne!(trace_id(&delivery), "00000000000000000000000000000000");
        });
    }
}
