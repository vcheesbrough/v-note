use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use protocol::{
    CreatePageRequest, LibraryEvent, ListPagesResponse, PageResponse, PageServerMessage,
    PageSummary, Paper, ThumbnailMetadata,
};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Claims;
use crate::observability::{db_query_span, metered};

#[derive(sqlx::FromRow)]
struct PageRow {
    id: String,
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    thumbnail_status: Option<String>,
    thumbnail_seq: Option<i64>,
    paper: String,
    binned_at: Option<DateTime<Utc>>,
}

impl From<PageRow> for PageSummary {
    fn from(row: PageRow) -> Self {
        let id = row.id;
        let thumbnail = match (row.thumbnail_status.as_deref(), row.thumbnail_seq) {
            (Some("generating"), Some(seq)) => ThumbnailMetadata::Generating {
                source_seq: seq as u64,
            },
            (Some("available"), Some(seq)) => ThumbnailMetadata::Available {
                source_seq: seq as u64,
                url: crate::thumbnails::thumbnail_url(&id, seq as u64),
            },
            (Some("failed"), Some(seq)) => ThumbnailMetadata::Failed {
                source_seq: seq as u64,
            },
            _ => ThumbnailMetadata::Empty,
        };
        Self {
            id,
            title: row.title,
            created_at: row.created_at.to_rfc3339(),
            updated_at: row.updated_at.to_rfc3339(),
            thumbnail,
            // A stored value outside the wire vocabulary is only reachable if
            // the `pages_paper_known` CHECK were dropped; degrade to a blank
            // page rather than failing the whole listing.
            paper: Paper::from_wire(&row.paper).unwrap_or_default(),
            binned_at: row.binned_at.map(|at| at.to_rfc3339()),
        }
    }
}

/// A page read: `$from` names the page rows as `p` (the table, or a CTE with
/// its columns), the newest thumbnail is joined in, and `$rest` filters and
/// orders. A macro rather than `format!` so every query stays a `'static`
/// literal, which is what sqlx accepts as audited SQL.
macro_rules! page_query {
    ($prefix:literal, $from:literal, $rest:literal) => {
        concat!(
            $prefix,
            " SELECT p.id, p.title, p.created_at, p.updated_at, p.paper, p.binned_at,
                     t.status AS thumbnail_status, t.source_seq AS thumbnail_seq
              FROM ",
            $from,
            " LEFT JOIN LATERAL (
                SELECT status, source_seq FROM page_thumbnails
                WHERE page_id = p.id ORDER BY source_seq DESC LIMIT 1
              ) t ON true ",
            $rest
        )
    };
}

/// A page-route failure, as the small half of every `Result` in this module.
///
/// The handlers used to carry an already-built `Response` in the `Err` variant.
/// A `Response` is 128 bytes, so every `Result` here — success path included —
/// was sized by its error (`clippy::result_large_err`). A status plus a static
/// message is 24, and axum renders the identical response through
/// `IntoResponse`: same code, same `text/plain` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    const PAGE_NOT_FOUND: Self = Self {
        status: StatusCode::FORBIDDEN,
        message: "page not found",
    };
    const THUMBNAIL_GONE: Self = Self {
        status: StatusCode::GONE,
        message: "thumbnail revision no longer available",
    };
    /// The page was permanently deleted (#631). Its own status, so a client
    /// tells "gone for good — you hold the only copy" from not-found.
    const PAGE_PURGED: Self = Self {
        status: StatusCode::GONE,
        message: "page permanently deleted",
    };
    /// Permanent deletion is only ever from the bin.
    const NOT_IN_BIN: Self = Self {
        status: StatusCode::CONFLICT,
        message: "page is not in the bin",
    };
    const DB_OPERATION_FAILED: Self = Self {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "database operation failed",
    };
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

