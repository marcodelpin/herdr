//! Regression tests for codex review f5251-r5 on 9e7c800d (ADR-0013
//! amendment 1, herdr-nxa8). The Windows translator cases of findings 1 and
//! 2 live with the translator tests in `client/input/windows_vti.rs`; the
//! oracle case of finding 3 lives with the property test.

use super::*;
use std::time::{Duration, Instant};

/// What a case expects to reach the pane, one entry per typed run.
type Delivered<'a> = &'a [&'a [u8]];

/// A colour reply case: header, tail, the input that ends the reply, and
/// what reaches the pane.
type ReplyCase<'a> = (&'a [u8], &'a [u8], &'a [u8], Delivered<'a>);

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

/// What reached the pane, one entry per escape-led event; the typed bytes
/// between them run together, so text splits do not matter but a key typed
/// as literal text does.
fn typed_runs(events: &[RawInputEvent]) -> Vec<Vec<u8>> {
    let mut runs: Vec<Vec<u8>> = Vec::new();
    for event in events {
        let bytes = match event {
            RawInputEvent::Key(key) => key.vt_bytes().unwrap_or_default().to_vec(),
            RawInputEvent::Text(text) => text.as_str().as_bytes().to_vec(),
            _ => continue,
        };
        match runs.last_mut() {
            Some(run) if bytes.first() != Some(&ESC) && run.first() != Some(&ESC) => {
                run.extend(bytes);
            }
            _ => runs.push(bytes),
        }
    }
    runs
}

/// Same class as codex f5251-r5 finding 1 (ADR-0013 amendment 1, D4): a byte
/// from 0x01 to 0x1f is never X10 payload. It ends an `ESC [ M` frame, in the
/// buffer or after a released prefix. An ESC starts the next token, so the
/// key after a broken report keeps its ESC; any other such byte is typed on
/// the Unix reader and dropped with the frame on the pump, which replays the
/// key record it staged for it. NUL stays payload (xterm's coordinate past
/// its limit). Before, a frame in the buffer took six bytes whatever they
/// were (since upstream), and the pump dropped a disproving ESC with the tail
/// of a released prefix.
#[test]
fn review_r5_1b_control_byte_ends_an_x10_frame() {
    // (pump, released prefix or none, rest, what reaches the pane)
    let cases: [(bool, &[u8], &[u8], Delivered<'_>); 12] = [
        (false, b"", b"\x1b[M\x1b[A", &[b"\x1b[A"]),
        (true, b"", b"\x1b[M\x1b[A", &[b"\x1b[A"]),
        (false, b"", b"\x1b[M!\x1b[A", &[b"\x1b[A"]),
        (true, b"", b"\x1b[M!\x1b[A", &[b"\x1b[A"]),
        (false, b"", b"\x1b[M\rab", &[b"\rab"]),
        (true, b"", b"\x1b[M\rab", &[b"ab"]),
        (false, b"", b"\x1b\x1b[M\rab", &[b"\x1b", b"\rab"]),
        (true, b"", b"\x1b\x1b[M\rab", &[b"\x1b", b"ab"]),
        (false, b"", b"\x1b[M \x00!x", &[b"x"]),
        (true, b"", b"\x1b[M \x00!x", &[b"x"]),
        (true, b"\x1b", b"[M\x1b[A", &[b"\x1b", b"\x1b[A"]),
        (true, b"\x1b[", b"M\x1b[A", &[b"\x1b[", b"\x1b[A"]),
    ];
    let mut wrong = Vec::new();
    for (pump, prefix, rest, expected) in cases {
        let base = Instant::now();
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        framer.set_test_clock(base);
        let events: Vec<RawInputEvent> = if pump {
            framer.set_pump_policy();
            let mut chunks = Vec::new();
            let mut at = 0;
            if !prefix.is_empty() {
                // Released at the first quiet idle tick; the rest comes
                // inside its tail hold.
                chunks.extend(push_at(&mut framer, base, 0, prefix));
                chunks.extend(ticks(&mut framer, base, 0, 300));
                at = 300;
            }
            chunks.extend(push_at(&mut framer, base, at, rest));
            chunks.extend(ticks(&mut framer, base, at, 3_000));
            events_from_framed_chunks(chunks)
        } else {
            unix_reader(&mut framer, base, &[(0, rest)], 3_000)
                .iter()
                .flat_map(|chunk| parse_framed_input(chunk))
                .collect()
        };
        let expected: Vec<Vec<u8>> = expected.iter().map(|run| run.to_vec()).collect();
        if typed_runs(&events) != expected || framer.has_pending_input() {
            wrong.push(format!(
                "pump={pump} {} then {}: {events:?}",
                prefix.escape_ascii(),
                rest.escape_ascii()
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
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

/// Same class as codex f5251-r5 finding 2, found by the extended property
/// stream (ADR-0013 amendment 1, D1): a colour reply cut off before its
/// terminator, whose header an idle release dropped. Its tail is held in the
/// header's context. When a byte that ends the reply's shape comes (an
/// ESC-led key, a typed letter) or the context's deadline passes, the tail
/// is reply bytes once header and tail spell the reply through `rgb:`, and
/// is dropped, after a semantic key or not. Before, only after a semantic
/// key: otherwise the tail was typed. A shorter shape is what typing after
/// Alt+] gives (#344), and stays typed. On the Unix reader and on the pump,
/// with a colour reply awaited or not.
#[test]
fn review_r5_2e_unterminated_colour_reply_tail_is_not_typed() {
    // (header, tail, the input that ends the reply, what reaches the pane)
    let cases: [ReplyCase<'_>; 8] = [
        (
            b"\x1b]4;171;rgb:b397/69a",
            b"b/5249",
            b"\x1bOP",
            &[b"\x1bOP"],
        ),
        (b"\x1b]4;171;rgb:b397/69a", b"b/5249", b"x", &[b"x"]),
        (b"\x1b]4;171;rgb:b397/69a", b"b/5249", b"", &[]),
        (
            b"\x1b]4",
            b";192;rgb:21ec/c639/488b",
            b"\x1b[A",
            &[b"\x1b[A"],
        ),
        (b"\x1b]", b"11;rgb:aaaa/bbbb/cc", b"\x1b[A", &[b"\x1b[A"]),
        (b"\x1b]", b"11;rgb:aaaa/bbbb/cc", b"", &[]),
        (b"\x1b]11;rgb:aaaa/bbbb/cccc", b"\x1b", b"[A", &[b"\x1b[A"]),
        (b"\x1b]", b"1a", b"\r", &[b"1a\r"]),
    ];
    let mut wrong = Vec::new();
    for pump in [false, true] {
        // After the idle release dropped the header, inside its tail hold.
        let at = if pump { 30 } else { 600 };
        for query in [false, true] {
            for (header, tail, end, expected) in cases {
                let base = Instant::now();
                let mut framer = RawInputByteFramer::with_host_input_policy(false);
                framer.set_test_clock(base);
                if query {
                    framer.host_color_query_sent();
                }
                let mut reads = vec![(0, header), (at, tail)];
                if !end.is_empty() {
                    reads.push((at + 100, end));
                }
                let events: Vec<RawInputEvent> = if pump {
                    framer.set_pump_policy();
                    let mut chunks = Vec::new();
                    let mut now = 0;
                    for (read_at, bytes) in reads {
                        chunks.extend(ticks(&mut framer, base, now, read_at));
                        chunks.extend(push_at(&mut framer, base, read_at, bytes));
                        now = read_at;
                    }
                    chunks.extend(ticks(&mut framer, base, now, 5_000));
                    events_from_framed_chunks(chunks)
                } else {
                    unix_reader(&mut framer, base, &reads, 5_000)
                        .iter()
                        .flat_map(|chunk| parse_framed_input(chunk))
                        .collect()
                };
                let expected: Vec<Vec<u8>> = expected.iter().map(|run| run.to_vec()).collect();
                if typed_runs(&events) != expected || framer.has_pending_input() {
                    wrong.push(format!(
                        "pump={pump} query={query} {} then {} then {}: {events:?}",
                        header.escape_ascii(),
                        tail.escape_ascii(),
                        end.escape_ascii()
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The same rule after an idle release dropped an OSC header that ended in
/// ESC (ADR-0013 amendment 1, D1): that ESC was only the possible start of
/// the ST. `\` completes the ST and is dropped; `[` disproves it, so the ESC
/// starts the next token and `ESC [3~` reaches the pane as Delete, not as
/// `[3~`. On the Unix reader and on the pump.
#[test]
fn review_r5_2d_escape_ending_a_dropped_osc_header_starts_the_next_key() {
    use crossterm::event::KeyCode;
    let header: &[u8] = b"\x1b]4;1;rgb:aaaa/bbbb/cccc\x1b";
    for pump in [false, true] {
        // After the idle release dropped the header, inside its tail hold.
        let at = if pump { 30 } else { 600 };
        for (tail, expected) in [
            (b"[3~".as_slice(), KeyCode::Delete),
            (b"\\x".as_slice(), KeyCode::Char('x')),
        ] {
            let base = Instant::now();
            let mut framer = RawInputByteFramer::with_host_input_policy(false);
            framer.set_test_clock(base);
            let (chunks, events): (Vec<Vec<u8>>, Vec<RawInputEvent>) = if pump {
                framer.set_pump_policy();
                let mut chunks = push_at(&mut framer, base, 0, header);
                chunks.extend(ticks(&mut framer, base, 0, at));
                chunks.extend(push_at(&mut framer, base, at, tail));
                chunks.extend(ticks(&mut framer, base, at, 3_000));
                (chunks.clone(), events_from_framed_chunks(chunks))
            } else {
                let chunks = unix_reader(&mut framer, base, &[(0, header), (at, tail)], 3_000);
                let events = chunks
                    .iter()
                    .flat_map(|chunk| parse_framed_input(chunk))
                    .collect();
                (chunks, events)
            };
            assert!(
                matches!(&events[..], [RawInputEvent::Key(key)]
                    if key.code == expected && key.modifiers.is_empty()),
                "pump={pump} {}: {chunks:?}: {events:?}",
                tail.escape_ascii()
            );
            assert!(!framer.has_pending_input());
        }
    }
}
