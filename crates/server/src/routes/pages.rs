use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use protocol::{
    CreatePageRequest, LibraryEvent, ListPagesResponse, PageResponse, PageSummary, Paper,
    ThumbnailMetadata,
};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Claims;
use crate::observability::db_query_span;

#[derive(sqlx::FromRow)]
struct PageRow {
    id: String,
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    thumbnail_status: Option<String>,
    thumbnail_seq: Option<i64>,
    paper: String,
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
        }
    }
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
    const DB_UNAVAILABLE: Self = Self {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "database not configured",
    };
    const PAGE_NOT_FOUND: Self = Self {
        status: StatusCode::FORBIDDEN,
        message: "page not found",
    };
    const THUMBNAIL_GONE: Self = Self {
        status: StatusCode::GONE,
        message: "thumbnail revision no longer available",
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

fn db(state: &AppState) -> Result<&sqlx::PgPool, ApiError> {
    state.db.as_ref().ok_or(ApiError::DB_UNAVAILABLE)
}

#[tracing::instrument(skip_all)]
pub async fn list_pages(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<ListPagesResponse>, ApiError> {
    let rows = sqlx::query_as::<_, PageRow>(
        r#"
        SELECT p.id, p.title, p.created_at, p.updated_at, p.paper,
               t.status AS thumbnail_status, t.source_seq AS thumbnail_seq
        FROM pages p
        LEFT JOIN LATERAL (
          SELECT status, source_seq FROM page_thumbnails
          WHERE page_id = p.id ORDER BY source_seq DESC LIMIT 1
        ) t ON true
        WHERE p.owner_id = $1
        ORDER BY p.updated_at DESC, p.created_at DESC
        "#,
    )
    .bind(claims.sub.clone())
    .fetch_all(db(&state)?)
    .instrument(db_query_span("SELECT", "list_pages"))
    .await
    .map_err(server_error)?;

    Ok(Json(ListPagesResponse {
        pages: rows.into_iter().map(PageSummary::from).collect(),
    }))
}

#[tracing::instrument(skip_all)]
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

    // A page is born with its paper — one round trip, no revision bump, and no
    // thumbnail job (a page with no ink has nothing to preview yet). Serde has
    // already rejected any value outside the wire vocabulary.
    let row = sqlx::query_as::<_, PageRow>(
        r#"
        INSERT INTO pages (id, owner_id, title, paper)
        VALUES ($1, $2, $3, $4)
        RETURNING id, title, created_at, updated_at, paper, NULL::text AS thumbnail_status, NULL::bigint AS thumbnail_seq
        "#,
    )
    .bind(page_id)
    .bind(claims.sub.clone())
    .bind(title)
    .bind(payload.paper.wire_value())
    .fetch_one(db(&state)?)
    .instrument(db_query_span("INSERT", "create_page"))
    .await
    .map_err(|error| {
        crate::observability::metrics().record_page_mutation("create_page", "error");
        server_error(error)
    })?;

    let page = PageSummary::from(row);
    state.realtime.publish_library_event(
        &claims.sub,
        LibraryEvent::PageCreated { page: page.clone() },
    );
    crate::observability::metrics().record_page_mutation("create_page", "success");
    Ok((StatusCode::CREATED, Json(PageResponse { page })))
}

#[tracing::instrument(skip_all, fields(page_id = %page_id))]
pub async fn get_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<Json<PageResponse>, ApiError> {
    let row = sqlx::query_as::<_, PageRow>(
        r#"
        SELECT p.id, p.title, p.created_at, p.updated_at, p.paper,
               t.status AS thumbnail_status, t.source_seq AS thumbnail_seq
        FROM pages p
        LEFT JOIN LATERAL (
          SELECT status, source_seq FROM page_thumbnails
          WHERE page_id = p.id ORDER BY source_seq DESC LIMIT 1
        ) t ON true
        WHERE p.id = $1 AND p.owner_id = $2
        "#,
    )
    .bind(page_id)
    .bind(claims.sub)
    .fetch_optional(db(&state)?)
    .instrument(db_query_span("SELECT", "get_page"))
    .await
    .map_err(server_error)?;

    match row {
        Some(row) => Ok(Json(PageResponse {
            page: PageSummary::from(row),
        })),
        None => Err(ApiError::PAGE_NOT_FOUND),
    }
}

#[tracing::instrument(skip_all, fields(page_id = %page_id, source_seq = source_seq))]
pub async fn get_thumbnail(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path((page_id, source_seq)): Path<(String, u64)>,
) -> Result<Response, ApiError> {
    let owned: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pages WHERE id = $1 AND owner_id = $2)")
            .bind(&page_id)
            .bind(&claims.sub)
            .fetch_one(db(&state)?)
            .instrument(db_query_span("SELECT", "get_thumbnail_owner"))
            .await
            .map_err(server_error)?;
    if !owned {
        return Err(ApiError::PAGE_NOT_FOUND);
    }
    let png = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT png FROM page_thumbnails WHERE page_id = $1 AND source_seq = $2 AND status = 'available'",
    )
    .bind(&page_id)
    .bind(source_seq as i64)
    .fetch_optional(db(&state)?)
    .instrument(db_query_span("SELECT", "get_thumbnail_png"))
    .await
    .map_err(server_error)?;
    match png {
        Some(bytes) => Ok((
            [
                (header::CONTENT_TYPE, "image/png"),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response()),
        None => Err(ApiError::THUMBNAIL_GONE),
    }
}

#[tracing::instrument(skip_all, fields(page_id = %page_id))]
pub async fn delete_page(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(page_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query(
        r#"
        DELETE FROM pages
        WHERE id = $1 AND owner_id = $2
        "#,
    )
    .bind(page_id.clone())
    .bind(claims.sub.clone())
    .execute(db(&state)?)
    .instrument(db_query_span("DELETE", "delete_page"))
    .await
    .map_err(|error| {
        crate::observability::metrics().record_page_mutation("delete_page", "error");
        server_error(error)
    })?;

    if result.rows_affected() == 0 {
        crate::observability::metrics().record_page_mutation("delete_page", "not_found");
        return Err(ApiError::PAGE_NOT_FOUND);
    }

    state
        .realtime
        .publish_library_event(&claims.sub, LibraryEvent::PageDeleted { page_id });
    crate::observability::metrics().record_page_mutation("delete_page", "success");
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
                ApiError::DB_UNAVAILABLE,
                StatusCode::SERVICE_UNAVAILABLE,
                "database not configured",
            ),
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