#[tracing::instrument(skip_all, fields(owner_id = %claims.sub))]
pub async fn list_pages(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<ListPagesResponse>, ApiError> {
    // Binned pages (#631) are not in the library; `GET /api/bin` lists them.
    let rows = sqlx::query_as::<_, PageRow>(page_query!(
        "",
        "pages p",
        "WHERE p.owner_id = $1 AND p.binned_at IS NULL
         ORDER BY p.updated_at DESC, p.created_at DESC"
    ))
    .bind(claims.sub.clone())
    .fetch_all(metered(&state.db))
    .instrument(db_query_span!("SELECT", "list_pages"))
    .await
    .map_err(server_error)?;

    // The count is the `db.query` span's `db.response.returned_rows`.
    tracing::debug!("pages listed");
    Ok(Json(ListPagesResponse {
        pages: rows.into_iter().map(PageSummary::from).collect(),
    }))
}

#[tracing::instrument(skip_all, fields(owner_id = %claims.sub, page_id))]
pub async fn create_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(payload): Json<CreatePageRequest>,
) -> Result<(StatusCode, Json<PageResponse>), ApiError> {
    let title = payload
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let page_id = format!("page_{}", Uuid::new_v4().simple());
    tracing::Span::current().record("page_id", page_id.as_str());

    // A page is born with its paper — one round trip, no revision bump, and no
    // thumbnail job (a page with no ink has nothing to preview yet). Serde has
    // already rejected any value outside the wire vocabulary.
    let row = sqlx::query_as::<_, PageRow>(
        r#"
        INSERT INTO pages (id, owner_id, title, paper)
        VALUES ($1, $2, $3, $4)
        RETURNING id, title, created_at, updated_at, paper, binned_at, NULL::text AS thumbnail_status, NULL::bigint AS thumbnail_seq
        "#,
    )
    .bind(page_id)
    .bind(claims.sub.clone())
    .bind(title)
    .bind(payload.paper.wire_value())
    .fetch_one(metered(&state.db))
    .instrument(db_query_span!("INSERT", "create_page"))
    .await
    .map_err(|error| {
        crate::observability::metrics().record_page_mutation("create_page", "error");
        server_error(error)
    })?;

    let page = PageSummary::from(row);
    tracing::info!(
        page_id = %page.id,
        paper = page.paper.wire_value(),
        titled = !page.title.is_empty(),
        "page created",
    );
    state.realtime.publish_library_event(
        &claims.sub,
        LibraryEvent::PageCreated { page: page.clone() },
    );
    crate::observability::metrics().record_page_mutation("create_page", "success");
    Ok((StatusCode::CREATED, Json(PageResponse { page })))
}

#[tracing::instrument(skip_all, fields(page_id = %page_id, owner_id = %claims.sub))]
pub async fn get_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<Json<PageResponse>, ApiError> {
    // A binned page is still readable (with `binned_at` set): the bin shows it.
    let row = sqlx::query_as::<_, PageRow>(page_query!(
        "",
        "pages p",
        "WHERE p.id = $1 AND p.owner_id = $2"
    ))
    .bind(&page_id)
    .bind(&claims.sub)
    .fetch_optional(metered(&state.db))
    .instrument(db_query_span!("SELECT", "get_page"))
    .await
    .map_err(server_error)?;

    match row {
        Some(row) => {
            tracing::debug!("page read");
            Ok(Json(PageResponse {
                page: PageSummary::from(row),
            }))
        }
        None => Err(missing_page(&state, &page_id, &claims.sub, "page read").await),
    }
}

#[tracing::instrument(
    skip_all,
    fields(page_id = %page_id, source_seq = source_seq, owner_id = %claims.sub)
)]
pub async fn get_thumbnail(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path((page_id, source_seq)): Path<(String, u64)>,
) -> Result<Response, ApiError> {
    let owned: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pages WHERE id = $1 AND owner_id = $2)")
            .bind(&page_id)
            .bind(&claims.sub)
            .fetch_one(metered(&state.db))
            .instrument(db_query_span!("SELECT", "get_thumbnail_owner"))
            .await
            .map_err(server_error)?;
    if !owned {
        tracing::warn!(
            page_id = %page_id,
            source_seq,
            "thumbnail read rejected: page not found or not owned by the caller"
        );
        return Err(ApiError::PAGE_NOT_FOUND);
    }
    let png = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT png FROM page_thumbnails WHERE page_id = $1 AND source_seq = $2 AND status = 'available'",
    )
    .bind(&page_id)
    .bind(source_seq as i64)
    .fetch_optional(metered(&state.db))
    .instrument(db_query_span!("SELECT", "get_thumbnail_png"))
    .await
    .map_err(server_error)?;
    match png {
        Some(bytes) => {
            tracing::debug!("thumbnail served");
            Ok((
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (
                        header::CACHE_CONTROL,
                        "private, max-age=31536000, immutable",
                    ),
                ],
                bytes,
            )
                .into_response())
        }
        None => {
            // A superseded or still-generating revision: the client asked for an
            // artifact that retention (or a newer commit) has moved past.
            tracing::warn!(
                page_id = %page_id,
                source_seq,
                "thumbnail read rejected: revision not available"
            );
            Err(ApiError::THUMBNAIL_GONE)
        }
    }
}

