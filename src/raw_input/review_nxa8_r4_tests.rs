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
