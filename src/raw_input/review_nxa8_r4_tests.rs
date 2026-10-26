//! Regression tests for codex review f5251-r4 on 61814714 and the native
//! Windows CI red on e0e6d7e6 (ADR-0013, herdr-nxa8). The Windows translator
//! cases of findings 1 and 2 live with the translator tests in
//! `client/input/windows_vti.rs`; the oracle cases of finding 4 live with the
//! property test.

use super::*;
use std::time::{Duration, Instant};

/// The Unix stdin reader loop under a virtual clock, through the production
/// `ReaderSchedule`: idle waits run `on_idle`, reads run `on_read`.
struct Reader {
    base: Instant,
    now: Duration,
    framer: RawInputByteFramer,
    schedule: ReaderSchedule,
    /// Every chunk the framer released, with the time it left.
    out: Vec<(Duration, Vec<u8>)>,
}

impl Reader {
    fn new() -> Self {
        let base = Instant::now();
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        framer.set_test_clock(base);
        Self {
            base,
            now: Duration::ZERO,
            framer,
            schedule: ReaderSchedule::default(),
            out: Vec::new(),
        }
    }

    fn set_clock(&mut self, now: Duration) {
        self.now = now;
        self.framer.set_test_clock(self.base + now);
    }

    fn record(&mut self, chunks: Vec<Vec<u8>>) {
        let now = self.now;
        self.out
            .extend(chunks.into_iter().map(|chunk| (now, chunk)));
    }

    /// Waits as the reader does until `until`, waking on every idle timeout.
    fn idle_until(&mut self, until: Duration) {
        while let Some((wait, framer_due)) = self.schedule.wait(&self.framer, false) {
            let wake = self.now + Duration::from_millis(wait.max(0) as u64);
            if wake >= until {
                break;
            }
            self.set_clock(wake);
            let chunks = self
                .schedule
                .on_idle(&mut self.framer, None, framer_due, false);
            self.record(chunks);
        }
        self.set_clock(until);
    }

    /// A read at the current time; `filtered` is what the graphics filter
    /// let through, possibly nothing.
    fn read(&mut self, filtered: &[&[u8]]) {
        let filtered = filtered.iter().map(|chunk| chunk.to_vec()).collect();
        let chunks = self.schedule.on_read(&mut self.framer, filtered, false);
        self.record(chunks);
    }
}

/// codex f5251-r4 finding 3 (ADR-0013 D3): reads that bring the framer
/// nothing (a kitty graphics response the filter holds, one byte every
/// 0.5 ms) still run its deadlines. `a ESC`, held after a dropped OSC header,
/// leaves at its 1600 ms tail deadline, not when the reads stop at 2200 ms.
#[test]
fn review_r4_3_graphics_only_reads_still_release_held_input_at_its_deadline() {
    let mut reader = Reader::new();
    reader.read(&[b"\x1b]4;50;"]);
    reader.idle_until(Duration::from_millis(600));
    reader.read(&[b"a\x1b"]);
    reader.idle_until(Duration::from_millis(700));
    // The filter takes `ESC _ G` and every payload byte out of the input.
    let mut at = Duration::from_millis(700);
    while at <= Duration::from_millis(2_200) {
        reader.set_clock(at);
        reader.read(&[]);
        at += Duration::from_micros(500);
    }

    let typed: Vec<u8> = reader
        .out
        .iter()
        .flat_map(|(_, chunk)| chunk.clone())
        .collect();
    assert_eq!(typed, b"a\x1b".to_vec(), "{:?}", reader.out);
    let left = reader.out.iter().map(|(at, _)| *at).max().unwrap();
    assert!(
        left <= Duration::from_millis(1_620),
        "held input left at {left:?}: {:?}",
        reader.out
    );
}

/// The native Windows red on e0e6d7e6 (seeds 0x0012_0002 case 404 and
/// 0x0012_0004 case 779): the pump flushes right after a key record that
/// has no event (a key-up), with no time passed. That is no evidence the
/// sequence ended: `ESC [ 3` must not be dropped as an incomplete prefix,
/// so `~` completes Delete (ADR-0013 D2).
#[test]
fn review_r4_windows_red_pump_flush_right_after_push_keeps_key_prefix() {
    let mut framer = RawInputFramer::default();
    assert!(framer.push(b"\x1b[3").is_empty());
    assert!(framer.flush_timeout().is_empty());
    let events = framer.push(b"~");
    assert!(
        matches!(&events[..], [RawInputEvent::Key(key)]
            if key.code == crossterm::event::KeyCode::Delete),
        "{events:?}"
    );
}

/// A pump byte framer under a virtual clock, with a colour query sent.
fn pump_framer_with_color_query() -> (RawInputByteFramer, Instant) {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    framer.set_pump_policy();
    framer.host_color_query_sent();
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

/// Found by the v3 property test (ADR-0013 D1), the generic form of codex
/// f5251-r4 finding 1: an OSC 4 reply whose `ESC ]` was dropped at its idle
/// timeout, its tail held, then a semantic key. The tail is the host's, not
/// the user's: the key does not turn it into typing, before or after it.
#[test]
fn review_r4_1b_color_reply_tail_held_across_a_semantic_key_is_not_typed() {
    let (mut framer, base) = pump_framer_with_color_query();
    let mut chunks = framer.push(b"\x1b]");
    chunks.extend(ticks(&mut framer, base, 0, 448));
    chunks.extend(framer.push(b"4;160;rgb:a2ae/e6"));
    chunks.extend(ticks(&mut framer, base, 448, 451));
    chunks.extend(framer.flush_interrupted());
    chunks.extend(framer.push(b"3"));
    chunks.extend(ticks(&mut framer, base, 451, 501));
    chunks.extend(framer.push(b"1/d43c\x07"));
    chunks.extend(ticks(&mut framer, base, 501, 3_000));

    assert!(chunks.is_empty(), "{chunks:?}");
    assert!(!framer.has_pending_input());
}

/// Found by the v3 property test: a colour reply that reaches the front of
/// the buffer only after an orphan UTF-8 continuation in front of it was
/// dropped is not abandoned on arrival. Interrupted 582 ms later, inside its
/// one-second idle limit, it is kept, and its tail completes it.
#[test]
fn review_r4_1c_color_reply_behind_dropped_orphan_bytes_is_not_abandoned() {
    let (mut framer, base) = pump_framer_with_color_query();
    let mut chunks = framer.push(b"\xbd\xa0\x1b]10;rgb:ac06/");
    chunks.extend(ticks(&mut framer, base, 0, 582));
    chunks.extend(framer.flush_interrupted());
    chunks.extend(framer.push(b"ae01/39da\x07"));
    chunks.extend(ticks(&mut framer, base, 582, 3_000));

    let typed: Vec<u8> = chunks
        .iter()
        .filter(|chunk| !chunk.starts_with(b"\x1b]"))
        .flatten()
        .copied()
        .collect();
    assert!(typed.is_empty(), "{chunks:?}");
}
