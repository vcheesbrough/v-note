//! What `permessage-deflate` (#342) is actually worth on a v-note replay, and
//! whether the level this server ships is the right one.
//!
//! #342 predicts **~4.2x** on a coalesced replay frame, but its table does not
//! say which deflate level produced it — and `realtime/socket.rs` deliberately
//! ships **level 6** rather than 9, because #323 bought a 13% replay-latency
//! win that compression trades CPU against. That leaves a gap worth closing
//! before the dev A/B runs: if level 6 were far off the headline, the number
//! the card is judged on would be wrong for the build being judged.
//!
//! So this measures the real thing: a replay payload serialized through the
//! real `PageServerMessage` serde impl, deflated the way `permessage-deflate`
//! deflates (raw deflate, no zlib wrapper), at each level the decision was
//! between.
//!
//! **This is not the card's acceptance measurement.** That one is wire bytes
//! and wall time A/B'd across two deployed images with a real client, and it
//! has to happen on dev. This is the part that can be pinned in CI, and it is
//! the part that would silently regress: the ratio depends on the payload
//! shape, so a future change to stroke encoding that wrecks compressibility
//! should fail here rather than be discovered in a dashboard.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::DeflateEncoder;
use protocol::{
    PageReplay, PageServerMessage, SOLID_ROUND_PRESSURE_STYLE_VERSION, SOLID_ROUND_TOOL,
    SolidRoundParameters, Stroke, StrokeBatch, StrokePoint, StrokeStyle,
};

/// The level `realtime/socket.rs` ships (`Options::with_balanced_compression`).
const SHIPPED_LEVEL: u32 = 6;

/// Deflate exactly as `permessage-deflate` does: raw DEFLATE, no zlib header or
/// trailer. RFC 7692 also strips the final empty block, which is 4 bytes and
/// irrelevant at this scale.
fn deflated_len(payload: &[u8], level: u32) -> usize {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(payload).expect("deflate should accept");
    encoder.finish().expect("deflate should finish").len()
}

/// A wandering, digitizer-shaped path quantized to 2 dp — deliberately *not* a
/// smooth analytic curve.
///
/// #342 is explicit that this matters: its own predecessor #212 measured a
/// smooth synthetic sine wave, compressed far better than real ink, and
/// produced the 3.0x / 0.91 MB figures the card tells you not to reuse. A
/// pseudo-random walk with a slow drift is the closest honest stand-in for a
/// stylus without shipping a captured trace as a fixture.
fn digitizer_points(count: usize, seed: u64) -> Vec<StrokePoint> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut next_unit = || {
        // xorshift64*, enough for a shaped payload and reproducible across runs.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(2_685_821_657_736_338_717) >> 11) as f64 / (1u64 << 53) as f64
    };

    let (mut x, mut y) = (400.0_f64, 300.0_f64);
    let (mut dx, mut dy) = (1.7_f64, -0.9_f64);
    (0..count)
        .map(|index| {
            // Momentum plus jitter: successive points stay close (as a stylus
            // does) without the whole path being analytically predictable.
            dx = (dx + (next_unit() - 0.5) * 0.8).clamp(-4.0, 4.0);
            dy = (dy + (next_unit() - 0.5) * 0.8).clamp(-4.0, 4.0);
            x = (x + dx).clamp(0.0, 1400.0);
            y = (y + dy).clamp(0.0, 2000.0);
            StrokePoint {
                x: (x * 100.0).round() / 100.0,
                y: (y * 100.0).round() / 100.0,
                t: index as i64 * 8,
                pressure: Some(((next_unit() * 0.6 + 0.35) * 100.0).round() / 100.0),
            }
        })
        .collect()
}

fn dense_replay(batches: usize, points_per_stroke: usize) -> PageReplay {
    PageReplay {
        page_id: "page_01k9v4v0t8hqz2m7cxr3w5nabc".to_string(),
        last_seq: batches as u64,
        batches: (0..batches)
            .map(|index| StrokeBatch {
                seq: index as u64 + 1,
                client_batch_id: format!("batch_{index:08x}"),
                strokes: vec![Stroke {
                    id: format!("stroke_{:032x}", index as u128 * 0x9E37_79B9),
                    style: StrokeStyle {
                        tool_kind: SOLID_ROUND_TOOL.to_string(),
                        style_version: SOLID_ROUND_PRESSURE_STYLE_VERSION,
                        parameters: SolidRoundParameters {
                            color: "#006400".to_string(),
                            width: 2.0,
                            cap_style: "round".to_string(),
                            join_style: "round".to_string(),
                        },
                    },
                    points: digitizer_points(points_per_stroke, index as u64 + 1),
                }],
            })
            .collect(),
        tombstones: Vec::new(),
    }
}

