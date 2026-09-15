//! Thumbnail job SQL against a real Postgres (#337): what `generate` reads and
//! stores, and which artifacts `cleanup` keeps. Compiled only with the
//! `postgres-tests` feature; see `realtime/store/postgres_tests.rs`.

use super::*;

const PAGE: &str = "page_thumbnail_test";

fn stroke(id: &str) -> Stroke {
    let mut stroke: Stroke =
        serde_json::from_str(include_str!("../../../../contracts/fixtures/stroke.json"))
            .expect("stroke fixture should parse");
    stroke.id = id.to_string();
    stroke
}

async fn insert_page(pool: &PgPool, ink_revision: i64) {
    sqlx::query(
        "INSERT INTO pages (id, owner_id, title, ink_revision) VALUES ($1, 'owner', '', $2)",
    )
    .bind(PAGE)
    .bind(ink_revision)
    .execute(pool)
    .await
    .expect("page row should insert");
}

async fn insert_batch(pool: &PgPool, seq: i64, revision: i64, strokes: &[Stroke]) {
    sqlx::query(
        "INSERT INTO stroke_batches (page_id, seq, revision, client_batch_id, strokes) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(PAGE)
    .bind(seq)
    .bind(revision)
    .bind(format!("batch_{seq}"))
    .bind(sqlx::types::Json(strokes))
    .execute(pool)
    .await
    .expect("batch row should insert");
}

async fn insert_job(pool: &PgPool, source_seq: i64, paper: Paper) {
    sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3)",
    )
    .bind(PAGE)
    .bind(source_seq)
    .bind(paper.wire_value())
    .execute(pool)
    .await
    .expect("job row should insert");
}

async fn stored(pool: &PgPool, source_seq: i64) -> (String, Option<Vec<u8>>) {
    sqlx::query_as("SELECT status, png FROM page_thumbnails WHERE page_id = $1 AND source_seq = $2")
        .bind(PAGE)
        .bind(source_seq)
        .fetch_one(pool)
        .await
        .expect("thumbnail row should exist")
}

#[sqlx::test(migrations = "./migrations")]
async fn generate_renders_the_revisions_ink_on_the_jobs_paper_and_stores_it(pool: PgPool) {
    insert_page(&pool, 1).await;
    insert_batch(&pool, 1, 1, &[stroke("s1")]).await;
    insert_job(&pool, 1, Paper::RuledWide).await;

    generate(&pool, PAGE, 1)
        .await
        .expect("generation should succeed");

    let (status, png) = stored(&pool, 1).await;
    assert_eq!(status, "available");
    assert_eq!(
        png,
        Some(render(Paper::RuledWide, &[stroke("s1")]).expect("render"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn generate_leaves_out_ink_erased_by_its_revision_and_ink_added_after_it(pool: PgPool) {
    insert_page(&pool, 3).await;
    insert_batch(&pool, 1, 1, &[stroke("erased")]).await;
    insert_batch(&pool, 2, 3, &[stroke("later")]).await;
    sqlx::query(
        "INSERT INTO stroke_tombstones (page_id, stroke_id, deleted_revision) VALUES ($1, 'erased', 2)",
    )
    .bind(PAGE)
    .execute(&pool)
    .await
    .expect("tombstone row should insert");
    insert_job(&pool, 2, Paper::None).await;

    generate(&pool, PAGE, 2)
        .await
        .expect("generation should succeed");

    let (_, png) = stored(&pool, 2).await;
    assert_eq!(
        png,
        Some(render(Paper::None, &[]).expect("render")),
        "revision 2 has no visible ink"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn generate_fails_when_the_job_row_is_missing(pool: PgPool) {
    insert_page(&pool, 1).await;

    let error = generate(&pool, PAGE, 1)
        .await
        .expect_err("there is no job row");

    assert!(error.contains("missing"), "{error}");
}

#[sqlx::test(migrations = "./migrations")]
async fn cleanup_keeps_the_head_revision_and_the_ten_newest_recent_artifacts(pool: PgPool) {
    insert_page(&pool, 13).await;
    for source_seq in 1..=13_i64 {
        sqlx::query(
            "INSERT INTO page_thumbnails (page_id, source_seq, status, png) VALUES ($1, $2, 'available', '\\x00')",
        )
        .bind(PAGE)
        .bind(source_seq)
        .execute(&pool)
        .await
        .expect("artifact row should insert");
    }
    // 12 is past the 7-day window; the head (13) is kept even though it is too.
    sqlx::query(
        "UPDATE page_thumbnails SET created_at = now() - interval '8 days' WHERE page_id = $1 AND source_seq IN (12, 13)",
    )
    .bind(PAGE)
    .execute(&pool)
    .await
    .expect("rows should age");

    cleanup(&pool, PAGE).await.expect("cleanup");

    let kept: Vec<i64> = sqlx::query_scalar(
        "SELECT source_seq FROM page_thumbnails WHERE page_id = $1 ORDER BY source_seq",
    )
    .bind(PAGE)
    .fetch_all(&pool)
    .await
    .expect("kept rows should load");
    assert_eq!(kept, [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13]);
}