/// Why a page operation found no page for its caller: permanently deleted
/// (`410`, #631), or not found or not theirs — one answer on purpose (a 403
/// either way), so the log line does not distinguish those two either.
async fn missing_page(state: &AppState, page_id: &str, owner_id: &str, what: &str) -> ApiError {
    match crate::realtime::page_was_purged(&state.db, page_id, owner_id).await {
        Ok(true) => {
            tracing::warn!(page_id, "{what} rejected: the page was permanently deleted");
            ApiError::PAGE_PURGED
        }
        Ok(false) => {
            tracing::warn!(
                page_id,
                "{what} rejected: not found or not owned by the caller"
            );
            ApiError::PAGE_NOT_FOUND
        }
        Err(error) => server_error(error),
    }
}

/// Reads one of the caller's pages, live or binned.
async fn owned_page(
    state: &AppState,
    page_id: &str,
    owner_id: &str,
) -> Result<Option<PageSummary>, ApiError> {
    let row = sqlx::query_as::<_, PageRow>(page_query!(
        "",
        "pages p",
        "WHERE p.id = $1 AND p.owner_id = $2"
    ))
    .bind(page_id)
    .bind(owner_id)
    .fetch_optional(metered(&state.db))
    .instrument(db_query_span!("SELECT", "owned_page"))
    .await
    .map_err(server_error)?;
    Ok(row.map(PageSummary::from))
}

