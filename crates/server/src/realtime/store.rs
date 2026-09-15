//! Postgres persistence for the page channel: replay loading and the three
//! page mutations (stroke batch, tombstones, paper). Every query is spanned and
//! metered; `observability/tests.rs` audits that.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use protocol::{Paper, Stroke, StrokeBatch, TombstoneBatch};
use sqlx::PgPool;
use tracing::Instrument as _;

use crate::observability::{db_query_span, metered};

async fn page_belongs_to_owner(
    pool: &PgPool,
    page_id: &str,
    owner_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT 1 FROM pages WHERE id = $1 AND owner_id = $2")
        .bind(page_id)
        .bind(owner_id)
        .fetch_optional(metered(pool))
        .instrument(db_query_span("SELECT", "page_belongs_to_owner"))
        .await?;
    Ok(row.is_some())
}

/// The page's current paper. An unrecognised stored value (only reachable if the
/// `pages_paper_known` CHECK were dropped) degrades to a blank page rather than
/// failing the connection.
async fn current_paper(pool: &PgPool, page_id: &str) -> Result<Paper, sqlx::Error> {
    let stored: String = sqlx::query_scalar("SELECT paper FROM pages WHERE id = $1")
        .bind(page_id)
        .fetch_one(metered(pool))
        .instrument(db_query_span("SELECT", "current_paper"))
        .await?;
    Ok(Paper::from_wire(&stored).unwrap_or_default())
}

async fn max_seq(pool: &PgPool, page_id: &str) -> Result<u64, sqlx::Error> {
    let seq: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1")
            .bind(page_id)
            .fetch_one(metered(pool))
            .instrument(db_query_span("SELECT", "max_seq"))
            .await?;
    Ok(seq as u64)
}

#[derive(sqlx::FromRow)]
struct StrokeBatchRow {
    seq: i64,
    client_batch_id: String,
    strokes: sqlx::types::Json<Vec<Stroke>>,
}

