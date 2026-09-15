//! Realtime WSS adapter — the "one adapter module" of `docs/PLAN.md` →
//! *Realtime transport (chosen)*.
//!
//! - `hub`: in-memory fan-out channels, tickets and the edit lease;
//! - `socket`: the axum upgrade handlers and per-connection loops;
//! - `dispatch`: what each client message does, independent of the socket;
//! - `store`: the Postgres side of the page channel.

mod dispatch;
mod hub;
mod socket;
mod store;

pub use hub::RealtimeHub;
pub use socket::{page_socket, realtime_socket, realtime_ticket};
