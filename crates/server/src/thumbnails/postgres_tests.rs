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

/// A job whose row is gone — its page was permanently deleted (#631), and the
/// row cascaded with it — acks and drops instead of failing.
#[sqlx::test(migrations = "./migrations")]
async fn generate_drops_a_job_whose_page_was_permanently_deleted(pool: PgPool) {
    insert_page(&pool, 1).await;
    insert_batch(&pool, 1, 1, &[stroke("s1")]).await;
    insert_job(&pool, 1, Paper::None).await;
    sqlx::query("DELETE FROM pages WHERE id = $1")
        .bind(PAGE)
        .execute(&pool)
        .await
        .expect("page should delete");

    assert_eq!(
        generate(&pool, PAGE, 1).await,
        Ok(Generated::JobGone),
        "a gone page's job is dropped, not failed"
    );
}

/// A binned page keeps rendering: its ink is kept, so its preview is too.
#[sqlx::test(migrations = "./migrations")]
async fn generate_renders_a_binned_pages_job(pool: PgPool) {
    insert_page(&pool, 1).await;
    sqlx::query("UPDATE pages SET binned_at = now() WHERE id = $1")
        .bind(PAGE)
        .execute(&pool)
        .await
        .expect("page should bin");
    insert_batch(&pool, 1, 1, &[stroke("s1")]).await;
    insert_job(&pool, 1, Paper::None).await;

    assert_eq!(generate(&pool, PAGE, 1).await, Ok(Generated::Rendered));
    assert_eq!(stored(&pool, 1).await.0, "available");
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

    assert_eq!(kept(&pool).await, [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13]);
}

async fn insert_artifact(pool: &PgPool, source_seq: i64, status: &str) {
    let png: Option<&[u8]> = (status == "available").then_some(b"\x00");
    sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status, png) VALUES ($1, $2, $3, $4)",
    )
    .bind(PAGE)
    .bind(source_seq)
    .bind(status)
    .bind(png)
    .execute(pool)
    .await
    .expect("thumbnail row should insert");
}

async fn kept(pool: &PgPool) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT source_seq FROM page_thumbnails WHERE page_id = $1 ORDER BY source_seq",
    )
    .bind(PAGE)
    .fetch_all(pool)
    .await
    .expect("kept rows should load")
}

