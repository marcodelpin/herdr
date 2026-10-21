//! Regression tests for herdr-nxa8 (a Delete key or X10 mouse report split
//! after its ESC, upstream #5251) and finding r3-1 of codex review f5251-r3
//! on dc224a4f (ADR-0012). They use only framer API that 40aeb849 already
//! had, so each one can be run against that revision to show it fails there.

use super::*;
use std::time::{Duration, Instant};

/// The bytes of each typed event (key, text, paste) production decodes from
/// the Unix reader's chunks, one entry per event.
fn unix_typed_events(chunks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    chunks
        .iter()
        .flat_map(|chunk| parse_framed_input(chunk))
        .filter_map(|event| typed_event_bytes(&event))
        .collect()
}

/// The same for the Windows pump, which decodes one event per chunk.
fn windows_typed_events(chunks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    events_from_framed_chunks(chunks.to_vec())
        .iter()
        .filter_map(typed_event_bytes)
        .collect()
}

fn typed_event_bytes(event: &RawInputEvent) -> Option<Vec<u8>> {
    match event {
        RawInputEvent::Key(key) => Some(key.vt_bytes().unwrap_or_default().to_vec()),
        RawInputEvent::Text(text) => Some(text.as_str().as_bytes().to_vec()),
        RawInputEvent::Paste(text) => Some(
            [BRACKETED_PASTE_START, text.as_bytes(), BRACKETED_PASTE_END].concat(),
        ),
        _ => None,
    }
}

/// The Unix stdin reader of `client/input.rs` under a virtual clock: `first`
/// arrives at 0, `rest` arrives `gap_ms` later, and nothing after it. The
/// reader flushes after its idle wait, again after the held-input wait, and
/// whenever the framer reports a release time.
fn unix_reader(
    first: &[u8],
    gap_ms: u64,
    rest: &[u8],
    mouse_capture: bool,
    color_query: bool,
) -> Vec<Vec<u8>> {
    let base = Instant::now();
    let mut now = 0_u64;
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    if color_query {
        framer.host_color_query_sent();
    }
    let mut chunks = framer.push(first);
    let mut step = framer.idle_flush_timeout_ms(mouse_capture) as u64;
    let mut idle_flushes = 0;
    while framer.has_pending_input() && now + step < gap_ms && idle_flushes < 8 {
        now += step;
        framer.set_test_clock(base + Duration::from_millis(now));
        chunks.extend(framer.flush_timeout());
        idle_flushes += 1;
        step = framer
            .held_input_release_ms()
            .unwrap_or(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS) as u64;
    }
    now = gap_ms;
    framer.set_test_clock(base + Duration::from_millis(now));
    chunks.extend(framer.push(rest));
    let wait = framer.idle_flush_timeout_ms(mouse_capture) as u64;
    now += wait;
    framer.set_test_clock(base + Duration::from_millis(now));
    chunks.extend(framer.flush_timeout());
    for _ in 0..8 {
        let Some(wait) = framer.held_input_release_ms() else {
            break;
        };
        now += wait as u64;
        framer.set_test_clock(base + Duration::from_millis(now));
        chunks.extend(framer.flush_timeout());
    }
    assert!(
        !framer.has_pending_input(),
        "the reader must drain: {:?}",
        framer.buffer.escape_ascii().to_string()
    );
    chunks
}

/// herdr-nxa8 / #5251: ESC, 300 ms, `[3~`. The ESC reached the pane as an
/// Escape key before its tail arrived; the tail is dropped, never typed.
#[test]
fn review_nxa8_split_delete_after_released_escape_is_not_typed() {
    for mouse_capture in [false, true] {
        for color_query in [false, true] {
            let chunks = unix_reader(b"\x1b", 300, b"[3~", mouse_capture, color_query);
            assert_eq!(
                unix_typed_events(&chunks),
                vec![b"\x1b".to_vec()],
                "capture {mouse_capture} query {color_query}: {chunks:?}"
            );
            assert_eq!(windows_typed_events(&chunks), vec![b"\x1b".to_vec()]);
        }
    }
}

/// herdr-nxa8 / #5251: ESC, 300 ms, `[MCd2` (legacy X10 mouse release).
#[test]
fn review_nxa8_split_x10_mouse_after_released_escape_is_not_typed() {
    for mouse_capture in [false, true] {
        for color_query in [false, true] {
            let chunks = unix_reader(b"\x1b", 300, b"[MCd2", mouse_capture, color_query);
            assert_eq!(
                unix_typed_events(&chunks),
                vec![b"\x1b".to_vec()],
                "capture {mouse_capture} query {color_query}: {chunks:?}"
            );
            assert_eq!(windows_typed_events(&chunks), vec![b"\x1b".to_vec()]);
        }
    }
}

/// herdr-nxa8: the same split one byte later, `ESC [` released as Alt+[
/// and then `3~`: the tail is dropped, never typed.
#[test]
fn review_nxa8_split_delete_after_released_csi_introducer_is_not_typed() {
    for mouse_capture in [false, true] {
        let chunks = unix_reader(b"\x1b[", 300, b"3~", mouse_capture, false);
        let typed = unix_typed_events(&chunks);
        assert!(
            !typed.iter().any(|event| event == b"3" || event == b"~"),
            "capture {mouse_capture}: {chunks:?}"
        );
    }
}

/// herdr-nxa8: a person typing `[` and then `D` after Escape (vim `[D`)
/// is typing, not a split key: two reads far apart are both typed.
#[test]
fn review_nxa8_typed_bracket_command_after_escape_is_kept() {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    let mut chunks = framer.push(b"\x1b");
    framer.set_test_clock(base + Duration::from_millis(10));
    chunks.extend(framer.flush_timeout());
    framer.set_test_clock(base + Duration::from_millis(200));
    chunks.extend(framer.push(b"["));
    framer.set_test_clock(base + Duration::from_millis(280));
    chunks.extend(framer.push(b"D"));
    framer.set_test_clock(base + Duration::from_millis(290));
    chunks.extend(framer.flush_timeout());
    assert_eq!(
        unix_typed_events(&chunks),
        vec![b"\x1b".to_vec(), b"[".to_vec(), b"D".to_vec()]
    );
    assert!(!framer.has_pending_input());
}

/// r3-1 (framer level, the Unix one-flush colour reply hold standing in for
/// the Windows query hold): text held after a dropped OSC header, then ESC,
/// then a semantic key. Every held byte, including an Escape found after
/// emitting the text, is emitted before the semantic key.
#[test]
fn review_r3_1_interruption_emits_escape_found_after_held_text() {
    let mut framer = RawInputFramer::default();
    framer.byte_framer.host_color_query_sent();
    assert!(framer.push(b"\x1b]4;50;").is_empty());
    assert!(framer.flush_timeout().is_empty());
    assert!(framer.push(b"a").is_empty());
    assert!(framer.push(b"\x1b").is_empty());

    let events = framer.flush_interrupted();
    let codes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RawInputEvent::Key(key) => Some(key.code),
            _ => None,
        })
        .collect();
    assert_eq!(
        codes,
        vec![
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyCode::Esc
        ],
        "{events:?}"
    );
    assert!(!framer.byte_framer.has_pending_input());
}
