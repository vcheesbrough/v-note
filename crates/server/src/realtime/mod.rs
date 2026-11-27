//! Realtime WSS adapter — the "one adapter module" of `docs/PLAN.md` →
//! *Realtime transport (chosen)*.
//!
//! - `hub`: in-memory fan-out channels and tickets;
//! - `socket`: the axum upgrade handlers and per-connection loops;
//! - `dispatch`: what each client message does, independent of the socket;
//! - `store`: the Postgres side of the page channel.

mod dispatch;
mod hub;
mod socket;
mod store;

pub(crate) use dispatch::page_deleted_error;
pub use hub::RealtimeHub;
pub use socket::{page_socket, realtime_socket, realtime_ticket};
pub(crate) use store::page_was_purged;
