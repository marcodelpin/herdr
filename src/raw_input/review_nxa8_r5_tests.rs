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

/// The Unix stdin reader loop under a virtual clock, through the production
/// `ReaderSchedule`: one read per `(ms, bytes)`, idle wakes in between, and
/// idle wakes until `until` ms.
fn unix_reader(
    framer: &mut RawInputByteFramer,
    base: Instant,
    reads: &[(u64, &[u8])],
    until: u64,
) -> Vec<Vec<u8>> {
    let mut schedule = ReaderSchedule::default();
    let mut chunks = Vec::new();
    let mut now = 0;
    for &(at, bytes) in reads.iter().chain([(until, &[][..])].iter()) {
        for _ in 0..100_000 {
            let Some((wait, framer_due)) = schedule.wait(framer, false) else {
                break;
            };
            let wake = now + wait.max(0) as u64;
            if wake >= at {
                break;
            }
            now = wake;
            framer.set_test_clock(base + Duration::from_millis(now));
            chunks.extend(schedule.on_idle(framer, None, framer_due, false));
        }
        now = at;
        framer.set_test_clock(base + Duration::from_millis(now));
        if !bytes.is_empty() {
            chunks.extend(schedule.on_read(framer, vec![bytes.to_vec()], false));
        }
    }
    chunks
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

/// Same class as codex f5251-r5 finding 2 (ADR-0013 amendment 1, D1): an ESC
/// inside a control string whose next byte is not `\` aborts the string, as
/// the terminal's own parser does. A truncated colour reply still in the
/// buffer, an empty OSC or a DCS, or a DCS tail under discard, is dropped,
/// and the key or paste that follows it reaches the pane: on the Unix reader
/// and on the pump, with or without a reply awaited, whether the key came in
/// the same idle interval or after a flush. Before, every one of these lost
/// the key or the paste.
#[test]
fn review_r5_2c_escape_aborts_a_truncated_control_string() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let strings: [&[u8]; 4] = [
        b"\x1b]10;rgb:aaaa/bbbb/cccc",
        b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b",
        b"\x1b]",
        b"\x1bP1$r",
    ];
    let mut cases = 0;
    let mut lost = Vec::new();
    for pump in [false, true] {
        for query in 0..3 {
            for gap in [0, 5, 30] {
                for string in strings {
                    for paste in [false, true] {
                        let input: &[u8] = if paste {
                            b"\x1b[200~abc\x1b[201~"
                        } else {
                            b"\x1b[3~"
                        };
                        let base = Instant::now();
                        let mut framer = RawInputByteFramer::with_host_input_policy(false);
                        framer.set_test_clock(base);
                        if pump {
                            framer.set_pump_policy();
                        }
                        match query {
                            1 => framer.host_color_query_sent(),
                            2 => framer.host_default_color_query_sent(Duration::from_secs(1)),
                            _ => {}
                        }
                        let events = if pump {
                            let mut chunks = push_at(&mut framer, base, 0, string);
                            chunks.extend(ticks(&mut framer, base, 0, gap));
                            chunks.extend(push_at(&mut framer, base, gap, input));
                            chunks.extend(ticks(&mut framer, base, gap, 5_000));
                            events_from_framed_chunks(chunks)
                        } else {
                            unix_reader(&mut framer, base, &[(0, string), (gap, input)], 5_000)
                                .iter()
                                .flat_map(|chunk| parse_framed_input(chunk))
                                .collect()
                        };
                        let delivered = match &events[..] {
                            [RawInputEvent::Key(key)] => {
                                !paste
                                    && key.code == KeyCode::Delete
                                    && key.modifiers == KeyModifiers::NONE
                            }
                            [RawInputEvent::Paste(text)] => paste && text == "abc",
                            _ => false,
                        };
                        cases += 1;
                        if !delivered || framer.has_pending_input() {
                            lost.push(format!(
                                "pump={pump} query={query} gap={gap} {} then {}: {events:?}",
                                string.escape_ascii(),
                                input.escape_ascii()
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {cases} lost the key or the paste:\n{}",
        lost.len(),
        lost.join("\n")
    );
}