/// The headline #342 is judged on: a coalesced replay frame compresses by
/// roughly 4x at the level this server actually ships.
///
/// The bound is deliberately loose (>= 3.0x). The point is to catch a payload
/// shape that stops compressing, not to pin a ratio that legitimately drifts
/// with stroke encoding — and a tight assertion here would fail for reasons
/// that have nothing to do with #342.
#[test]
fn a_coalesced_replay_frame_compresses_about_four_fold_at_the_shipped_level() {
    let replay = PageServerMessage::PageReplay(dense_replay(600, 150));
    let payload = serde_json::to_vec(&replay).expect("replay should serialize");
    let compressed = deflated_len(&payload, SHIPPED_LEVEL);
    let ratio = payload.len() as f64 / compressed as f64;

    println!(
        "coalesced replay, level {SHIPPED_LEVEL}: {} -> {} bytes ({ratio:.2}x)",
        payload.len(),
        compressed,
    );

    assert!(
        ratio >= 3.0,
        "a coalesced replay should still compress several-fold; got {ratio:.2}x \
         ({} -> {} bytes). If stroke encoding changed, re-measure #342's headline.",
        payload.len(),
        compressed,
    );
}

/// Why level 6 and not 9: the extra ratio at 9 is small enough that it does not
/// pay for the CPU on a multi-megabyte replay frame, which is the latency #323
/// won and #342 must not give back.
///
/// Asserted as a *relationship*, not as absolute numbers: 9 must not be
/// dramatically better than 6, or the shipped choice would be wrong and this
/// test should say so.
#[test]
fn level_nine_does_not_beat_the_shipped_level_enough_to_justify_the_cpu() {
    let replay = PageServerMessage::PageReplay(dense_replay(600, 150));
    let payload = serde_json::to_vec(&replay).expect("replay should serialize");

    let shipped = deflated_len(&payload, SHIPPED_LEVEL);
    let best = deflated_len(&payload, 9);
    let extra = (shipped - best) as f64 / shipped as f64;

    println!(
        "level {SHIPPED_LEVEL}: {shipped} bytes; level 9: {best} bytes \
         ({:.2}% smaller)",
        extra * 100.0,
    );

    assert!(
        extra < 0.10,
        "level 9 is {:.1}% smaller than the shipped level {SHIPPED_LEVEL} — large \
         enough that the level choice in realtime/socket.rs should be revisited",
        extra * 100.0,
    );
}

/// The credit #342 specifically owes to #323 having coalesced first: one big
/// frame compresses from a single dictionary, where N small frames each start
/// cold.
///
/// This is the claim that corrects #212's reasoning — that coalescing alone
/// reached per-message-deflate's size "without needing `permessage-deflate` at
/// all" — so it is worth holding to a test rather than a table. Context
/// takeover is deliberately *not* modelled here: this is the per-message
/// worst case, which is what a client that negotiates
/// `server_no_context_takeover` actually gets.
#[test]
fn one_coalesced_frame_beats_deflating_each_batch_separately() {
    let replay = dense_replay(600, 150);

    let coalesced = deflated_len(
        &serde_json::to_vec(&PageServerMessage::PageReplay(replay.clone()))
            .expect("replay should serialize"),
        SHIPPED_LEVEL,
    );

    let per_message: usize = replay
        .batches
        .iter()
        .map(|batch| {
            let frame = PageServerMessage::StrokeBatch(batch.clone());
            let payload = serde_json::to_vec(&frame).expect("batch should serialize");
            deflated_len(&payload, SHIPPED_LEVEL)
        })
        .sum();

    println!(
        "coalesced: {coalesced} bytes; per-message: {per_message} bytes \
         ({:.2}x better)",
        per_message as f64 / coalesced as f64,
    );

    assert!(
        coalesced < per_message,
        "coalescing before compressing should win: {coalesced} >= {per_message}"
    );
}