/// Retention prunes finished artifacts only (#634): a job still `generating`
/// survives however old or far down the list it is. The 10-newest window still
/// counts it, as before.
#[sqlx::test(migrations = "./migrations")]
async fn cleanup_leaves_jobs_still_generating_and_prunes_finished_artifacts_as_before(
    pool: PgPool,
) {
    insert_page(&pool, 16).await;
    // 1: in flight and past the age limit — kept.
    insert_artifact(&pool, 1, "generating").await;
    // 2: finished and past the age limit — pruned.
    insert_artifact(&pool, 2, "available").await;
    // 3: a failed artifact outside the window — pruned.
    insert_artifact(&pool, 3, "failed").await;
    // 4: in flight, outside the window — kept.
    insert_artifact(&pool, 4, "generating").await;
    // 5..=14: finished.
    for source_seq in 5..=14_i64 {
        insert_artifact(&pool, source_seq, "available").await;
    }
    // 15: in flight, and it takes a window slot: the window is 6..=15, so 5
    // is pruned.
    insert_artifact(&pool, 15, "generating").await;
    // 16: the head.
    insert_artifact(&pool, 16, "available").await;
    sqlx::query(
        "UPDATE page_thumbnails SET created_at = now() - interval '8 days' WHERE page_id = $1 AND source_seq IN (1, 2)",
    )
    .bind(PAGE)
    .execute(&pool)
    .await
    .expect("rows should age");

    cleanup(&pool, PAGE).await.expect("cleanup");

    assert_eq!(
        kept(&pool).await,
        [1, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
}

/// The race the card describes (#634): jobs in flight while a newer one renders
/// first and runs cleanup. Their rows survive that cleanup. When each reaches a
/// permit, a job still inside the window renders, and one a burst has pushed
/// out of it drops itself as `Superseded` — never `JobGone`, which is left to
/// permanently deleted pages.
///
/// Job 2 is the job-side check's lower edge: exactly 9 newer non-head
/// revisions (3..=11) plus the head. It must render, as `cleanup` keeps it — a
/// check that counted the head, or stopped at 9, would drop it.
#[sqlx::test(migrations = "./migrations")]
async fn jobs_in_flight_survive_a_newer_jobs_cleanup_and_drop_themselves_only_once_superseded(
    pool: PgPool,
) {
    insert_page(&pool, 12).await;
    insert_batch(&pool, 1, 1, &[stroke("s1")]).await;
    insert_batch(&pool, 2, 2, &[stroke("s2")]).await;
    insert_batch(&pool, 3, 11, &[stroke("s11")]).await;
    insert_batch(&pool, 4, 12, &[stroke("s12")]).await;
    // 1: outside the window of the 10 newest non-head revisions (2..=11).
    insert_job(&pool, 1, Paper::None).await;
    // 2: the window's oldest slot.
    insert_job(&pool, 2, Paper::None).await;
    for source_seq in 3..=10_i64 {
        insert_artifact(&pool, source_seq, "available").await;
    }
    // 11: inside it.
    insert_job(&pool, 11, Paper::None).await;
    insert_job(&pool, 12, Paper::None).await;

    assert_eq!(generate(&pool, PAGE, 12).await, Ok(Generated::Rendered));
    assert_eq!(
        stored(&pool, 1).await.0,
        "generating",
        "the newer job's cleanup leaves an older job's row alone"
    );
    assert_eq!(stored(&pool, 11).await.0, "generating");

    assert_eq!(
        generate(&pool, PAGE, 11).await,
        Ok(Generated::Rendered),
        "a job inside the window renders"
    );
    assert_eq!(
        generate(&pool, PAGE, 2).await,
        Ok(Generated::Rendered),
        "9 newer non-head revisions plus the head still leave a job in the window"
    );
    assert_eq!(
        generate(&pool, PAGE, 1).await,
        Ok(Generated::Superseded),
        "a job pushed out of the window drops itself instead of rendering"
    );
    assert_eq!(
        kept(&pool).await,
        [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        "the superseded job removed its own row"
    );
}

/// The window's age limit applies to the job's own check as it does to
/// `cleanup`: a recovered job older than 7 days is superseded unless it is the
/// head, which always renders.
#[sqlx::test(migrations = "./migrations")]
async fn a_job_past_the_age_limit_is_superseded_unless_it_is_the_head(pool: PgPool) {
    insert_page(&pool, 2).await;
    insert_batch(&pool, 1, 1, &[stroke("s1")]).await;
    insert_batch(&pool, 2, 2, &[stroke("s2")]).await;
    insert_job(&pool, 1, Paper::None).await;
    insert_job(&pool, 2, Paper::None).await;
    sqlx::query(
        "UPDATE page_thumbnails SET created_at = now() - interval '8 days' WHERE page_id = $1",
    )
    .bind(PAGE)
    .execute(&pool)
    .await
    .expect("rows should age");

    assert_eq!(generate(&pool, PAGE, 1).await, Ok(Generated::Superseded));
    assert_eq!(generate(&pool, PAGE, 2).await, Ok(Generated::Rendered));
    assert_eq!(kept(&pool).await, [2]);
}

/// `enqueue`'s arms for a job that ends without rendering (#634): each counts
/// its own `generate_thumbnail` result and hands back the queue-depth slot
/// `enqueue` took, so a commit burst of superseded jobs cannot leak gauge depth.
#[sqlx::test(migrations = "./migrations")]
async fn enqueue_counts_a_dropped_job_and_releases_its_queue_slot(pool: PgPool) {
    let _metrics = crate::observability::TEST_METRICS_LOCK.lock().await;
    let metrics = crate::observability::metrics();
    let state = AppState {
        db: pool.clone(),
        ..AppState::for_tests()
    };
    // Revision 1 is superseded: 10 newer non-head revisions, plus the head.
    insert_page(&pool, 12).await;
    insert_job(&pool, 1, Paper::None).await;
    for source_seq in 2..=12_i64 {
        insert_artifact(&pool, source_seq, "available").await;
    }

    for (result, source_seq) in [("superseded", 1), ("job_gone", 99)] {
        let count = metrics.page_mutation_count("generate_thumbnail", result);
        let depth = metrics.thumbnail_queue_depth();

        enqueue(
            state.clone(),
            PAGE.to_string(),
            "owner".to_string(),
            source_seq,
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while metrics.page_mutation_count("generate_thumbnail", result) == count {
            assert!(Instant::now() < deadline, "the {result} job should finish");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            metrics.page_mutation_count("generate_thumbnail", result),
            count + 1,
            "{result} counted once"
        );
        assert_eq!(
            metrics.thumbnail_queue_depth(),
            depth,
            "the {result} job released its queue slot"
        );
    }
    assert_eq!(kept(&pool).await, (2..=12).collect::<Vec<i64>>());
}
