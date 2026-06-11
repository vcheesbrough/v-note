use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaResponse {
    pub app_name: String,
    pub app_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeResponse {
    pub sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListPagesResponse {
    pub pages: Vec<PageSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageResponse {
    pub page: PageSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreatePageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RealtimeTicketResponse {
    pub ticket: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LibraryEvent {
    PageCreated { page: PageSummary },
    PageDeleted { page_id: String },
}

// ---- Canonical ink (stroke geometry) -------------------------------------
//
// Strokes are authored in **world/document coordinates** on the infinite
// canvas — zoom/pan never rewrite stored geometry. MVP uses a single hardcoded
// pen (dark green `#006400`, constant 4px logical width, no pressure). `pressure`
// is reserved on each point so post-MVP pressure/tilt curves extend the schema
// without breaking v1 strokes (see `docs/PLAN.md` → Stroke tool).

/// MVP pen constants — single hardcoded tool.
pub const PEN_TOOL: &str = "pen";
pub const PEN_COLOR: &str = "#006400";
pub const PEN_WIDTH: f64 = 4.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrokePoint {
    pub x: f64,
    pub y: f64,
    /// Milliseconds relative to the start of the stroke.
    pub t: i64,
    /// MVP omits pressure; reserved for post-MVP pressure/tilt curves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stroke {
    /// Constant `"pen"` in MVP (single hardcoded tool).
    pub tool: String,
    /// Constant `#006400` dark green in MVP.
    pub color: String,
    /// Constant 4.0 logical px in MVP (no pressure→width mapping).
    pub width: f64,
    pub points: Vec<StrokePoint>,
}

/// A persisted, server-sequenced batch of coalesced strokes on a page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrokeBatch {
    /// Per-page monotonic sequence assigned by the server on commit.
    pub seq: u64,
    /// Client-generated idempotency key; lets reconnect retries dedupe.
    pub client_batch_id: String,
    pub strokes: Vec<Stroke>,
}

/// Full ordered replay of a page's ink (snapshot). Used as the golden
/// page-replay contract and as the server snapshot payload shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageReplay {
    pub page_id: String,
    pub last_seq: u64,
    pub batches: Vec<StrokeBatch>,
}

// ---- Page channel (per-`page_id` WSS) ------------------------------------
//
// Bidirectional, one duplex connection per open page. The client subscribes
// for gap-fill/snapshot, acquires the single-editor edit lease, and commits
// coalesced stroke batches; the server fans out ordered deltas to the owner's
// sibling sessions on that page (see `docs/PLAN.md` → Wire protocol v1,
// Multi-device / concurrent edit).

/// Client → server messages on the page channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PageClientMessage {
    /// Sent right after connect: request gap-fill of batches with `seq > from_seq`
    /// (`from_seq = 0` ⇒ full snapshot).
    Subscribe { from_seq: u64 },
    /// Request the single-editor edit lease before inking.
    AcquireLease,
    /// Voluntarily release the edit lease (navigation away).
    ReleaseLease,
    /// Commit a coalesced stroke batch (idempotent by `client_batch_id`).
    CommitBatch {
        client_batch_id: String,
        strokes: Vec<Stroke>,
    },
}

/// Server → client messages on the page channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PageServerMessage {
    /// First message after connect: session identity, persisted head `seq`,
    /// and the current lease holder (if any).
    Welcome {
        session_id: String,
        last_seq: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        lease_holder: Option<String>,
    },
    /// A sequenced stroke batch (gap-fill replay, snapshot, or live fan-out).
    StrokeBatch(StrokeBatch),
    /// End of gap-fill replay; this session is caught up to `last_seq`.
    Synced { last_seq: u64 },
    /// This session now holds the edit lease.
    LeaseGranted,
    /// The edit lease is held by another session; ink is blocked.
    LeaseDenied { holder: String },
    /// Broadcast when the page's lease holder changes (or clears → omitted).
    LeaseChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
    },
    /// Recoverable error envelope (e.g. rejected commit).
    Error { code: String, message: String },
}
