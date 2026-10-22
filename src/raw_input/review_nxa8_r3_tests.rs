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
        RawInputEvent::Paste(text) => {
            Some([BRACKETED_PASTE_START, text.as_bytes(), BRACKETED_PASTE_END].concat())
        }
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
    assert!(framer.idle_tick().is_empty());
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

/// The bytes the direct-graphics filter keeps while a response is awaited
/// (`client::direct_graphics::InputFilter`): a trailing prefix of the kitty
/// response introducer `ESC _ G`, released once no response is awaited.
struct GraphicsFilterModel {
    pending: Vec<u8>,
    interested_until: u64,
}

impl GraphicsFilterModel {
    fn push(&mut self, bytes: &[u8], now: u64) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        if now >= self.interested_until {
            return std::mem::take(&mut self.pending);
        }
        let keep = (1..3)
            .rev()
            .find(|len| self.pending.ends_with(&b"\x1b_G"[..*len]))
            .unwrap_or(0);
        let emit = self.pending.len() - keep;
        self.pending.drain(..emit).collect()
    }

    fn flush_if_inactive(&mut self, now: u64) -> Option<Vec<u8>> {
        (!self.pending.is_empty() && now >= self.interested_until)
            .then(|| std::mem::take(&mut self.pending))
    }
}

/// The idle turns of the Unix reader (`client/input.rs`) with no further
/// input until `until`: one wait per turn from `reader_idle_wait`, then the
/// graphics filter's release or the framer's flush. Returns the time each
/// chunk was sent.
fn unix_reader_idle_turns(
    framer: &mut RawInputByteFramer,
    filter: &mut GraphicsFilterModel,
    base: Instant,
    mut now: u64,
    mut idle_flush_at: Option<u64>,
    until: u64,
) -> Vec<(u64, Vec<u8>)> {
    let mut sent = Vec::new();
    while now < until {
        let idle_ms = idle_flush_at
            .filter(|_| framer.has_pending_input())
            .map(|at| at.saturating_sub(now) as i32);
        let framer_wait = match (idle_ms, framer.held_input_release_ms()) {
            (Some(idle), Some(release)) => Some(idle.min(release)),
            (idle, release) => idle.or(release),
        };
        let Some((wait, framer_due)) = reader_idle_wait(framer_wait, !filter.pending.is_empty())
        else {
            break;
        };
        now += wait.max(0) as u64;
        framer.set_test_clock(base + Duration::from_millis(now));
        let chunks = if let Some(data) = filter.flush_if_inactive(now) {
            let chunks = framer.push(&data);
            idle_flush_at = Some(now + framer.idle_flush_timeout_ms(false) as u64);
            chunks
        } else if framer_due {
            idle_flush_at = None;
            framer.flush_timeout()
        } else {
            Vec::new()
        };
        sent.extend(chunks.into_iter().map(|chunk| (now, chunk)));
    }
    sent
}

/// r3-2 (codex review f5251-r3, `client/input.rs:181`): while the
/// direct-graphics filter keeps an ESC for an awaited response, the reader
/// polled only the filter, so bytes the framer held waited for the response
/// window (here until 4650 ms) instead of their own bound. With one wait per
/// turn, `a` and ESC held after a dropped OSC header are released by their
/// tail deadline and no later than the pending-input backstop.
#[test]
fn review_r3_2_graphics_wait_does_not_starve_held_input() {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    framer.host_color_query_sent();
    let mut filter = GraphicsFilterModel {
        pending: Vec::new(),
        interested_until: 0,
    };

    // An OSC 4 header arrives and its reply stops: dropped at the idle wait.
    let mut sent = Vec::new();
    let chunks = framer.push(b"\x1b]4;50;");
    sent.extend(chunks.into_iter().map(|chunk| (0, chunk)));
    let idle = framer.idle_flush_timeout_ms(false) as u64;
    sent.extend(unix_reader_idle_turns(
        &mut framer,
        &mut filter,
        base,
        0,
        Some(idle),
        600,
    ));
    assert!(!framer.has_pending_input(), "the header is dropped");

    // `a` and ESC are typed and held as a possible reply tail.
    framer.set_test_clock(base + Duration::from_millis(600));
    let chunks = framer.push(&filter.push(b"a\x1b", 600));
    sent.extend(chunks.into_iter().map(|chunk| (600, chunk)));
    assert!(framer.has_pending_input(), "a and ESC are held");

    // A direct-graphics response is awaited from 700 to 4650 ms, and the
    // filter keeps an ESC that may start it.
    filter.interested_until = 4650;
    framer.set_test_clock(base + Duration::from_millis(700));
    let chunks = framer.push(&filter.push(b"\x1b", 700));
    sent.extend(chunks.into_iter().map(|chunk| (700, chunk)));
    assert!(!filter.pending.is_empty(), "the filter keeps the ESC");

    sent.extend(unix_reader_idle_turns(
        &mut framer,
        &mut filter,
        base,
        700,
        None,
        2_200,
    ));
    let bound = 600 + PENDING_INPUT_AGE_LIMIT.as_millis() as u64;
    let released: Vec<u64> = sent
        .iter()
        .filter(|(_, chunk)| chunk.as_slice() == b"a" || chunk.as_slice() == b"\x1b")
        .map(|(at, _)| *at)
        .collect();
    assert_eq!(
        unix_typed_events(
            &sent
                .iter()
                .map(|(_, chunk)| chunk.clone())
                .collect::<Vec<_>>()
        ),
        vec![b"a".to_vec(), b"\x1b".to_vec()],
        "sent by 2200 ms: {sent:?}"
    );
    assert!(
        released.iter().all(|at| *at <= bound),
        "held input released after its {bound} ms bound: {sent:?}"
    );
    assert!(!framer.has_pending_input());
}