/// `DELETE /api/pages/{id}` — moves the page to the bin (#631). Nothing is
/// destroyed: ink, tombstones and thumbnails stay, the page simply leaves the
/// library (and search, once #151 lands) on every client. Idempotent: binning
/// a binned page is a `204` that changes and announces nothing.
#[tracing::instrument(skip_all, fields(page_id = %page_id, owner_id = %claims.sub))]
pub async fn delete_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let metrics = crate::observability::metrics();
    // `updated_at` is deliberately untouched: it means "last edited", and a
    // restored page goes back to where it was in the library.
    let binned = sqlx::query_as::<_, PageRow>(page_query!(
        "WITH p AS (
           UPDATE pages SET binned_at = now()
           WHERE id = $1 AND owner_id = $2 AND binned_at IS NULL
           RETURNING id, title, created_at, updated_at, paper, binned_at
         )",
        "p",
        ""
    ))
    .bind(&page_id)
    .bind(&claims.sub)
    .fetch_optional(metered(&state.db))
    .instrument(db_query_span!("UPDATE", "bin_page"))
    .await
    .map_err(|error| {
        metrics.record_page_mutation("bin_page", "error");
        server_error(error)
    })?;

    let Some(row) = binned else {
        return match owned_page(&state, &page_id, &claims.sub).await? {
            Some(_) => {
                tracing::debug!("page already in the bin");
                metrics.record_page_mutation("bin_page", "noop");
                Ok(StatusCode::NO_CONTENT)
            }
            None => {
                metrics.record_page_mutation("bin_page", "not_found");
                Err(missing_page(&state, &page_id, &claims.sub, "page bin").await)
            }
        };
    };

    let page = PageSummary::from(row);
    let binned_at = page.binned_at.clone().unwrap_or_default();
    tracing::info!(page_id = %page_id, "page moved to the bin");
    state
        .realtime
        .publish_library_event(&claims.sub, LibraryEvent::PageBinned { page });
    // Every open session on the page learns at once, so a device drawing on it
    // can say its ink is going to a binned page.
    state
        .realtime
        .publish_page(&page_id, PageServerMessage::PageBinned { binned_at });
    metrics.record_page_mutation("bin_page", "success");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/pages/{id}/restore` — brings a page back from the bin with all
/// its ink, thumbnails and (once #151 lands) search results, at its old place
/// in the library. Idempotent: restoring a live page returns it unchanged.
#[tracing::instrument(skip_all, fields(page_id = %page_id, owner_id = %claims.sub))]
pub async fn restore_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<Json<PageResponse>, ApiError> {
    let metrics = crate::observability::metrics();
    let restored = sqlx::query_as::<_, PageRow>(page_query!(
        "WITH p AS (
           UPDATE pages SET binned_at = NULL
           WHERE id = $1 AND owner_id = $2 AND binned_at IS NOT NULL
           RETURNING id, title, created_at, updated_at, paper, binned_at
         )",
        "p",
        ""
    ))
    .bind(&page_id)
    .bind(&claims.sub)
    .fetch_optional(metered(&state.db))
    .instrument(db_query_span!("UPDATE", "restore_page"))
    .await
    .map_err(|error| {
        metrics.record_page_mutation("restore_page", "error");
        server_error(error)
    })?;

    let Some(row) = restored else {
        return match owned_page(&state, &page_id, &claims.sub).await? {
            Some(page) => {
                tracing::debug!("page is not in the bin; nothing to restore");
                metrics.record_page_mutation("restore_page", "noop");
                Ok(Json(PageResponse { page }))
            }
            None => {
                metrics.record_page_mutation("restore_page", "not_found");
                Err(missing_page(&state, &page_id, &claims.sub, "page restore").await)
            }
        };
    };

    let page = PageSummary::from(row);
    tracing::info!(page_id = %page_id, "page restored from the bin");
    state.realtime.publish_library_event(
        &claims.sub,
        LibraryEvent::PageRestored { page: page.clone() },
    );
    state
        .realtime
        .publish_page(&page_id, PageServerMessage::PageRestored);
    metrics.record_page_mutation("restore_page", "success");
    Ok(Json(PageResponse { page }))
}

/// `GET /api/bin` — the caller's binned pages, most recently binned first,
/// each with its thumbnail and `binned_at`.
#[tracing::instrument(skip_all, fields(owner_id = %claims.sub))]
pub async fn list_bin(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<ListPagesResponse>, ApiError> {
    let rows = sqlx::query_as::<_, PageRow>(page_query!(
        "",
        "pages p",
        "WHERE p.owner_id = $1 AND p.binned_at IS NOT NULL
         ORDER BY p.binned_at DESC, p.id"
    ))
    .bind(&claims.sub)
    .fetch_all(metered(&state.db))
    .instrument(db_query_span!("SELECT", "list_bin"))
    .await
    .map_err(server_error)?;
    tracing::debug!("bin listed");
    Ok(Json(ListPagesResponse {
        pages: rows.into_iter().map(PageSummary::from).collect(),
    }))
}

/// Permanently deletes binned pages: `Some(id)` for one, `None` for the whole
/// bin. One transaction removes the rows — their ink, tombstones and
/// thumbnails go by cascade, and a thumbnail job still in flight finds its row
/// gone and drops itself — and records each id in `purged_pages`, so a later
/// write for it gets the definitive `page_deleted` answer. Returns the ids.
async fn purge_binned(
    state: &AppState,
    owner_id: &str,
    page_id: Option<&str>,
) -> Result<Vec<String>, sqlx::Error> {
    let mut tx = state
        .db
        .begin()
        .instrument(db_query_span!("BEGIN", "purge_pages"))
        .await?;
    let purged: Vec<String> = sqlx::query_scalar(
        "DELETE FROM pages
         WHERE owner_id = $1 AND binned_at IS NOT NULL AND ($2::text IS NULL OR id = $2)
         RETURNING id",
    )
    .bind(owner_id)
    .bind(page_id)
    .fetch_all(metered(&mut *tx))
    .instrument(db_query_span!("DELETE", "purge_pages"))
    .await?;
    sqlx::query(
        "INSERT INTO purged_pages (id, owner_id)
         SELECT id, $2 FROM unnest($1::text[]) AS purged(id)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&purged)
    .bind(owner_id)
    .execute(metered(&mut *tx))
    .instrument(db_query_span!("INSERT", "record_purged_pages"))
    .await?;
    tx.commit()
        .instrument(db_query_span!("COMMIT", "purge_pages"))
        .await?;
    Ok(purged)
}

/// Tells every client a page is gone for good: the library drops it, and a
/// session still open on it gets the definitive `page_deleted`.
fn announce_purged(state: &AppState, owner_id: &str, page_id: String) {
    state
        .realtime
        .publish_page(&page_id, crate::realtime::page_deleted_error(None));
    state
        .realtime
        .publish_library_event(owner_id, LibraryEvent::PageDeleted { page_id });
}

/// `DELETE /api/bin/{id}` — permanently deletes one page, the only destructive
/// step there is (#631), and only from the bin: a live page is a `409`.
/// Idempotent: a page already permanently deleted is a `204`.
#[tracing::instrument(skip_all, fields(page_id = %page_id, owner_id = %claims.sub))]
pub async fn purge_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let metrics = crate::observability::metrics();
    let purged = purge_binned(&state, &claims.sub, Some(&page_id))
        .await
        .map_err(|error| {
            metrics.record_page_mutation("purge_page", "error");
            server_error(error)
        })?;
    if purged.is_empty() {
        if owned_page(&state, &page_id, &claims.sub).await?.is_some() {
            tracing::warn!(page_id = %page_id, "permanent delete rejected: the page is not in the bin");
            metrics.record_page_mutation("purge_page", "not_binned");
            return Err(ApiError::NOT_IN_BIN);
        }
        let missing = missing_page(&state, &page_id, &claims.sub, "permanent delete").await;
        if missing == ApiError::PAGE_PURGED {
            metrics.record_page_mutation("purge_page", "noop");
            return Ok(StatusCode::NO_CONTENT);
        }
        metrics.record_page_mutation("purge_page", "not_found");
        return Err(missing);
    }
    tracing::info!(page_id = %page_id, "page permanently deleted");
    announce_purged(&state, &claims.sub, page_id);
    metrics.record_page_mutation("purge_page", "success");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/bin` — permanently deletes every page in the caller's bin.
