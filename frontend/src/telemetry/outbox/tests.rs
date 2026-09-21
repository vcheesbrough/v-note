use super::*;

fn signed_in() -> ExportPolicy {
    let mut policy = ExportPolicy::default();
    policy.set_authenticated(true);
    policy
}

/// How many ticks pass before `should_export` next says yes, that tick included.
fn ticks_until_export(policy: &mut ExportPolicy) -> u32 {
    (1..=1_000)
        .find(|_| policy.should_export())
        .expect("the policy should allow an export eventually")
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

#[test]
fn a_full_outbox_discards_the_oldest_and_counts_it() {
    let mut outbox = Outbox::new(3);
    for item in 1..=5 {
        outbox.push(item);
    }

    assert_eq!(outbox.len(), 3);
    assert_eq!(outbox.dropped(), 2);
    assert_eq!(outbox.take_batch(10), [3, 4, 5], "the newest survive");
}

/// The property that matters when the sidecar is down for an hour: memory does
/// not track how long it has been down.
#[test]
fn an_outbox_never_exceeds_its_capacity() {
    let mut outbox = Outbox::new(CAPACITY);
    for item in 0..(CAPACITY * 10) {
        outbox.push(item);
        assert!(outbox.len() <= CAPACITY);
    }
    assert_eq!(outbox.dropped(), (CAPACITY * 9) as u64);
}

#[test]
fn a_batch_is_the_oldest_items_in_order_and_leaves_the_rest() {
    let mut outbox = Outbox::new(10);
    for item in 1..=5 {
        outbox.push(item);
    }

    assert_eq!(outbox.take_batch(2), [1, 2]);
    assert_eq!(outbox.take_batch(2), [3, 4]);
    assert_eq!(outbox.take_batch(2), [5]);
    assert!(outbox.is_empty());
    assert_eq!(outbox.take_batch(2), Vec::<i32>::new());
}

#[test]
fn a_zero_capacity_outbox_holds_nothing_and_does_not_panic() {
    let mut outbox = Outbox::new(0);
    outbox.push(1);

    assert!(outbox.is_empty());
    assert_eq!(outbox.dropped(), 1);
}

// ---------------------------------------------------------------------------
// Status -> outcome
// ---------------------------------------------------------------------------

#[test]
fn statuses_map_to_what_the_exporter_should_do() {
    use ExportOutcome::*;
    for (status, expected) in [
        (Some(200), Accepted),
        (Some(202), Accepted),
        // The kill switch, and the only status that is final.
        (Some(404), SwitchedOff),
        // The batch's fault. Do not back off: the next one is probably fine.
        (Some(400), BatchRefused),
        (Some(413), BatchRefused),
        (Some(415), BatchRefused),
        // Not the batch's fault, and may clear up.
        (Some(401), Unavailable),
        (Some(403), Unavailable),
        (Some(429), Unavailable),
        (Some(502), Unavailable),
        (Some(503), Unavailable),
        (Some(307), Unavailable),
        (None, Unavailable),
    ] {
        assert_eq!(ExportOutcome::from_status(status), expected, "{status:?}");
    }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// The cookie is the credential, so a signed-out page can only collect 401s.
#[test]
fn nothing_is_exported_until_there_is_a_session() {
    let mut policy = ExportPolicy::default();
    assert!(!policy.should_export());

    policy.set_authenticated(true);
    assert!(policy.should_export());

    policy.set_authenticated(false);
    assert!(!policy.should_export());
}

#[test]
fn a_healthy_exporter_exports_every_tick() {
    let mut policy = signed_in();
    for _ in 0..5 {
        assert!(policy.should_export());
        policy.record(ExportOutcome::Accepted);
    }
}

/// 404 is how the server's kill switch reaches the browser, and it is final:
/// the switch is snapshotted at server start, so it cannot come back on within
/// a page's lifetime, and a reload starts a fresh policy anyway.
#[test]
fn a_404_switches_the_exporter_off_for_good() {
    let mut policy = signed_in();
    policy.record(ExportOutcome::SwitchedOff);

    assert!(policy.is_switched_off());
    for _ in 0..100 {
        assert!(!policy.should_export());
    }
    // Nothing un-switches it — not a later success, not a fresh sign-in.
    policy.record(ExportOutcome::Accepted);
    policy.set_authenticated(true);
    assert!(!policy.should_export());
}

/// A sidecar that is down gets *fewer* requests the longer it stays down, and
/// never fewer than one a minute, so recovery is noticed.
#[test]
fn failures_back_off_exponentially_to_a_ceiling() {
    let mut policy = signed_in();
    // Each round: an attempt fails, then count the ticks to the next attempt
    // (which is the one that fails in the following round).
    let gaps: Vec<u32> = (0..7)
        .map(|_| {
            policy.record(ExportOutcome::Unavailable);
            ticks_until_export(&mut policy)
        })
        .collect();

    // A wait of 1, 2, 4, 8 ticks, then pinned at the 12-tick ceiling; the
    // attempt itself lands on the tick after the wait, hence one more each.
    assert_eq!(gaps, [2, 3, 5, 9, 13, 13, 13]);
}

#[test]
fn one_success_clears_the_backoff_completely() {
    let mut policy = signed_in();
    for _ in 0..6 {
        policy.record(ExportOutcome::Unavailable);
    }
    assert!(ticks_until_export(&mut policy) > 1);

    policy.record(ExportOutcome::Accepted);
    assert!(policy.should_export(), "no residual wait after a success");

    // …and the *next* failure starts from the bottom again, not the ceiling.
    policy.record(ExportOutcome::Unavailable);
    assert_eq!(ticks_until_export(&mut policy), 2);
}

/// A refused batch says nothing about the collector's health, so it must not
/// slow the exporter down — or one malformed span would throttle a session.
#[test]
fn a_refused_batch_does_not_trigger_backoff() {
    let mut policy = signed_in();
    policy.record(ExportOutcome::BatchRefused);

    assert!(policy.should_export());
}