async fn load_batches_after(
    pool: &PgPool,
    page_id: &str,
    from_seq: u64,
) -> Result<Vec<StrokeBatch>, sqlx::Error> {
    let rows = sqlx::query_as::<_, StrokeBatchRow>(
        r#"
        SELECT seq, client_batch_id, strokes
        FROM stroke_batches
        WHERE page_id = $1 AND seq > $2
        ORDER BY seq
        "#,
    )
    .bind(page_id)
    .bind(from_seq as i64)
    .fetch_all(metered(pool))
    .instrument(db_query_span("SELECT", "load_batches_after"))
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| StrokeBatch {
            seq: row.seq as u64,
            client_batch_id: row.client_batch_id,
            strokes: row.strokes.0,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct TombstoneBatchRow {
    revision: i64,
    client_mutation_id: String,
    stroke_ids: sqlx::types::Json<Vec<String>>,
}

async fn load_page_replay(
    pool: &PgPool,
    page_id: &str,
    from_seq: u64,
) -> Result<(Vec<StrokeBatch>, Vec<TombstoneBatch>), sqlx::Error> {
    let batches = load_batches_after(pool, page_id, from_seq).await?;
    let tombstones = sqlx::query_as::<_, TombstoneBatchRow>(
        r#"
        SELECT revision, client_mutation_id, stroke_ids
        FROM tombstone_batches
        WHERE page_id = $1
        ORDER BY revision, client_mutation_id
        "#,
    )
    .bind(page_id)
    .fetch_all(metered(pool))
    .instrument(db_query_span("SELECT", "load_tombstone_batches"))
    .await?
    .into_iter()
    .map(|row| TombstoneBatch {
        revision: row.revision as u64,
        client_mutation_id: row.client_mutation_id,
        stroke_ids: row.stroke_ids.0,
    })
    .collect();
    Ok((batches, tombstones))
}

/// Persist a stroke batch with a per-page monotonic sequence. Idempotent by
/// `client_batch_id` so reconnect retries return the existing seq instead of
/// double-inserting. New batches atomically bump the page `updated_at` and
/// persist their thumbnail job before the commit is acknowledged.
pub(super) struct PersistedBatch {
    pub(super) seq: u64,
    pub(super) revision: u64,
    pub(super) owner_id: String,
    /// Strokes that survive delete-wins filtering — the only ones to broadcast
    /// and store. Empty means the whole add was suppressed by tombstones and no
    /// visible state changed.
    pub(super) visible_strokes: Vec<Stroke>,
    pub(super) thumbnail_job_created: bool,
    /// The new `updated_at` (RFC 3339) when this commit actually bumped the row,
    /// so the library can re-sort. `None` for idempotent retries and fully
    /// suppressed adds, which change no last-edited timestamp.
    pub(super) updated_at: Option<String>,
}

async fn persist_batch(
    pool: &PgPool,
    page_id: &str,
    client_batch_id: &str,
    strokes: &[Stroke],
) -> Result<PersistedBatch, sqlx::Error> {
    let mut tx = pool
        .begin()
        .instrument(db_query_span("BEGIN", "persist_batch"))
        .await?;

    // Serialize seq allocation for this page against concurrent commits. `paper`
    // is read under the same lock so the thumbnail job records the paper in force
    // at the revision it will rasterize.
    let (owner_id, current_revision, paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_batch_lock_page"))
    .await?;

    // Delete-wins: a permanent tombstone for a stroke id suppresses every later
    // add of that id. Filtering before insert stops an add-after-delete (or a
    // stale offline replay) from advancing the revision or being broadcast as
    // visible ink, while identical retries stay idempotent by `client_batch_id`.
    let submitted_ids: Vec<String> = strokes.iter().map(|stroke| stroke.id.clone()).collect();
    let tombstoned: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND stroke_id = ANY($2)",
    )
    .bind(page_id)
    .bind(&submitted_ids)
    .fetch_all(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_batch_tombstoned"))
    .await?
    .into_iter()
    .collect();
    let visible_strokes: Vec<Stroke> = strokes
        .iter()
        .filter(|stroke| !tombstoned.contains(&stroke.id))
        .cloned()
        .collect();

    let existing = sqlx::query_as::<_, (i64, i64)>(
        "SELECT seq, revision FROM stroke_batches WHERE page_id = $1 AND client_batch_id = $2",
    )
    .bind(page_id)
    .bind(client_batch_id)
    .fetch_optional(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_batch_existing"))
    .await?;
    if let Some((seq, revision)) = existing {
        // Idempotent retry: re-echo only the strokes still visible today.
        tx.commit()
            .instrument(db_query_span("COMMIT", "persist_batch"))
            .await?;
        return Ok(PersistedBatch {
            seq: seq as u64,
            revision: revision as u64,
            owner_id,
            visible_strokes,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    if visible_strokes.is_empty() {
        // Every submitted stroke is tombstoned — acknowledge the add as a no-op
        // without inserting a batch, advancing the revision, or broadcasting.
        let head_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1",
        )
        .bind(page_id)
        .fetch_one(metered(&mut *tx))
        .instrument(db_query_span("SELECT", "persist_batch_head_seq"))
        .await?;
        tx.commit()
            .instrument(db_query_span("COMMIT", "persist_batch"))
            .await?;
        return Ok(PersistedBatch {
            seq: head_seq as u64,
            revision: current_revision as u64,
            owner_id,
            visible_strokes,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM stroke_batches WHERE page_id = $1",
    )
    .bind(page_id)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_batch_next_seq"))
    .await?;
    let revision = current_revision + 1;

    sqlx::query(
        "INSERT INTO stroke_batches (page_id, seq, revision, client_batch_id, strokes) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(page_id)
    .bind(next)
    .bind(revision)
    .bind(client_batch_id)
    .bind(sqlx::types::Json(&visible_strokes))
    .execute(metered(&mut *tx))
    .instrument(db_query_span("INSERT", "persist_batch_insert"))
    .await?;

    let updated_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "UPDATE pages SET updated_at = now(), ink_revision = $2 WHERE id = $1 RETURNING updated_at",
    )
    .bind(page_id)
    .bind(revision)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("UPDATE", "persist_batch_bump_page"))
    .await?
    .to_rfc3339();
    let thumbnail_job_created = sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING",
    )
    .bind(page_id)
    .bind(revision)
    .bind(&paper)
    .execute(metered(&mut *tx))
    .instrument(db_query_span("INSERT", "persist_batch_thumbnail_job"))
    .await?
    .rows_affected()
        == 1;

    tx.commit()
        .instrument(db_query_span("COMMIT", "persist_batch"))
        .await?;
    Ok(PersistedBatch {
        seq: next as u64,
        revision: revision as u64,
        owner_id,
        visible_strokes,
        thumbnail_job_created,
        updated_at: Some(updated_at),
    })
}

pub(super) struct PersistedTombstones {
    pub(super) revision: u64,
    pub(super) owner_id: String,
    pub(super) stroke_ids: Vec<String>,
    pub(super) thumbnail_job_created: bool,
    /// See `PersistedBatch::updated_at` — `None` when the erase changed no
    /// strokes (idempotent replay or all-already-tombstoned).
    pub(super) updated_at: Option<String>,
}

async fn persist_tombstones(
    pool: &PgPool,
    page_id: &str,
    client_mutation_id: &str,
    requested_ids: &[String],
) -> Result<PersistedTombstones, sqlx::Error> {
    let mut tx = pool
        .begin()
        .instrument(db_query_span("BEGIN", "persist_tombstones"))
        .await?;
    let (owner_id, current_revision, paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_tombstones_lock_page"))
    .await?;
    if let Some((revision, ids)) = sqlx::query_as::<_, (i64, sqlx::types::Json<Vec<String>>)>(
        "SELECT revision, stroke_ids FROM tombstone_batches WHERE page_id = $1 AND client_mutation_id = $2",
    )
    .bind(page_id)
    .bind(client_mutation_id)
    .fetch_optional(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_tombstones_existing"))
    .await? {
        tx.commit().instrument(db_query_span("COMMIT", "persist_tombstones")).await?;
        return Ok(PersistedTombstones { revision: revision as u64, owner_id, stroke_ids: ids.0, thumbnail_job_created: false, updated_at: None });
    }

    let mut ids = requested_ids.to_vec();
    ids.sort();
    ids.dedup();
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND stroke_id = ANY($2)",
    )
    .bind(page_id)
    .bind(&ids)
    .fetch_all(metered(&mut *tx))
    .instrument(db_query_span(
        "SELECT",
        "persist_tombstones_already_deleted",
    ))
    .await?;
    ids.retain(|id| !existing.contains(id));
    let revision = if ids.is_empty() {
        current_revision
    } else {
        current_revision + 1
    };
    for id in &ids {
        sqlx::query("INSERT INTO stroke_tombstones (page_id, stroke_id, deleted_revision) VALUES ($1, $2, $3)")
            .bind(page_id).bind(id).bind(revision).execute(metered(&mut *tx)).instrument(db_query_span("INSERT", "persist_tombstones_insert_stroke")).await?;
    }
    sqlx::query("INSERT INTO tombstone_batches (page_id, client_mutation_id, revision, stroke_ids) VALUES ($1, $2, $3, $4)")
        .bind(page_id).bind(client_mutation_id).bind(revision).bind(sqlx::types::Json(&ids)).execute(metered(&mut *tx)).instrument(db_query_span("INSERT", "persist_tombstones_insert_batch")).await?;
    let updated_at = if !ids.is_empty() {
        Some(
            sqlx::query_scalar::<_, DateTime<Utc>>(
                "UPDATE pages SET updated_at = now(), ink_revision = ink_revision + 1 WHERE id = $1 RETURNING updated_at",
            )
            .bind(page_id)
            .fetch_one(metered(&mut *tx))
            .instrument(db_query_span("UPDATE", "persist_tombstones_bump_page"))
            .await?
            .to_rfc3339(),
        )
    } else {
        None
    };
    let thumbnail_job_created = if ids.is_empty() {
        false
    } else {
        sqlx::query("INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING")
            .bind(page_id).bind(revision).bind(&paper).execute(metered(&mut *tx)).instrument(db_query_span("INSERT", "persist_tombstones_thumbnail_job")).await?.rows_affected() == 1
    };
    tx.commit()
        .instrument(db_query_span("COMMIT", "persist_tombstones"))
        .await?;
    Ok(PersistedTombstones {
        revision: revision as u64,
        owner_id,
        stroke_ids: ids,
        thumbnail_job_created,
        updated_at,
    })
}

/// Outcome of a paper change.
///
/// Note this widens `pages.ink_revision` from "ink mutations" to "anything that
/// changes how the page renders". That is safe: gap-fill and `Welcome.last_seq`
/// use `stroke_batches.seq`, a *separate* sequence, and `thumbnails::generate`
/// filters revisions with range predicates, so gaps in `revision` are harmless.
/// The column is deliberately **not** renamed — it is read in three query sites.
pub(super) struct PersistedPaper {
    /// False when the page already had this paper: nothing bumped, nothing
    /// stored, nothing to fan out.
    pub(super) changed: bool,
    /// The revision the paper is in force at — the new one when the page had
    /// visible ink, otherwise the unchanged current one.
    pub(super) revision: u64,
    pub(super) owner_id: String,
    pub(super) thumbnail_job_created: bool,
    /// See `PersistedBatch::updated_at` — `None` for a same-value no-op.
    pub(super) updated_at: Option<String>,
}

async fn persist_paper(
    pool: &PgPool,
    page_id: &str,
    paper: Paper,
) -> Result<PersistedPaper, sqlx::Error> {
    let mut tx = pool
        .begin()
        .instrument(db_query_span("BEGIN", "persist_paper"))
        .await?;
    let (owner_id, current_revision, current_paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_paper_lock_page"))
    .await?;

    if current_paper == paper.wire_value() {
        tx.commit()
            .instrument(db_query_span("COMMIT", "persist_paper"))
            .await?;
        return Ok(PersistedPaper {
            changed: false,
            revision: current_revision as u64,
            owner_id,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    // Does the page have any ink still *visible* at this revision? Erased ink
    // does not count — an all-erased page is as inkless as a never-inked one.
    let has_visible_ink: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (
             SELECT 1
             FROM stroke_batches b
             CROSS JOIN LATERAL jsonb_array_elements(b.strokes) AS s(stroke)
             WHERE b.page_id = $1
               AND b.revision <= $2
               AND NOT EXISTS (
                 SELECT 1 FROM stroke_tombstones t
                 WHERE t.page_id = b.page_id
                   AND t.stroke_id = s.stroke ->> 'id'
                   AND t.deleted_revision <= $2
               )
           )"#,
    )
    .bind(page_id)
    .bind(current_revision)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("SELECT", "persist_paper_has_visible_ink"))
    .await?;

    // With visible ink the existing thumbnail is now stale, so bump the revision
    // to mint a fresh immutable artifact (new source_seq → new URL) and let the
    // existing PageThumbnailUpdated fan-out and `source_seq >=` freshness guards
    // do the rest.
    //
    // With no visible ink there is nothing to invalidate — and bumping would
    // break an invariant `thumbnails::cleanup` depends on: it protects the head
    // artifact with `source_seq <> (SELECT ink_revision …)`. Today ink_revision
    // always names an existing thumbnail row; a bump with no job created would
    // leave it naming nothing, making *every* surviving thumbnail of that page
    // retention-eligible and silently dropping the library preview after 7 days.
    let revision = if has_visible_ink {
        current_revision + 1
    } else {
        current_revision
    };

    let updated_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "UPDATE pages SET paper = $2, updated_at = now(), ink_revision = $3 WHERE id = $1 RETURNING updated_at",
    )
    .bind(page_id)
    .bind(paper.wire_value())
    .bind(revision)
    .fetch_one(metered(&mut *tx))
    .instrument(db_query_span("UPDATE", "persist_paper_update_page"))
    .await?
    .to_rfc3339();

    let thumbnail_job_created = if has_visible_ink {
        sqlx::query(
            "INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING",
        )
        .bind(page_id)
        .bind(revision)
        .bind(paper.wire_value())
        .execute(metered(&mut *tx))
        .instrument(db_query_span("INSERT", "persist_paper_thumbnail_job"))
        .await?
        .rows_affected()
            == 1
    } else {
        false
    };

    tx.commit()
        .instrument(db_query_span("COMMIT", "persist_paper"))
        .await?;
    Ok(PersistedPaper {
        changed: true,
        revision: revision as u64,
        owner_id,
        thumbnail_job_created,
        // The library re-sorts on a paper change even for an inkless page: the
        // page really was last edited now.
        updated_at: Some(updated_at),
    })
}

/// The page channel's persistence port. `dispatch` depends on this rather than
/// on Postgres, so every client message is unit-tested against an in-memory
/// store; [`PgPageStore`] is the production implementation.
pub(super) trait PageStore: Send + Sync {
    async fn page_belongs_to_owner(
        &self,
        page_id: &str,
        owner_id: &str,
    ) -> Result<bool, sqlx::Error>;
    async fn current_paper(&self, page_id: &str) -> Result<Paper, sqlx::Error>;
    async fn max_seq(&self, page_id: &str) -> Result<u64, sqlx::Error>;
    async fn load_page_replay(
        &self,
        page_id: &str,
        from_seq: u64,
    ) -> Result<(Vec<StrokeBatch>, Vec<TombstoneBatch>), sqlx::Error>;
    async fn persist_batch(
        &self,
        page_id: &str,
        client_batch_id: &str,
        strokes: &[Stroke],
    ) -> Result<PersistedBatch, sqlx::Error>;
    async fn persist_tombstones(
        &self,
        page_id: &str,
        client_mutation_id: &str,
        stroke_ids: &[String],
    ) -> Result<PersistedTombstones, sqlx::Error>;
    async fn persist_paper(
        &self,
        page_id: &str,
        paper: Paper,
    ) -> Result<PersistedPaper, sqlx::Error>;
}

/// [`PageStore`] over the application pool. Each method delegates to the
/// spanned, metered query function above, so the db-span audit in
/// `observability/tests.rs` still sees every call site.
pub(super) struct PgPageStore {
    pool: PgPool,
}

impl PgPageStore {
    pub(super) fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl PageStore for PgPageStore {
    async fn page_belongs_to_owner(
        &self,
        page_id: &str,
        owner_id: &str,
    ) -> Result<bool, sqlx::Error> {
        page_belongs_to_owner(&self.pool, page_id, owner_id).await
    }

    async fn current_paper(&self, page_id: &str) -> Result<Paper, sqlx::Error> {
        current_paper(&self.pool, page_id).await
    }

    async fn max_seq(&self, page_id: &str) -> Result<u64, sqlx::Error> {
        max_seq(&self.pool, page_id).await
    }

    async fn load_page_replay(
        &self,
        page_id: &str,
        from_seq: u64,
    ) -> Result<(Vec<StrokeBatch>, Vec<TombstoneBatch>), sqlx::Error> {
        load_page_replay(&self.pool, page_id, from_seq).await
    }

    async fn persist_batch(
        &self,
        page_id: &str,
        client_batch_id: &str,
        strokes: &[Stroke],
    ) -> Result<PersistedBatch, sqlx::Error> {
        persist_batch(&self.pool, page_id, client_batch_id, strokes).await
    }

    async fn persist_tombstones(
        &self,
        page_id: &str,
        client_mutation_id: &str,
        stroke_ids: &[String],
    ) -> Result<PersistedTombstones, sqlx::Error> {
        persist_tombstones(&self.pool, page_id, client_mutation_id, stroke_ids).await
    }

    async fn persist_paper(
        &self,
        page_id: &str,
        paper: Paper,
    ) -> Result<PersistedPaper, sqlx::Error> {
        persist_paper(&self.pool, page_id, paper).await
    }
}

#[cfg(all(test, feature = "postgres-tests"))]
mod postgres_tests;
