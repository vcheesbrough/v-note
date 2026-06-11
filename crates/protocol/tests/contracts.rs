use std::fs;
use std::path::Path;

use protocol::{
    CreatePageRequest, HealthResponse, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse,
    PageSummary, RealtimeTicketResponse, PROTOCOL_VERSION,
};

fn fixture(path: &str) -> String {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("contracts")
        .join("fixtures")
        .join(path);
    fs::read_to_string(fixture_path).expect("fixture should be readable")
}

#[test]
fn deserializes_health_fixture() {
    let parsed: HealthResponse =
        serde_json::from_str(&fixture("health.json")).expect("health fixture should parse");
    assert_eq!(parsed.status, "ok");
}

#[test]
fn deserializes_meta_fixture() {
    let parsed: MetaResponse =
        serde_json::from_str(&fixture("meta.json")).expect("meta fixture should parse");
    assert_eq!(parsed.app_name, "v-note");
    assert_eq!(parsed.protocol_version, PROTOCOL_VERSION);
}

#[test]
fn deserializes_me_fixture() {
    let parsed: MeResponse =
        serde_json::from_str(&fixture("me.json")).expect("me fixture should parse");
    assert_eq!(parsed.sub, "v-note-test-service-account");
    assert_eq!(parsed.email.as_deref(), Some("test@example.com"));
}

#[test]
fn deserializes_page_replay_stub_fixture() {
    let parsed: serde_json::Value = serde_json::from_str(&fixture("page-replay-stub.json"))
        .expect("page replay stub should parse");
    assert_eq!(parsed["kind"], "page-replay-stub");
}

#[test]
fn deserializes_page_fixtures() {
    let page: PageSummary =
        serde_json::from_str(&fixture("page.json")).expect("page fixture should parse");
    assert_eq!(page.title, "Meeting notes");

    let list: ListPagesResponse =
        serde_json::from_str(&fixture("pages.json")).expect("pages fixture should parse");
    assert_eq!(list.pages.len(), 1);

    let create: CreatePageRequest = serde_json::from_str(&fixture("create-page.json"))
        .expect("create page fixture should parse");
    assert_eq!(create.title.as_deref(), Some("Meeting notes"));
}

#[test]
fn deserializes_realtime_fixtures() {
    let ticket: RealtimeTicketResponse = serde_json::from_str(&fixture("realtime-ticket.json"))
        .expect("realtime ticket fixture should parse");
    assert!(ticket.ticket.starts_with("ticket_"));

    let created: LibraryEvent = serde_json::from_str(&fixture("library-event-page-created.json"))
        .expect("page-created event fixture should parse");
    assert!(matches!(created, LibraryEvent::PageCreated { .. }));

    let deleted: LibraryEvent = serde_json::from_str(&fixture("library-event-page-deleted.json"))
        .expect("page-deleted event fixture should parse");
    assert!(matches!(deleted, LibraryEvent::PageDeleted { .. }));
}
