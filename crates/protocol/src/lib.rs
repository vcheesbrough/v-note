use serde::{Deserialize, Serialize};

pub mod paper;

// Named re-exports only — a glob would pull `Paper::None` into scope and shadow
// `Option::None` at every call site.
pub use paper::{
    GRID_SPACING_LARGE, GRID_SPACING_SMALL, MARGIN_COLOR, MARGIN_COLOR_RGB, MARGIN_LINE_WIDTH,
    MARGIN_X, MAX_EXACT_PAPER_WORLD_EXTENT, MAX_PAPER_MARKS_PER_AXIS, MIN_PAPER_MARK_DEVICE_PITCH,
    MIN_PAPER_MARK_DEVICE_WIDTH, PAPER_TEXTURE_COLOR, PAPER_TEXTURE_COLOR_RGB,
    PAPER_TEXTURE_MAX_ALPHA, PAPER_TEXTURE_TILE_SIZE, Paper, PaperMark, PaperMarkKind, RULE_COLOR,
    RULE_COLOR_RGB, RULE_LINE_WIDTH, RULE_SPACING_NARROW, RULE_SPACING_WIDE, WorldViewport,
    is_ink_classified, paper_family_visible, paper_has_texture, paper_mark_device_width,
    paper_marks, paper_texture_alpha, paper_texture_tile, preview_viewport, visit_paper_marks,
};

pub const PROTOCOL_VERSION: u32 = 6;

/// The request-correlation header. Clients send one per HTTP call and WSS
/// handshake; the server echoes it on the response and stamps it on every log
/// line and span for that request. Lowercase because HTTP header names are
/// case-insensitive and the server builds its `HeaderName` from this literal.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

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
    /// The page's paper (rule lines). Always serialized, but defaulted on read
    /// so pre-v5 payloads still parse as blank pages.
    #[serde(default)]
    pub paper: Paper,
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
    /// Paper for the new page. Absent ⇒ `none`. Carrying it on create (rather
    /// than a post-create `SetPaper`) is how a client's sticky default lands:
    /// one round trip, no revision bump, page born with its paper.
    #[serde(default)]
    pub paper: Paper,
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
    PageUpdated {
        page_id: String,
        updated_at: String,
    },
}

impl LibraryEvent {
    /// The serde `type` tag, as a `'static` metric label.
    ///
    /// An exhaustive `match`, deliberately with no wildcard: a new variant does
    /// not compile until it names its label, so it can never land in an `other`
    /// bucket. `tests/message_types.rs` holds every label to the tag serde
    /// actually writes.
    pub fn message_type(&self) -> &'static str {
        match self {
            Self::PageCreated { .. } => "page-created",
            Self::PageDeleted { .. } => "page-deleted",
            Self::PageThumbnailUpdated { .. } => "page-thumbnail-updated",
            Self::PageUpdated { .. } => "page-updated",
        }
    }
}

// ---- Canonical ink (stroke geometry) -------------------------------------
//
// Strokes are authored in **world/document coordinates** on the infinite
// canvas — zoom/pan never rewrite stored geometry. A stroke is drawn with a
// single pen (`solid_round`, dark green `#006400` by default) whose nib width is
// modulated by per-point stylus `pressure` (see `docs/PLAN.md` → Stroke tool).

/// Stored styles deliberately carry a discriminator and version so later
/// rendering models cannot reinterpret historical ink.
pub const SOLID_ROUND_TOOL: &str = "solid_round";
/// `solid_round` v2: pressure-modulated nib width (see [`StrokeStyle::rendered_width`]).
/// Points MAY carry a normalised `pressure` in `0.0..=1.0`. The `width`
/// parameter is the full-pressure (`p = 1.0`) diameter.
///
/// This is the **only** accepted style version. A `solid_round` v1 once meant a
/// constant-width nib on which pressure was forbidden; it was retired in
/// iteration 48 and stored v1 ink was migrated up to v2 in place. That upgrade
/// is lossless precisely because a v2 stroke whose points carry no `pressure`
/// renders at constant full `width` — identical to what v1 drew. The number `1`
/// is never reused: it stays permanently invalid so a `2` on the wire means
/// "pressure-modulated" in historical rows and new ones alike.
pub const SOLID_ROUND_PRESSURE_STYLE_VERSION: u32 = 2;
pub const DEFAULT_PEN_COLOR: &str = "#006400";
pub const DEFAULT_PEN_WIDTH: f64 = 4.0;
pub const MIN_PEN_WIDTH: f64 = 1.0;
pub const MAX_PEN_WIDTH: f64 = 32.0;

