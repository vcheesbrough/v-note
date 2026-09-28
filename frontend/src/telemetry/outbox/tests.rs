use super::*;

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

/// The property that matters when the ingest is down for an hour: memory does
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

/// The pre-config buffer is small; configuration raises the cap without losing
/// what waited, and lowering it drops the oldest.
#[test]
fn capacity_can_be_raised_and_lowered() {
    let mut outbox = Outbox::new(2);
    outbox.push(1);
    outbox.push(2);
    outbox.set_capacity(4);
    outbox.push(3);
    assert_eq!(outbox.len(), 3);

    outbox.set_capacity(1);
    assert_eq!(outbox.take_batch(10), [3]);
    assert_eq!(outbox.dropped(), 2);
}

// ---------------------------------------------------------------------------
// Configuration: no configuration, no telemetry
// ---------------------------------------------------------------------------

fn credentials(token: &str, expires_at: f64) -> Credentials {
    Credentials {
        endpoint: "https://v-notes-dev.desync.link".to_string(),
        access_token: token.to_string(),
        expires_at,
    }
}

#[test]
fn nothing_is_configured_until_the_server_says_so() {
    let lifecycle = Lifecycle::new(0.0);
    assert_eq!(lifecycle.credentials(), None);
    assert!(!lifecycle.is_off());
}

/// `204`, `404`, any other status and no response all arrive as `Absent`, and
/// every one of them means OTLP is never initialised.
#[test]
fn absent_configuration_turns_telemetry_off_for_the_page() {
    let mut lifecycle = Lifecycle::new(0.0);
    lifecycle.on_config(ConfigFetch::Absent);
    assert_eq!(lifecycle, Lifecycle::Off(OffReason::NotConfigured));

    // "Off" is final: a later configuration does not revive it.
    lifecycle.on_config(ConfigFetch::Configured(credentials("t", 1e12)));
    assert_eq!(lifecycle.credentials(), None);
}

#[test]
fn configuration_turns_telemetry_on() {
    let mut lifecycle = Lifecycle::new(0.0);
    lifecycle.on_config(ConfigFetch::Configured(credentials("t", 1e12)));
    assert_eq!(
        lifecycle.credentials().map(|c| c.access_token.as_str()),
        Some("t")
    );
}

/// The pre-config buffer is short-lived: never held for the session in hope.
#[test]
fn waiting_for_configuration_times_out() {
    let mut lifecycle = Lifecycle::new(1_000.0);
    assert!(!lifecycle.expire(1_000.0 + PRE_CONFIG_MAX_MS - 1.0));
    assert!(lifecycle.expire(1_000.0 + PRE_CONFIG_MAX_MS));
    assert_eq!(lifecycle, Lifecycle::Off(OffReason::ConfigTimedOut));
    assert!(!lifecycle.expire(1e12), "only the first call reports it");
}

/// A configured page never times out.
#[test]
fn configured_telemetry_does_not_expire() {
    let mut lifecycle = Lifecycle::new(0.0);
    lifecycle.on_config(ConfigFetch::Configured(credentials("t", 1e12)));
    assert!(!lifecycle.expire(1e12));
    assert!(lifecycle.credentials().is_some());
}

#[test]
fn urls_are_the_bare_endpoint_plus_the_otlp_path() {
    let credentials = credentials("t", 1e12);
    assert_eq!(
        credentials.url(Signal::Traces),
        "https://v-notes-dev.desync.link/v1/traces"
    );
    assert_eq!(
        credentials.url(Signal::Logs),
        "https://v-notes-dev.desync.link/v1/logs"
    );
    let trailing = Credentials {
        endpoint: "https://localhost:4318/".to_string(),
        ..credentials
    };
    assert_eq!(trailing.url(Signal::Logs), "https://localhost:4318/v1/logs");
}

/// The token is read per request: whatever the lifecycle holds *now* is what is
/// sent, so a refresh takes effect on the next export without re-initialising.
#[test]
fn the_token_is_read_per_request_and_follows_a_refresh() {
    let mut lifecycle = Lifecycle::new(0.0);
    lifecycle.on_config(ConfigFetch::Configured(credentials("first", 10_000.0)));
    let token = |lifecycle: &Lifecycle| {
        lifecycle
            .credentials()
            .and_then(|c| c.usable_token(1_000.0))
            .map(str::to_string)
    };
    assert_eq!(token(&lifecycle).as_deref(), Some("first"));

    lifecycle.on_config(ConfigFetch::Configured(credentials("second", 10_000.0)));
    assert_eq!(token(&lifecycle).as_deref(), Some("second"));
}

