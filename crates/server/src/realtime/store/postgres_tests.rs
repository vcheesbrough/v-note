//! The page channel's SQL against a real Postgres (#337).
//!
//! `dispatch` is unit-tested against an in-memory store; these pin what the
//! Postgres implementation actually does — sequence and revision allocation,
//! idempotent retries, delete-wins, and the paper revision rules — which until
//! now only the Playwright suite exercised. Compiled only with the
//! `postgres-tests` feature; `scripts/rust-ci-test.sh` provides `DATABASE_URL`,
//! and `#[sqlx::test]` gives each test a fresh migrated database.

use super::*;

const PAGE: &str = "page_store_test";
const OWNER: &str = "owner_store_test";

fn stroke(id: &str) -> Stroke {
    let mut stroke: Stroke = serde_json::from_str(include_str!(
        "../../../../../contracts/fixtures/stroke.json"
    ))
    .expect("stroke fixture should parse");
    stroke.id = id.to_string();
    stroke
}

fn ids(strokes: &[Stroke]) -> Vec<&str> {
    strokes.iter().map(|stroke| stroke.id.as_str()).collect()
}

fn owned(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

async fn insert_page(pool: &PgPool, paper: Paper) {
    sqlx::query("INSERT INTO pages (id, owner_id, title, paper) VALUES ($1, $2, '', $3)")
        .bind(PAGE)
        .bind(OWNER)
        .bind(paper.wire_value())
        .execute(pool)
        .await
        .expect("page row should insert");
}

async fn ink_revision(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT ink_revision FROM pages WHERE id = $1")
        .bind(PAGE)
        .fetch_one(pool)
        .await
        .expect("page should exist")
}

/// `(source_seq, status, paper)` for every thumbnail job on the page.
async fn thumbnail_jobs(pool: &PgPool) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT source_seq, status, paper FROM page_thumbnails WHERE page_id = $1 ORDER BY source_seq",
    )
    .bind(PAGE)
    .fetch_all(pool)
    .await
    .expect("thumbnail jobs should load")
}

fn job(source_seq: i64, paper: &str) -> (i64, String, String) {
    (source_seq, "generating".to_string(), paper.to_string())
}

