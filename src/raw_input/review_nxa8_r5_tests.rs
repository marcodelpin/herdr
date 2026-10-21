//! Regression tests for codex review f5251-r5 on 9e7c800d (ADR-0013
//! amendment 1, herdr-nxa8). The Windows translator cases of findings 1 and
//! 2 live with the translator tests in `client/input/windows_vti.rs`; the
//! oracle case of finding 3 lives with the property test.

use super::*;
use std::time::{Duration, Instant};

/// A pump byte framer under a virtual clock, with the Windows default-colour
/// query sent at 0 ms (one-second reply window).
fn pump_framer_with_default_color_query() -> (RawInputByteFramer, Instant) {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    framer.set_pump_policy();
    framer.host_default_color_query_sent(Duration::from_secs(1));
    (framer, base)
}

/// Pump idle ticks every 10 ms from `from` to `to`.
fn ticks(framer: &mut RawInputByteFramer, base: Instant, from: u64, to: u64) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut now = from;
    while now + 10 <= to {
        now += 10;
        framer.set_test_clock(base + Duration::from_millis(now));
        chunks.extend(framer.flush_timeout());
    }
    framer.set_test_clock(base + Duration::from_millis(to));
    chunks
}

fn push_at(framer: &mut RawInputByteFramer, base: Instant, at: u64, bytes: &[u8]) -> Vec<Vec<u8>> {
    framer.set_test_clock(base + Duration::from_millis(at));
    framer.push(bytes)
}

/// Same class as codex f5251-r5 finding 2 (ADR-0013 amendment 1, D1): a
/// default-colour reply prefix that ends in ESC when its window closes is
/// dropped, and its ESC is kept as the possible start of the ST. `[` then
/// disproves the ST: the ESC starts the next token, so `ESC [3~` reaches the
/// pane as Delete, not as `[3~`.
#[test]
fn review_r5_2b_escape_after_default_color_tail_recovery_starts_the_next_key() {
    let (mut framer, base) = pump_framer_with_default_color_query();
    let reply_prefix = b"\x1b]10;rgb:aaaa/bbbb/cccc";
    let mut chunks = ticks(&mut framer, base, 0, 980);
    chunks.extend(push_at(&mut framer, base, 980, reply_prefix));
    chunks.extend(ticks(&mut framer, base, 980, 990));
    chunks.extend(push_at(&mut framer, base, 990, b"\x1b"));
    chunks.extend(ticks(&mut framer, base, 990, 1_010));
    chunks.extend(push_at(&mut framer, base, 1_015, b"[3~"));
    chunks.extend(ticks(&mut framer, base, 1_015, 1_300));

    let events = events_from_framed_chunks(chunks.clone());
    assert!(
        matches!(&events[..], [RawInputEvent::Key(key)]
            if key.code == crossterm::event::KeyCode::Delete),
        "{chunks:?}: {events:?}"
    );
    assert!(!framer.has_pending_input());
}