/// A token about to lapse is not spent on a certain `401`.
#[test]
fn a_token_near_expiry_is_not_usable() {
    let credentials = credentials("t", 1_000.0);
    assert_eq!(
        credentials.usable_token(1_000.0 - EXPIRY_MARGIN_S - 1.0),
        Some("t")
    );
    assert_eq!(credentials.usable_token(1_000.0 - EXPIRY_MARGIN_S), None);
    assert_eq!(credentials.usable_token(2_000.0), None);
}

// ---------------------------------------------------------------------------
// Status -> outcome
// ---------------------------------------------------------------------------

/// OTLP names the retryable answers, and they are the only ones.
#[test]
fn statuses_are_classified_per_the_otlp_contract() {
    use ExportOutcome::*;
    let retryable = |status| Retryable {
        status: Some(status),
        retry_after_ms: None,
    };
    for (status, expected) in [
        (Some(200), Accepted),
        (Some(202), Accepted),
        (Some(401), Unauthorized),
        (Some(400), Rejected { status: 400 }),
        (Some(403), Rejected { status: 403 }),
        (Some(404), Rejected { status: 404 }),
        (Some(413), Rejected { status: 413 }),
        (Some(415), Rejected { status: 415 }),
        (Some(500), Rejected { status: 500 }),
        (Some(501), Rejected { status: 501 }),
        (Some(307), Rejected { status: 307 }),
        (Some(429), retryable(429)),
        (Some(502), retryable(502)),
        (Some(503), retryable(503)),
        (Some(504), retryable(504)),
        (
            None,
            Retryable {
                status: None,
                retry_after_ms: None,
            },
        ),
    ] {
        assert_eq!(
            ExportOutcome::from_response(status, None),
            expected,
            "{status:?}"
        );
    }
}

