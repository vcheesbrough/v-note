use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;

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
    pub thumbnail: ThumbnailMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ThumbnailMetadata {
    Empty,
    Generating { source_seq: u64 },
    Available { source_seq: u64, url: String },
    Failed { source_seq: u64 },
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
    PageCreated {
        page: PageSummary,
    },
    PageDeleted {
        page_id: String,
    },
    PageThumbnailUpdated {
        page_id: String,
        thumbnail: ThumbnailMetadata,
    },
}

// ---- Canonical ink (stroke geometry) -------------------------------------
//
// Strokes are authored in **world/document coordinates** on the infinite
// canvas — zoom/pan never rewrite stored geometry. MVP uses a single hardcoded
// pen (dark green `#006400`, constant 4px logical width, no pressure). `pressure`
// is reserved on each point so post-MVP pressure/tilt curves extend the schema
// without breaking v1 strokes (see `docs/PLAN.md` → Stroke tool).

/// The only style accepted by protocol v3. Stored styles deliberately carry a
/// discriminator and version so later rendering models cannot reinterpret
/// historical ink.
pub const SOLID_ROUND_TOOL: &str = "solid_round";
pub const SOLID_ROUND_STYLE_VERSION: u32 = 1;
pub const DEFAULT_PEN_COLOR: &str = "#006400";
pub const DEFAULT_PEN_WIDTH: f64 = 4.0;
pub const MIN_PEN_WIDTH: f64 = 1.0;
pub const MAX_PEN_WIDTH: f64 = 32.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SolidRoundParameters {
    pub color: String,
    pub width: f64,
    pub cap_style: String,
    pub join_style: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrokeStyle {
    pub tool_kind: String,
    pub style_version: u32,
    pub parameters: SolidRoundParameters,
}

impl StrokeStyle {
    pub fn default_solid_round() -> Self {
        Self {
            tool_kind: SOLID_ROUND_TOOL.to_string(),
            style_version: SOLID_ROUND_STYLE_VERSION,
            parameters: SolidRoundParameters {
                color: DEFAULT_PEN_COLOR.to_string(),
                width: DEFAULT_PEN_WIDTH,
                cap_style: "round".to_string(),
                join_style: "round".to_string(),
            },
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tool_kind != SOLID_ROUND_TOOL || self.style_version != SOLID_ROUND_STYLE_VERSION {
            return Err("unsupported style");
        }
        let parameters = &self.parameters;
        if parameters.cap_style != "round" || parameters.join_style != "round" {
            return Err("solid_round requires round cap and join");
        }
        let color = &parameters.color;
        if color.len() != 7
            || !color.starts_with('#')
            || !color[1..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
        {
            return Err("color must be uppercase #RRGGBB");
        }
        let width = parameters.width;
        if !width.is_finite()
            || !(MIN_PEN_WIDTH..=MAX_PEN_WIDTH).contains(&width)
            || ((width * 2.0).round() - width * 2.0).abs() > f64::EPSILON
        {
            return Err("width must be 1.0..32.0 in 0.5 increments");
        }
        Ok(())
    }
}

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
    /// Client-assigned immutable identity. Tombstones target this id.
    pub id: String,
    /// Immutable style snapshot captured at stylus-down.
    pub style: StrokeStyle,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TombstoneBatch {
    pub revision: u64,
    pub client_mutation_id: String,
    pub stroke_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolPreset {
    pub id: String,
    pub revision: u64,
    pub style: StrokeStyle,
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
    /// Renew the single-editor edit lease while this page remains active.
    RenewLease,
    /// Voluntarily release the edit lease (navigation away).
    ReleaseLease,
    /// Commit a coalesced stroke batch (idempotent by `client_batch_id`).
    CommitBatch {
        client_batch_id: String,
        strokes: Vec<Stroke>,
    },
    /// Permanently hide complete strokes by their stable ids.
    CommitTombstones {
        client_mutation_id: String,
        stroke_ids: Vec<String>,
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
    TombstoneBatch(TombstoneBatch),
    /// End of gap-fill replay; this session is caught up to `last_seq`.
    Synced {
        last_seq: u64,
    },
    /// This session now holds the edit lease.
    LeaseGranted,
    /// The edit lease is held by another session; ink is blocked.
    LeaseDenied {
        holder: String,
    },
    /// Broadcast when the page's lease holder changes (or clears → omitted).
    LeaseChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
    },
    /// Recoverable error envelope (e.g. rejected commit).
    Error {
        code: String,
        message: String,
    },
}