/// Rendered nib diameter at zero pressure, in world logical pixels — an
/// **absolute** floor, not a fraction of the preset. The shared, cross-platform
/// pressure→width curve interpolates linearly from this floor up to the preset
/// width, so a heavy pen still tapers to a thin line at light pressure:
///
/// `width(p) = min_floor + (preset_width − min_floor) × p`,
/// where `min_floor = min(MIN_PRESSURE_WIDTH, preset_width)`.
///
/// This constant is the single source of truth for the curve; Android mirrors
/// the same value in Kotlin. It is part of the style's defined semantics, not a
/// stored parameter.
pub const MIN_PRESSURE_WIDTH: f64 = 1.5;

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
    /// Pressure-modulated pen in the default colour and width; the `width` is
    /// the full-pressure (`p = 1.0`) diameter.
    pub fn default_solid_round_pressure() -> Self {
        Self {
            tool_kind: SOLID_ROUND_TOOL.to_string(),
            style_version: SOLID_ROUND_PRESSURE_STYLE_VERSION,
            parameters: SolidRoundParameters {
                color: DEFAULT_PEN_COLOR.to_string(),
                width: DEFAULT_PEN_WIDTH,
                cap_style: "round".to_string(),
                join_style: "round".to_string(),
            },
        }
    }

    /// Rendered nib diameter (world-space logical pixels) for a point carrying
    /// the given optional pressure. This is the single, shared cross-platform
    /// curve — every renderer (thumbnails, SPA, Android) must derive per-point
    /// width from here so geometry matches.
    ///
    /// Linear interpolation from an absolute [`MIN_PRESSURE_WIDTH`] floor
    /// (capped at the preset for very thin pens) up to the preset `width`, with
    /// `p` clamped to `0.0..=1.0`. A point with no pressure renders at full
    /// `width` (`p = 1.0`), which is what migrated pressure-less ink relies on.
    pub fn rendered_width(&self, pressure: Option<f64>) -> f64 {
        let p = pressure.unwrap_or(1.0).clamp(0.0, 1.0);
        let preset = self.parameters.width;
        let floor = MIN_PRESSURE_WIDTH.min(preset);
        floor + (preset - floor) * p
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tool_kind != SOLID_ROUND_TOOL
            || self.style_version != SOLID_ROUND_PRESSURE_STYLE_VERSION
        {
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
    /// Normalised stylus pressure in `0.0..=1.0`. Optional: a point without
    /// pressure renders at full width (see [`StrokeStyle::rendered_width`]),
    /// which is how ink migrated up from the retired v1 style keeps its
    /// original constant-width appearance.
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

impl Stroke {
    /// Validate the style and the per-point pressure invariants together:
    /// pressure is optional on every point, but when present must be a finite
    /// value in `0.0..=1.0`. Out-of-range pressure is rejected, never silently
    /// clamped — mirroring the width/colour posture in
    /// [`StrokeStyle::validate`].
    pub fn validate(&self) -> Result<(), &'static str> {
        self.style.validate()?;
        for point in &self.points {
            if let Some(pressure) = point.pressure
                && (!pressure.is_finite() || !(0.0..=1.0).contains(&pressure))
            {
                return Err("pressure must be finite in 0.0..=1.0");
            }
        }
        Ok(())
    }
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
/// page-replay contract and as the server snapshot payload shape: one frame
/// carries every surviving stroke batch, every tombstone batch, and the head
/// `seq` the subscriber is now caught up to, so a dense page costs one frame
/// instead of one per batch (card #323).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageReplay {
    pub page_id: String,
    pub last_seq: u64,
    pub batches: Vec<StrokeBatch>,
    /// Every tombstone batch on the page, replayed even when `from_seq` skips
    /// the stroke batches they deleted from — a reconnecting client may still
    /// hold those strokes. `default` keeps the reader tolerant while the schema
    /// keeps this `required`, so the server must always emit it.
    #[serde(default)]
    pub tombstones: Vec<TombstoneBatch>,
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
    /// Change the page's paper. Requires the edit lease — paper is a visible
    /// page mutation that bumps the revision, mints a thumbnail and re-sorts the
    /// library, exactly the class the single-editor invariant governs.
    /// `client_mutation_id` exists purely so an `Error` can be correlated back.
    SetPaper {
        client_mutation_id: String,
        paper: Paper,
    },
}

impl PageClientMessage {
    /// The serde `type` tag, as a `'static` metric label — see
    /// [`LibraryEvent::message_type`] for why the match has no wildcard.
    pub fn message_type(&self) -> &'static str {
        match self {
            Self::Subscribe { .. } => "subscribe",
            Self::AcquireLease => "acquire-lease",
            Self::RenewLease => "renew-lease",
            Self::ReleaseLease => "release-lease",
            Self::CommitBatch { .. } => "commit-batch",
            Self::CommitTombstones { .. } => "commit-tombstones",
            Self::SetPaper { .. } => "set-paper",
        }
    }
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
        /// The page's current paper, so the page channel is self-sufficient
        /// after a reconnect. Defaulted on read, so pre-v5 welcome payloads
        /// still parse.
        #[serde(default)]
        paper: Paper,
    },
    /// A sequenced stroke batch, fanned out live to the page's other sessions
    /// on commit. Replay no longer uses this — see [`Self::PageReplay`].
    StrokeBatch(StrokeBatch),
    /// The whole `subscribe` replay in one frame: surviving batches, every
    /// tombstone batch, and the head `seq`. Subsumes the N x `stroke-batch` +
    /// M x `tombstone-batch` + `synced` sequence it replaced, so receiving it
    /// means the session is caught up to `last_seq`.
    PageReplay(PageReplay),
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
    /// The page's paper is now `paper` as of `revision`. Broadcast on a real
    /// change; also sent directly as an ack when a `SetPaper` names the paper
    /// already in force, so a racing client still converges.
    PaperChanged {
        paper: Paper,
        revision: u64,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_mutation_id: Option<String>,
    },
}

impl PageServerMessage {
    /// The serde `type` tag, as a `'static` metric label — see
    /// [`LibraryEvent::message_type`] for why the match has no wildcard.
    pub fn message_type(&self) -> &'static str {
        match self {
            Self::Welcome { .. } => "welcome",
            Self::StrokeBatch(_) => "stroke-batch",
            Self::PageReplay(_) => "page-replay",
            Self::TombstoneBatch(_) => "tombstone-batch",
            Self::Synced { .. } => "synced",
            Self::LeaseGranted => "lease-granted",
            Self::LeaseDenied { .. } => "lease-denied",
            Self::PaperChanged { .. } => "paper-changed",
            Self::LeaseChanged { .. } => "lease-changed",
            Self::Error { .. } => "error",
        }
    }
}