#[test]
fn retry_after_is_read_as_seconds_on_retryable_answers_only() {
    assert_eq!(
        ExportOutcome::from_response(Some(429), Some("7")),
        ExportOutcome::Retryable {
            status: Some(429),
            retry_after_ms: Some(7_000.0)
        }
    );
    assert_eq!(
        ExportOutcome::from_response(Some(503), Some(" 2 ")),
        ExportOutcome::Retryable {
            status: Some(503),
            retry_after_ms: Some(2_000.0)
        }
    );
    // The HTTP-date form, or garbage, is ignored rather than guessed at.
    assert_eq!(
        ExportOutcome::from_response(Some(503), Some("Wed, 21 Oct 2015 07:28:00 GMT")),
        ExportOutcome::Retryable {
            status: Some(503),
            retry_after_ms: None
        }
    );
    assert_eq!(
        ExportOutcome::from_response(Some(400), Some("5")),
        ExportOutcome::Rejected { status: 400 }
    );
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

const MID: f64 = 0.5;

#[test]
fn a_healthy_exporter_exports_whenever_asked() {
    let mut policy = ExportPolicy::default();
    for tick in 0..5 {
        let now = f64::from(tick) * 5_000.0;
        assert!(policy.should_export(now));
        let decision = policy.record(ExportOutcome::Accepted, 1, now, MID);
        assert_eq!(decision.transition, None, "no line while healthy");
    }
}

/// A rejected batch is dropped with no backoff: it was the batch's fault.
#[test]
fn a_rejected_batch_is_dropped_without_backoff() {
    let mut policy = ExportPolicy::default();
    let decision = policy.record(ExportOutcome::Rejected { status: 400 }, 1, 0.0, MID);
    assert!(!decision.retry_batch);
    assert!(policy.should_export(0.0));
}

/// `401` → one refresh → a second `401` stops telemetry for the page load.
#[test]
fn unauthorized_refreshes_once_then_stops() {
    let mut policy = ExportPolicy::default();

    let first = policy.record(ExportOutcome::Unauthorized, 1, 0.0, MID);
    assert!(first.refresh_token);
    assert!(!first.retry_batch, "the 401'd batch is dropped");
    assert!(
        !policy.should_export(0.0),
        "nothing is sent until the refresh"
    );
    assert!(policy.needs_refresh());

    policy.refreshed();
    assert!(policy.should_export(0.0));

    let second = policy.record(ExportOutcome::Unauthorized, 1, 0.0, MID);
    assert_eq!(
        second.transition,
        Some(Transition::Stopped(OffReason::Unauthorized))
    );
    assert!(!second.refresh_token);
    assert_eq!(policy.stopped(), Some(OffReason::Unauthorized));
    assert!(!policy.should_export(1e12));
}

/// A success between two `401`s earns the next one a fresh refresh.
#[test]
fn a_success_resets_the_refresh_budget() {
    let mut policy = ExportPolicy::default();
    policy.record(ExportOutcome::Unauthorized, 1, 0.0, MID);
    policy.refreshed();
    policy.record(ExportOutcome::Accepted, 1, 0.0, MID);

    let decision = policy.record(ExportOutcome::Unauthorized, 1, 0.0, MID);
    assert!(decision.refresh_token);
    assert_eq!(policy.stopped(), None);
}

#[test]
fn retryable_failures_back_off_and_retry_the_batch() {
    let mut policy = ExportPolicy::default();
    let outcome = ExportOutcome::Retryable {
        status: Some(503),
        retry_after_ms: None,
    };
    let decision = policy.record(outcome, 1, 0.0, MID);
    assert!(decision.retry_batch);
    assert_eq!(decision.transition, Some(Transition::StartedFailing));
    assert!(!policy.should_export(1.0));
    assert!(policy.should_export(backoff_ms(1, MID)));
}

/// `Retry-After` wins when it is longer than the backoff, up to a cap.
#[test]
fn retry_after_is_honoured_and_capped() {
    let mut policy = ExportPolicy::default();
    policy.record(
        ExportOutcome::Retryable {
            status: Some(429),
            retry_after_ms: Some(60_000.0),
        },
        1,
        0.0,
        MID,
    );
    assert!(!policy.should_export(59_999.0));
    assert!(policy.should_export(60_000.0));

    let mut policy = ExportPolicy::default();
    policy.record(
        ExportOutcome::Retryable {
            status: Some(429),
            retry_after_ms: Some(1e9),
        },
        1,
        0.0,
        MID,
    );
    assert!(policy.should_export(RETRY_AFTER_MAX_MS));
}

/// Jitter stays inside [half, all] of the exponential ceiling, and the ceiling
/// itself is capped.
#[test]
fn jitter_is_bounded() {
    for failures in 1..=20 {
        let ceiling = (BACKOFF_BASE_MS * 2_f64.powi(failures as i32 - 1)).min(BACKOFF_MAX_MS);
        let low = backoff_ms(failures, 0.0);
        let high = backoff_ms(failures, 0.999_999);
        assert!((low - ceiling / 2.0).abs() < 1e-6, "{failures}: {low}");
        assert!(
            high <= ceiling && high > ceiling * 0.99,
            "{failures}: {high}"
        );
        // Out-of-range draws are clamped, not trusted.
        assert!(backoff_ms(failures, 7.0) <= ceiling);
        assert!(backoff_ms(failures, -3.0) >= ceiling / 2.0);
    }
    assert!(backoff_ms(2, MID) > backoff_ms(1, MID), "exponential");
}

/// A batch is sent at most MAX_BATCH_ATTEMPTS times.
#[test]
fn a_batch_is_retried_a_bounded_number_of_times() {
    let mut policy = ExportPolicy::default();
    let outcome = ExportOutcome::Retryable {
        status: None,
        retry_after_ms: None,
    };
    assert!(policy.record(outcome, 1, 0.0, MID).retry_batch);
    assert!(policy.record(outcome, 2, 0.0, MID).retry_batch);
    assert!(
        !policy
            .record(outcome, MAX_BATCH_ATTEMPTS, 0.0, MID)
            .retry_batch
    );
}

/// A crowd that keeps failing stops for the session instead of hammering.
#[test]
fn repeated_failure_gives_up_for_the_session() {
    let mut policy = ExportPolicy::default();
    let outcome = ExportOutcome::Retryable {
        status: Some(502),
        retry_after_ms: None,
    };
    let mut transitions = Vec::new();
    for attempt in 1..=MAX_CONSECUTIVE_FAILURES {
        if let Some(transition) = policy.record(outcome, attempt, 0.0, MID).transition {
            transitions.push(transition);
        }
    }
    assert_eq!(
        transitions,
        [
            Transition::StartedFailing,
            Transition::Stopped(OffReason::GaveUp)
        ],
        "one line when it starts, one when it gives up — none in between"
    );
    assert!(!policy.should_export(1e15));
    // Nothing revives it.
    policy.record(ExportOutcome::Accepted, 1, 0.0, MID);
    assert!(!policy.should_export(1e15));
}

#[test]
fn recovery_is_reported_once_and_clears_the_backoff() {
    let mut policy = ExportPolicy::default();
    let outcome = ExportOutcome::Retryable {
        status: Some(503),
        retry_after_ms: None,
    };
    policy.record(outcome, 1, 0.0, MID);
    policy.record(outcome, 2, 0.0, MID);

    let decision = policy.record(ExportOutcome::Accepted, 1, 100_000.0, MID);
    assert_eq!(decision.transition, Some(Transition::Recovered));
    assert!(policy.should_export(100_000.0), "no residual wait");

    // The next failure starts from the bottom again.
    policy.record(outcome, 1, 200_000.0, 0.0);
    assert!(policy.should_export(200_000.0 + backoff_ms(1, 0.0)));
}