#[tracing::instrument(skip_all, fields(owner_id = %claims.sub, purged))]
pub async fn empty_bin(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> Result<StatusCode, ApiError> {
    let metrics = crate::observability::metrics();
    let purged = purge_binned(&state, &claims.sub, None)
        .await
        .map_err(|error| {
            metrics.record_page_mutation("empty_bin", "error");
            server_error(error)
        })?;
    tracing::Span::current().record("purged", purged.len());
    tracing::info!(purged = purged.len(), "bin emptied");
    for page_id in purged {
        announce_purged(&state, &claims.sub, page_id);
    }
    metrics.record_page_mutation("empty_bin", "success");
    Ok(StatusCode::NO_CONTENT)
}

fn server_error(error: sqlx::Error) -> ApiError {
    tracing::error!(error = %error, "page database operation failed");
    ApiError::DB_OPERATION_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn rendered(error: ApiError) -> (StatusCode, String, String) {
        let response = error.into_response();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body should be readable");
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).expect("error body should be UTF-8"),
        )
    }

    // The Err variant moved from a built `Response` to `ApiError`, so every
    // status/body pair the handlers can return is pinned here — the shrink must
    // not have changed a single byte a client sees.
    #[tokio::test]
    async fn api_errors_render_the_same_responses_as_before() {
        for (error, expected_status, expected_body) in [
            (
                ApiError::PAGE_NOT_FOUND,
                StatusCode::FORBIDDEN,
                "page not found",
            ),
            (
                ApiError::THUMBNAIL_GONE,
                StatusCode::GONE,
                "thumbnail revision no longer available",
            ),
            (
                ApiError::DB_OPERATION_FAILED,
                StatusCode::INTERNAL_SERVER_ERROR,
                "database operation failed",
            ),
            (
                ApiError::PAGE_PURGED,
                StatusCode::GONE,
                "page permanently deleted",
            ),
            (
                ApiError::NOT_IN_BIN,
                StatusCode::CONFLICT,
                "page is not in the bin",
            ),
        ] {
            let (status, content_type, body) = rendered(error).await;
            assert_eq!(status, expected_status);
            assert_eq!(content_type, "text/plain; charset=utf-8");
            assert_eq!(body, expected_body);
        }
    }

    // A sqlx failure must stay a 500 with the generic body — the log line carries
    // the detail, the client never does.
    #[tokio::test]
    async fn server_error_maps_sqlx_failures_to_a_generic_500() {
        let (status, _, body) = rendered(server_error(sqlx::Error::RowNotFound)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "database operation failed");
    }

    // The whole point of the change: the success path no longer pays for the
    // error path. 128 bytes is clippy's `result_large_err` threshold, which a
    // bare `Response` hits exactly.
    #[test]
    fn the_error_variant_stays_small() {
        assert!(
            size_of::<ApiError>() < 128,
            "ApiError is {} bytes; result_large_err fires at 128",
            size_of::<ApiError>()
        );
    }
}
