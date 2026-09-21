//! In-process realtime state: tickets, per-owner and per-page broadcast
//! channels, and the single-editor edit lease. No transport, no SQL.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use opentelemetry::trace::SpanContext;
use protocol::{LibraryEvent, PageServerMessage, RealtimeTicketResponse};
use rand::Rng;
use tokio::sync::broadcast;

const TICKET_TTL_SECONDS: i64 = 60;
const LIBRARY_CHANNEL_CAPACITY: usize = 64;
const PAGE_CHANNEL_CAPACITY: usize = 256;
const LEASE_TTL_SECONDS: i64 = 30;

#[derive(Default)]
pub struct RealtimeHub {
    tickets: Mutex<HashMap<String, Ticket>>,
    library_channels: Mutex<HashMap<String, broadcast::Sender<Fanout<LibraryEvent>>>>,
    page_channels: Mutex<HashMap<String, broadcast::Sender<Fanout<PageServerMessage>>>>,
    leases: Mutex<HashMap<String, Lease>>,
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
/// current span. Its own trace when the publisher had none (e.g. a lease
/// released on disconnect outside any request).
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

struct Lease {
    holder: String,
    expires_at: DateTime<Utc>,
}

pub(super) enum LeaseOutcome {
    Granted,
    Denied { holder: String },
}

impl RealtimeHub {
    pub fn issue_ticket(&self, owner_id: String, origin: SpanContext) -> RealtimeTicketResponse {
        let ticket = format!("ticket_{}", random_hex(32));
        let expires_at = Utc::now() + Duration::seconds(TICKET_TTL_SECONDS);
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
        let ticket = tickets.remove(ticket)?;
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
        let _ = sender.send(Fanout::from_current_span(event));
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

    pub(super) fn publish_page(&self, page_id: &str, message: PageServerMessage) {
        let sender = {
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
                .clone()
        };
        let _ = sender.send(Fanout::from_current_span(message));
    }

    /// Acquire (or renew, for the current holder) the single-editor edit lease.
    pub(super) fn acquire_lease(&self, page_id: &str, session_id: &str) -> LeaseOutcome {
        let now = Utc::now();
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        if let Some(existing) = leases.get(page_id)
            && existing.expires_at <= now
        {
            leases.remove(page_id);
        }
        match leases.get_mut(page_id) {
            Some(existing) if existing.holder == session_id => {
                existing.expires_at = now + Duration::seconds(LEASE_TTL_SECONDS);
                LeaseOutcome::Granted
            }
            Some(existing) => LeaseOutcome::Denied {
                holder: existing.holder.clone(),
            },
            None => {
                leases.insert(
                    page_id.to_string(),
                    Lease {
                        holder: session_id.to_string(),
                        expires_at: now + Duration::seconds(LEASE_TTL_SECONDS),
                    },
                );
                LeaseOutcome::Granted
            }
        }
    }

    /// Release the lease if held by `session_id`; returns true when released.
    pub(super) fn release_lease(&self, page_id: &str, session_id: &str) -> bool {
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        match leases.get(page_id) {
            Some(existing) if existing.holder == session_id => {
                leases.remove(page_id);
                true
            }
            _ => false,
        }
    }

    pub(super) fn current_lease_holder(&self, page_id: &str) -> Option<String> {
        let now = Utc::now();
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        match leases.get(page_id) {
            Some(existing) if existing.expires_at > now => Some(existing.holder.clone()),
            Some(_) => {
                leases.remove(page_id);
                None
            }
            None => None,
        }
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
            publisher.in_scope(|| hub.publish_page("page_1", PageServerMessage::LeaseGranted));

            let Fanout { message, origin } = receiver
                .try_recv()
                .expect("the published message should reach the subscriber");
            assert_eq!(message, PageServerMessage::LeaseGranted);
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

    #[test]
    fn edit_lease_grants_then_blocks_second_session() {
        let hub = RealtimeHub::default();

        // First session acquires; the holder may renew freely.
        assert!(matches!(
            hub.acquire_lease("page_1", "session_a"),
            LeaseOutcome::Granted
        ));
        assert!(matches!(
            hub.acquire_lease("page_1", "session_a"),
            LeaseOutcome::Granted
        ));

        // A second session is blocked and told the current holder.
        match hub.acquire_lease("page_1", "session_b") {
            LeaseOutcome::Denied { holder } => assert_eq!(holder, "session_a"),
            LeaseOutcome::Granted => panic!("second session should be blocked"),
        }
        assert_eq!(
            hub.current_lease_holder("page_1").as_deref(),
            Some("session_a")
        );

        // A non-holder cannot release the lease.
        assert!(!hub.release_lease("page_1", "session_b"));

        // The holder releases and the page frees up for the next session.
        assert!(hub.release_lease("page_1", "session_a"));
        assert_eq!(hub.current_lease_holder("page_1"), None);
        assert!(matches!(
            hub.acquire_lease("page_1", "session_b"),
            LeaseOutcome::Granted
        ));
    }
}