#[sqlx::test(migrations = "./migrations")]
async fn ownership_paper_and_head_seq_are_read_from_the_page(pool: PgPool) {
    insert_page(&pool, Paper::RuledNarrow).await;

    assert!(
        page_belongs_to_owner(&pool, PAGE, OWNER)
            .await
            .expect("owner check")
    );
    assert!(
        !page_belongs_to_owner(&pool, PAGE, "someone_else")
            .await
            .expect("owner check")
    );
    assert!(
        !page_belongs_to_owner(&pool, "page_missing", OWNER)
            .await
            .expect("owner check")
    );
    assert_eq!(
        current_paper(&pool, PAGE).await.expect("paper"),
        Paper::RuledNarrow
    );
    assert_eq!(
        max_seq(&pool, PAGE).await.expect("max_seq"),
        0,
        "an inkless page has head seq 0"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn persist_batch_allocates_seq_and_revision_and_retries_are_idempotent(pool: PgPool) {
    insert_page(&pool, Paper::None).await;

    let first = persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("first batch");
    assert_eq!((first.seq, first.revision), (1, 1));
    assert_eq!(first.owner_id, OWNER);
    assert_eq!(ids(&first.visible_strokes), ["s1"]);
    assert!(first.thumbnail_job_created);
    assert!(first.updated_at.is_some());

    let second = persist_batch(&pool, PAGE, "batch_2", &[stroke("s2")])
        .await
        .expect("second batch");
    assert_eq!((second.seq, second.revision), (2, 2));

    let retry = persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("retry");
    assert_eq!(
        (retry.seq, retry.revision),
        (1, 1),
        "a retry returns the original allocation"
    );
    assert!(!retry.thumbnail_job_created);
    assert_eq!(retry.updated_at, None, "a retry is not an edit");

    assert_eq!(max_seq(&pool, PAGE).await.expect("max_seq"), 2);
    assert_eq!(ink_revision(&pool).await, 2);
    assert_eq!(
        thumbnail_jobs(&pool).await,
        [job(1, "none"), job(2, "none")]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_tombstoned_stroke_is_never_added_back(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("batch");
    let erased = persist_tombstones(&pool, PAGE, "erase_1", &owned(&["s1"]))
        .await
        .expect("erase");
    assert_eq!(erased.revision, 2);

    let re_add = persist_batch(&pool, PAGE, "batch_2", &[stroke("s1")])
        .await
        .expect("re-add");

    assert!(re_add.visible_strokes.is_empty());
    assert_eq!(
        (re_add.seq, re_add.revision),
        (1, 2),
        "a fully suppressed add allocates nothing"
    );
    assert!(!re_add.thumbnail_job_created);
    assert_eq!(re_add.updated_at, None);
    assert_eq!(max_seq(&pool, PAGE).await.expect("max_seq"), 1);
    assert_eq!(ink_revision(&pool).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn persist_tombstones_dedupes_skips_already_erased_and_is_idempotent(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    persist_batch(&pool, PAGE, "batch_1", &[stroke("s1"), stroke("s2")])
        .await
        .expect("batch");

    let first = persist_tombstones(&pool, PAGE, "erase_1", &owned(&["s2", "s1", "s1"]))
        .await
        .expect("erase");
    assert_eq!(first.stroke_ids, ["s1", "s2"]);
    assert_eq!(first.revision, 2);
    assert!(first.thumbnail_job_created);
    assert!(first.updated_at.is_some());

    let nothing_new = persist_tombstones(&pool, PAGE, "erase_2", &owned(&["s1"]))
        .await
        .expect("second erase");
    assert!(nothing_new.stroke_ids.is_empty());
    assert_eq!(
        nothing_new.revision, 2,
        "erasing nothing new does not bump the revision"
    );
    assert!(!nothing_new.thumbnail_job_created);
    assert_eq!(nothing_new.updated_at, None);

    let retry = persist_tombstones(&pool, PAGE, "erase_1", &owned(&["s2", "s1", "s1"]))
        .await
        .expect("retry");
    assert_eq!(retry.stroke_ids, ["s1", "s2"]);
    assert_eq!(retry.revision, 2);
    assert!(!retry.thumbnail_job_created);
    assert_eq!(ink_revision(&pool).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn load_page_replay_returns_batches_after_the_cursor_and_every_tombstone(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("batch 1");
    persist_batch(&pool, PAGE, "batch_2", &[stroke("s2")])
        .await
        .expect("batch 2");
    persist_tombstones(&pool, PAGE, "erase_1", &owned(&["s1"]))
        .await
        .expect("erase");

    let (batches, tombstones) = load_page_replay(&pool, PAGE, 1).await.expect("replay");
    let batches: Vec<(u64, &str, Vec<&str>)> = batches
        .iter()
        .map(|batch| {
            (
                batch.seq,
                batch.client_batch_id.as_str(),
                ids(&batch.strokes),
            )
        })
        .collect();
    assert_eq!(batches, [(2, "batch_2", vec!["s2"])]);
    assert_eq!(
        tombstones,
        [TombstoneBatch {
            revision: 3,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: owned(&["s1"]),
        }]
    );

    let (all, _) = load_page_replay(&pool, PAGE, 0).await.expect("full replay");
    assert_eq!(
        all.iter().map(|batch| batch.seq).collect::<Vec<_>>(),
        [1, 2],
        "stored batches keep erased ink; delete-wins is applied when sending"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn setting_the_paper_already_in_force_changes_nothing(pool: PgPool) {
    insert_page(&pool, Paper::RuledWide).await;

    let persisted = persist_paper(&pool, PAGE, Paper::RuledWide)
        .await
        .expect("paper");

    assert!(!persisted.changed);
    assert_eq!(persisted.revision, 0);
    assert!(!persisted.thumbnail_job_created);
    assert_eq!(persisted.updated_at, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn changing_paper_on_an_inkless_page_keeps_the_revision(pool: PgPool) {
    insert_page(&pool, Paper::None).await;

    let persisted = persist_paper(&pool, PAGE, Paper::SquaredSmall)
        .await
        .expect("paper");

    assert!(persisted.changed);
    assert_eq!(persisted.revision, 0);
    assert!(
        !persisted.thumbnail_job_created,
        "no ink, nothing to preview"
    );
    assert!(persisted.updated_at.is_some(), "the library still re-sorts");
    assert_eq!(
        current_paper(&pool, PAGE).await.expect("paper"),
        Paper::SquaredSmall
    );
    assert!(thumbnail_jobs(&pool).await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn changing_paper_on_an_inked_page_mints_a_thumbnail_revision_with_that_paper(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("batch");

    let persisted = persist_paper(&pool, PAGE, Paper::RuledMarginNarrow)
        .await
        .expect("paper");

    assert!(persisted.changed);
    assert_eq!(persisted.revision, 2);
    assert!(persisted.thumbnail_job_created);
    assert_eq!(ink_revision(&pool).await, 2);
    assert_eq!(
        thumbnail_jobs(&pool).await.last(),
        Some(&job(2, "ruled-margin-narrow"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_page_whose_ink_is_all_erased_counts_as_inkless_for_paper(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    persist_batch(&pool, PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("batch");
    persist_tombstones(&pool, PAGE, "erase_1", &owned(&["s1"]))
        .await
        .expect("erase");

    let persisted = persist_paper(&pool, PAGE, Paper::RuledWide)
        .await
        .expect("paper");

    assert!(persisted.changed);
    assert_eq!(persisted.revision, 2, "no visible ink, so no revision bump");
    assert!(!persisted.thumbnail_job_created);
}

#[sqlx::test(migrations = "./migrations")]
async fn pg_page_store_goes_through_the_same_queries(pool: PgPool) {
    insert_page(&pool, Paper::None).await;
    let store = PgPageStore::new(pool);

    let persisted = store
        .persist_batch(PAGE, "batch_1", &[stroke("s1")])
        .await
        .expect("batch through the store");

    assert_eq!(persisted.seq, 1);
    assert_eq!(store.max_seq(PAGE).await.expect("max_seq"), 1);
    assert!(
        store
            .page_belongs_to_owner(PAGE, OWNER)
            .await
            .expect("owner check")
    );
}
