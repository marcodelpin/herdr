use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Parse raw terminal input bytes into a list of `RawInputEvent`s.
///
/// This directly extracts events without going through a channel, making it
/// suitable for synchronous use.
#[cfg(test)]
pub fn parse_raw_input_bytes_sync(data: &[u8]) -> Vec<RawInputEvent> {
    let mut framer = RawInputFramer::default();
    let mut events = framer.push(data);
    events.extend(framer.flush_timeout());
    events
}

/// Decode bytes the byte framer already split into complete input. This applies
/// no timing policy of its own: the reader decided where each event ends.
#[cfg(any(unix, test))]
pub(crate) fn parse_framed_input(data: &[u8]) -> Vec<RawInputEvent> {
    let mut events = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some((event, consumed)) = extract_one_event(rest) else {
            // A lone ESC or Alt introducer the reader released as a key.
            events.extend(events_from_framed_chunks(vec![rest.to_vec()]));
            break;
        };
        events.push(event);
        rest = &rest[consumed..];
    }
    events
}

use crate::input::{parse_terminal_key_sequence, TerminalKey, TextCommit};
use crate::terminal_theme::{
    parse_default_color_response, parse_palette_color_response, DefaultColorKind, HostAppearance,
    RgbColor,
};

const ESC: u8 = 0x1b;
pub(crate) const RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS: i32 = 10;
#[cfg(any(unix, test))]
pub(crate) const MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS: i32 = 150;
/// Covers the 33 ms split in #4630 without gluing legacy Alt+[ to the next key.
#[cfg(any(unix, test))]
pub(crate) const MOUSE_ACTIVE_CSI_INTRODUCER_FLUSH_TIMEOUT_MS: i32 = 50;
/// Covers the 350 ms mouse tail delay in #3480. Other input ends it early (#4751).
#[cfg(unix)]
const DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS: i32 = 500;
/// Bytes already inside a sequence cannot be a key, so wait long enough for a
/// split read over a slow link before giving up on them.
pub(crate) const INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS: i32 = 500;
pub(crate) const GHOSTTY_COLOR_SCHEME_DARK_REPORT: &[u8] = b"\x1b[?997;1n";
pub(crate) const GHOSTTY_COLOR_SCHEME_LIGHT_REPORT: &[u8] = b"\x1b[?997;2n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Returns the UTF-8 payload when `data` is exactly one complete bracketed paste.
pub(crate) fn complete_text_bracketed_paste(data: &[u8]) -> Option<&str> {
    if !data.starts_with(BRACKETED_PASTE_START) {
        return None;
    }
    let end = find_subsequence(data, BRACKETED_PASTE_END)?;
    if end + BRACKETED_PASTE_END.len() != data.len() {
        return None;
    }
    std::str::from_utf8(&data[BRACKETED_PASTE_START.len()..end]).ok()
}

/// Client transport uses this to distinguish recoverable oversized interactive
/// pastes from generic oversized input, which remains a protocol violation.
pub(crate) fn is_complete_text_bracketed_paste(data: &[u8]) -> bool {
    complete_text_bracketed_paste(data).is_some()
}

#[derive(Debug)]
pub enum RawInputEvent {
    Key(TerminalKey),
    Text(TextCommit),
    Paste(String),
    Mouse(MouseEvent),
    OuterFocusGained,
    OuterFocusLost,
    HostDefaultColor {
        kind: DefaultColorKind,
        color: RgbColor,
    },
    HostPaletteColors {
        colors: Vec<(u8, RgbColor)>,
    },
    HostColorSchemeChanged(HostAppearance),
    // The dimensions are only read by the Unix client.
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    HostCellSizeReport {
        width_px: u32,
        height_px: u32,
    },
    Unsupported,
}

/// Byte framer plus event decoding, for the Windows input pump. Its byte
/// framer runs the pump policy (ADR-0013 D2, D4).
#[cfg(any(windows, test))]
pub(crate) struct RawInputFramer {
    byte_framer: RawInputByteFramer,
}

#[cfg(any(windows, test))]
impl Default for RawInputFramer {
    fn default() -> Self {
        Self::with_byte_framer(RawInputByteFramer::default())
    }
}

#[cfg(any(windows, test))]
impl RawInputFramer {
    pub(crate) fn for_host_input() -> Self {
        Self::with_byte_framer(RawInputByteFramer::for_host_input())
    }

    fn with_byte_framer(mut byte_framer: RawInputByteFramer) -> Self {
        byte_framer.set_pump_policy();
        Self { byte_framer }
    }

    /// Moves this framer's clock `by` into the future, as the pump's console
    /// wait does between idle ticks (test translator only).
    #[cfg(test)]
    pub(crate) fn advance_test_clock(&mut self, by: Duration) {
        self.byte_framer.test_clock_skew += by;
    }

    /// One pump idle tick: the console wait passed with no record, then the
    /// pump flushes (ADR-0013 D2).
    #[cfg(test)]
    pub(crate) fn idle_tick(&mut self) -> Vec<RawInputEvent> {
        self.advance_test_clock(PUMP_IDLE_QUIET);
        self.flush_timeout()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn host_default_color_query_sent(&mut self, escape_grace: Duration) {
        self.byte_framer.host_default_color_query_sent(escape_grace);
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<RawInputEvent> {
        events_from_framed_chunks(self.byte_framer.push(data))
    }

    #[cfg(any(windows, test))]
    pub(crate) fn holds_host_default_color_escape(&self) -> bool {
        self.byte_framer.buffer.as_slice() == [ESC]
            && self.byte_framer.host_color_replies_awaited > 0
            && self
                .byte_framer
                .host_default_color_query_deadline
                .is_some_and(|deadline| self.byte_framer.now() < deadline)
    }

    pub(crate) fn has_pending_bracketed_paste(&self) -> bool {
        self.byte_framer.has_pending_bracketed_paste()
    }

    /// An X10 mouse report is in progress: `ESC [ M` and fewer than three
    /// report bytes are pending, or the context of a released `ESC` / `ESC [`
    /// holds the rest of one (ADR-0013 D4). The framer's context answers,
    /// not the buffer's shape alone.
    pub(crate) fn has_pending_default_mouse_sequence(&self) -> bool {
        starts_with_incomplete_default_mouse_sequence(&self.byte_framer.buffer)
            || self.byte_framer.holds_x10_tail()
    }

    pub(crate) fn flush_timeout(&mut self) -> Vec<RawInputEvent> {
        events_from_framed_chunks(self.byte_framer.flush_timeout())
    }

    pub(crate) fn flush_keyboard_escape(&mut self) -> Vec<RawInputEvent> {
        if self.byte_framer.buffer.as_slice() == [ESC] {
            let escape = std::mem::take(&mut self.byte_framer.buffer);
            self.byte_framer
                .open_tail_hold(HoldAfter::Escape { color_reply: false });
            events_from_framed_chunks(vec![escape])
        } else {
            self.flush_timeout()
        }
    }

    /// Semantic input ends every report and key hold, unlike another idle
    /// interval: what was held is input and goes first. A host reply prefix
    /// inside its reply window stays (ADR-0013 D1).
    pub(crate) fn flush_interrupted(&mut self) -> Vec<RawInputEvent> {
        events_from_framed_chunks(self.byte_framer.flush_interrupted())
    }
}

/// Events for chunks the byte framer released as complete input.
fn events_from_framed_chunks(chunks: Vec<Vec<u8>>) -> Vec<RawInputEvent> {
    chunks
        .into_iter()
        .filter_map(|chunk| {
            if chunk.as_slice() == [ESC] {
                return Some(RawInputEvent::Key(
                    TerminalKey::new(crossterm::event::KeyCode::Esc, KeyModifiers::empty())
                        .with_vt_bytes(chunk),
                ));
            }
            // The byte framer released this introducer as a key on its own.
            if is_alt_key_introducer(&chunk) {
                let text = std::str::from_utf8(&chunk).ok()?;
                return parse_terminal_key_sequence(text)
                    .map(|key| RawInputEvent::Key(key.with_vt_bytes(chunk.clone())));
            }
            extract_one_event(&chunk).map(|(event, _consumed)| {
                tracing::debug!(raw_bytes = ?chunk, event = ?event, "raw input event parsed");
                event
            })
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct RawInputByteFramer {
    /// Every byte neither emitted nor dropped yet, and only those (ADR-0010).
    /// They are always the last `buffer.len()` bytes pushed (ADR-0012).
    buffer: Vec<u8>,
    /// Bytes pushed so far.
    pushed: u64,
    /// When each read arrived: (end offset in the pushed stream, time), for
    /// the reads that may still have bytes in `buffer`.
    arrivals: VecDeque<(u64, Instant)>,
    discard_until: Option<ControlStringFamily>,
    discarded_tail_bytes: usize,
    /// The one context over `buffer`: what was disposed of before it, so
    /// what the bytes after it may still complete.
    tail_hold: Option<TailHold>,
    #[cfg(test)]
    test_clock: Option<Instant>,
    /// Added to the clock, fixed or real (test translator idle ticks).
    #[cfg(test)]
    test_clock_skew: Duration,
    /// The Windows pump drives this framer (ADR-0013 D2, D4).
    #[cfg(any(windows, test))]
    pump_policy: bool,
    host_color_replies_awaited: u16,
    #[cfg(any(windows, test))]
    host_default_color_query_issued: bool,
    #[cfg(any(windows, test))]
    host_default_color_query_deadline: Option<Instant>,
    #[cfg(any(windows, test))]
    host_color_tail_deadline: Option<Instant>,
    #[cfg(any(windows, test))]
    host_color_tail_split_st: bool,
    host_cell_size_replies_awaited: u16,
    host_appearance_reply_awaited: bool,
    /// Last query sent or reply received. Hosts may answer only part of a
    /// query (or none of it), so the reply counters alone never expire.
    host_reply_activity_at: Option<Instant>,
    /// Last read that left an unfinished OSC 10/11 reply buffered.
    partial_host_color_reply_read_at: Option<Instant>,
    held_pending_host_reply_esc: bool,
    host_color_scheme_change_tracking: bool,
    host_appearance_query_on_focus: bool,
    split_coalesced_escape: bool,
    #[cfg(unix)]
    host_escape_disambiguation_active: bool,
}

/// Bytes disposed of just before `buffer` without their tail, or (Unix) a
/// prefix still in `buffer` that waits for one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HoldAfter {
    /// A lone ESC was released as Escape. `[...` after it can be the rest of
    /// a mouse report or a CSI key (herdr-nxa8); with a colour query
    /// outstanding, `]10;` / `]11;` / `]4;` through BEL or ST is the rest of
    /// that reply.
    Escape { color_reply: bool },
    /// `ESC [` was released as Alt+[; what follows can be the rest of a mouse
    /// report (#5251) or a CSI key (herdr-nxa8).
    CsiIntroducer,
    /// An incomplete `ESC [ <` report or `ESC [` key sequence was dropped at
    /// its idle timeout.
    DroppedPrefix(Vec<u8>),
    /// An incomplete OSC string was dropped at its idle timeout; colour reply
    /// payload through BEL or ST is its tail. `split_st` when the dropped
    /// bytes ended in the ESC of an ST, so a lone `\` completes it. `header`
    /// is what was dropped (at most the report budget), so an interruption
    /// can tell a colour reply from text (ADR-0013 D1).
    OscHeader { split_st: bool, header: Vec<u8> },
    /// A confirmed host sends Escape as `CSI 27u`, so the `ESC` / `ESC [` the
    /// first `len` buffered bytes hold may still get a delayed mouse tail (#3480).
    #[cfg(unix)]
    DisambiguatedPrefix { len: usize },
}

#[derive(Debug)]
struct TailHold {
    after: HoldAfter,
    /// When the context opened. Its deadline is derived, never stored: see
    /// `tail_hold_deadline`.
    opened_at: Instant,
    /// A semantic key came while it held a colour reply's tail: from then on
    /// its reply-shaped bytes complete the reply or are dropped, never typed
    /// after that key (ADR-0013 D1, I4').
    interrupted: bool,
}

/// How a release treats bytes that could still wait (ADR-0013 D1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// A reader waited idle: bytes may stay held, each within its named bound.
    Idle,
    /// The pump says it is idle; that is not evidence the newest byte is
    /// quiet. As `Idle`, but a lossy disposal waits until the newest
    /// pending byte is `PUMP_IDLE_QUIET` old (ADR-0013 D2).
    PumpIdle,
    /// Semantic input: report and key holds end. A host reply prefix inside
    /// its reply window is not input and stays.
    Interrupt,
    /// A byte past its last bound: nothing stays held.
    Backstop,
}

impl Release {
    fn idle(self) -> bool {
        matches!(self, Release::Idle | Release::PumpIdle)
    }

    fn interrupt(self) -> bool {
        !self.idle()
    }
}

/// What the bytes in `buffer` are, given what was disposed of before them.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// They cannot complete it: frame them as ordinary input.
    Emit,
    /// The first `n` complete the disposed report: drop them.
    Drop(usize),
    /// The first `n` complete a reply (or, on the pump, an X10 report) whose
    /// prefix was released: put the prefix back so the whole parses as one.
    Rejoin(usize),
    /// They can still complete it: wait, until the deadline.
    Hold,
}

impl HoldAfter {
    /// How many leading bytes of `tail` still spell a colour reply after
    /// what this context dropped or released (0 for key and report contexts).
    fn reply_shaped_len(&self, tail: &[u8]) -> usize {
        let before: &[u8] = match self {
            HoldAfter::OscHeader { header, .. } => header,
            HoldAfter::Escape { color_reply: true } => &[ESC],
            _ => return 0,
        };
        let mut reply = before.to_vec();
        let mut len = 0;
        for byte in tail.iter().take(MAX_DISCARDED_CONTROL_TAIL_BYTES) {
            reply.push(*byte);
            if !is_color_reply_shape_prefix(&reply) {
                break;
            }
            len += 1;
        }
        len
    }

    /// The bytes a released context sent to the pane as a key.
    fn released_prefix(&self) -> &'static [u8] {
        match self {
            HoldAfter::CsiIntroducer => b"\x1b[",
            _ => &[ESC],
        }
    }

    /// The length of an incomplete X10 report tail `tail` starts with after
    /// this released context (`[M..` after ESC, `M..` after `ESC [`).
    fn incomplete_x10_tail_len(&self, tail: &[u8]) -> Option<usize> {
        let marker: &[u8] = match self {
            HoldAfter::Escape { .. } => b"[M",
            HoldAfter::CsiIntroducer => b"M",
            _ => return None,
        };
        let report = tail.strip_prefix(marker)?;
        (report.len() < 3 && report.iter().all(|byte| *byte >= 0x20)).then_some(tail.len())
    }
}

impl HoldAfter {
    fn limit(&self) -> Duration {
        match self {
            HoldAfter::Escape { color_reply: true } | HoldAfter::OscHeader { .. } => {
                COLOR_REPLY_TAIL_LIMIT
            }
            HoldAfter::Escape { color_reply: false }
            | HoldAfter::CsiIntroducer
            | HoldAfter::DroppedPrefix(_) => REPORT_TAIL_LIMIT,
            #[cfg(unix)]
            HoldAfter::DisambiguatedPrefix { .. } => DISAMBIGUATED_MOUSE_TAIL_LIMIT,
        }
    }
}

/// Classifies the pending bytes `tail` against what was disposed of before
/// them. `burst` says every pending byte arrived within `ORPHAN_TAIL_BURST`
/// of the first; `pump` that the Windows pump drives the framer (ADR-0013
/// D4). Pure: the deadline is checked by the caller first.
fn classify_held_tail(after: &HoldAfter, tail: &[u8], burst: bool, pump: bool) -> Verdict {
    match after {
        HoldAfter::Escape { .. } if tail.first() == Some(&b'[') => {
            classify_report_continuation(&[ESC], tail, true, burst, pump)
        }
        HoldAfter::Escape { color_reply: true } if tail.first() == Some(&b']') => {
            match orphaned_color_reply_tail(tail) {
                OrphanedColorReply::Prefix => Verdict::Hold,
                OrphanedColorReply::Unrelated => Verdict::Emit,
                OrphanedColorReply::Header(header) => {
                    match classify_discarded_osc_tail(&tail[header..]) {
                        DiscardedOscTail::Pending => Verdict::Hold,
                        DiscardedOscTail::Terminated(len) => Verdict::Rejoin(header + len),
                        DiscardedOscTail::Foreign => Verdict::Emit,
                    }
                }
            }
        }
        HoldAfter::Escape { .. } => Verdict::Emit,
        HoldAfter::CsiIntroducer => classify_report_continuation(b"\x1b[", tail, true, burst, pump),
        HoldAfter::DroppedPrefix(prefix) => {
            classify_report_continuation(prefix, tail, false, burst, pump)
        }
        HoldAfter::OscHeader { split_st: true, .. } if tail.first() == Some(&b'\\') => {
            Verdict::Drop(1)
        }
        HoldAfter::OscHeader { .. } => match classify_discarded_osc_tail(tail) {
            DiscardedOscTail::Pending => Verdict::Hold,
            DiscardedOscTail::Terminated(len) => Verdict::Drop(len),
            DiscardedOscTail::Foreign => Verdict::Emit,
        },
        // Pushes end this hold before classification; see `push_extended`.
        #[cfg(unix)]
        HoldAfter::DisambiguatedPrefix { .. } => Verdict::Hold,
    }
}

/// Classifies `tail` as the rest of a report or key whose `prefix` (`ESC`,
/// `ESC [`, or more) was disposed of (ADR-0012 D3). `released` when that
/// prefix reached the pane as a key: a key or X10 tail after it is dropped
/// only when it arrived as one `burst`, since typing can spell one. An SGR
/// mouse tail cannot be typed by accident and is dropped however it came.
/// On the Windows `pump`, typed keys never reach the framer as bytes, so an
/// X10 tail after a released prefix is the report at any spread: it is put
/// back together, so the pump accepts the key records it staged for it
/// (ADR-0013 D4).
fn classify_report_continuation(
    prefix: &[u8],
    tail: &[u8],
    released: bool,
    burst: bool,
    pump: bool,
) -> Verdict {
    let mut report = prefix.to_vec();
    report.extend_from_slice(&tail[..tail.len().min(MAX_DISCARDED_CONTROL_TAIL_BYTES)]);
    if report.len() < 3 {
        return if b"\x1b[".starts_with(&report) {
            Verdict::Hold
        } else {
            Verdict::Emit
        };
    }
    if !report.starts_with(b"\x1b[") {
        return Verdict::Emit;
    }
    let complete = |len: usize| {
        if !released || burst {
            Verdict::Drop(len - prefix.len())
        } else {
            Verdict::Emit
        }
    };
    match report[2] {
        b'<' => match classify_sgr_mouse_continuation(prefix, tail) {
            SgrMouseContinuation::Incomplete => Verdict::Hold,
            SgrMouseContinuation::Complete(len) => Verdict::Drop(len),
            SgrMouseContinuation::Invalid => Verdict::Emit,
        },
        b'M' => {
            const X10_LEN: usize = 6;
            if report[3..]
                .iter()
                .take(X10_LEN - 3)
                .any(|byte| *byte < 0x20)
            {
                Verdict::Emit
            } else if report.len() < X10_LEN {
                Verdict::Hold
            } else if parse_default_mouse(&report[..X10_LEN]).is_some() {
                if released && pump {
                    Verdict::Rejoin(X10_LEN - prefix.len())
                } else {
                    complete(X10_LEN)
                }
            } else {
                Verdict::Emit
            }
        }
        _ => {
            // CSI: parameter and intermediate bytes, then one final byte.
            let Some(final_index) = report[2..]
                .iter()
                .position(|byte| !matches!(byte, 0x20..=0x3f))
                .map(|index| index + 2)
            else {
                return if report.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
                    Verdict::Emit
                } else {
                    Verdict::Hold
                };
            };
            if !matches!(report[final_index], 0x40..=0x7e) {
                return Verdict::Emit;
            }
            let len = final_index + 1;
            let is_key = matches!(
                extract_one_event(&report[..len]),
                Some((
                    RawInputEvent::Key(_)
                        | RawInputEvent::OuterFocusGained
                        | RawInputEvent::OuterFocusLost,
                    consumed
                )) if consumed == len
            );
            if is_key {
                complete(len)
            } else {
                Verdict::Emit
            }
        }
    }
}

const HOST_COLOR_QUERY_REPLIES: u16 = 258;
/// How long after the last query or reply Herdr still treats `ESC[` / `ESC]`
/// as a possible host reply instead of a key.
const HOST_REPLY_QUIET_WINDOW: Duration = Duration::from_secs(1);
/// How long an unfinished OSC 10/11 reply may wait for its next read before
/// it is dropped (#5052). Twice the split-read wait for any incomplete
/// sequence, and far above the tens of milliseconds seen between fragments
/// over SSH and tmux, yet short enough that a reply the link cut off cannot
/// hold input for long.
const HOST_COLOR_REPLY_IDLE_LIMIT: Duration = Duration::from_secs(1);
#[cfg(any(unix, test))]
const HOST_CELL_SIZE_QUERY_REPLIES: u16 = 1;
/// Bound of a report tail hold (after a released ESC or `ESC [`, or a dropped
/// `ESC [` prefix): the split-read wait for any incomplete sequence. It
/// bounds how late the tail may start, and how long a held byte waits from
/// its arrival.
const REPORT_TAIL_LIMIT: Duration =
    Duration::from_millis(INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS as u64);
/// Bound of a colour reply tail hold (after an ESC released while a colour
/// query was outstanding, or a dropped OSC header): the unfinished-reply limit.
const COLOR_REPLY_TAIL_LIMIT: Duration = HOST_COLOR_REPLY_IDLE_LIMIT;
/// Bound of the confirmed-disambiguation hold of `ESC` / `ESC [` (#3480).
#[cfg(unix)]
const DISAMBIGUATED_MOUSE_TAIL_LIMIT: Duration =
    Duration::from_millis(DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS as u64);
/// A key or X10 tail after a released prefix counts as the terminal's own
/// split write only when all of it arrived within this of its first byte:
/// the idle flush interval, which is what one read means to the reader.
/// Typing the same bytes takes one keystroke each, tens of ms apart.
const ORPHAN_TAIL_BURST: Duration = Duration::from_millis(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS as u64);
/// A pump idle is evidence that a sequence ended only once its newest byte
/// is this old: one idle flush interval (ADR-0013 D2).
const PUMP_IDLE_QUIET: Duration = Duration::from_millis(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS as u64);
/// Backstop for every pending byte except an unterminated bracketed paste:
/// past this age it is released as on an interruption (ADR-0012 D2). The
/// longest named hold plus one split-read wait, so no named hold reaches it.
const PENDING_INPUT_AGE_LIMIT: Duration = Duration::from_millis(
    COLOR_REPLY_TAIL_LIMIT.as_millis() as u64 + INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS as u64,
);

/// The one wait of the Unix stdin reader before it blocks in `read()`
/// (ADR-0012 D4): the earliest of the framer's release time
/// (`held_input_release_ms`) and, while the direct-graphics filter keeps
/// bytes, its idle poll. Returns the wait and whether the framer is due when
/// the wait ends idle; `None` means only input can change anything.
#[cfg(any(unix, test))]
pub(crate) fn reader_idle_wait(
    framer_release_ms: Option<i32>,
    direct_pending: bool,
) -> Option<(i32, bool)> {
    match (framer_release_ms, direct_pending) {
        (Some(framer), true) => Some((
            framer.min(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS),
            framer <= RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
        )),
        (Some(framer), false) => Some((framer, true)),
        (None, true) => Some((RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS, false)),
        (None, false) => None,
    }
}

/// The framer side of the Unix stdin reader loop, one turn at a time
/// (ADR-0013 D3). The reader and the property harness both run it, so an
/// empty filtered read takes the production path in tests too.
#[cfg(any(unix, test))]
#[derive(Debug, Default)]
pub(crate) struct ReaderSchedule {
    /// When the bytes the framer last received get their idle flush, or,
    /// after a flush that kept bytes, when the framer is next due.
    idle_flush_at: Option<Instant>,
}

#[cfg(any(unix, test))]
impl ReaderSchedule {
    /// The one wait before `read()`, rounded up to whole milliseconds, and
    /// whether the framer is due when it ends idle (ADR-0012 D4).
    pub(crate) fn wait(
        &self,
        framer: &RawInputByteFramer,
        direct_pending: bool,
    ) -> Option<(i32, bool)> {
        let now = framer.now();
        let idle = self
            .idle_flush_at
            .filter(|_| framer.has_pending_input())
            .map(|at| ceil_ms(at.saturating_duration_since(now)));
        let framer_wait = match (idle, framer.held_input_release_ms()) {
            (Some(idle), Some(release)) => Some(idle.min(release)),
            (idle, release) => idle.or(release),
        };
        reader_idle_wait(framer_wait, direct_pending)
    }

    /// A read woke the reader; `filtered` is what the direct-graphics filter
    /// let through, possibly nothing. Every wake runs the framer's due
    /// releases first. Only bytes the framer receives restart its idle
    /// interval; a wake that brought it none flushes it when that interval
    /// (or its release time) has passed (ADR-0013 D3).
    pub(crate) fn on_read(
        &mut self,
        framer: &mut RawInputByteFramer,
        filtered: Vec<Vec<u8>>,
        mouse_capture: bool,
    ) -> Vec<Vec<u8>> {
        let mut chunks = framer.release_due();
        let mut received = false;
        for chunk in filtered.into_iter().filter(|chunk| !chunk.is_empty()) {
            received = true;
            chunks.extend(framer.push(&chunk));
        }
        let now = framer.now();
        if received {
            self.arm_idle_flush(framer, mouse_capture);
        } else if framer.has_pending_input() && self.idle_flush_at.is_some_and(|at| now >= at) {
            chunks.extend(self.flush(framer));
        }
        chunks
    }

    /// The wait ended with no input. `released` is what the graphics filter
    /// let go because no response is awaited any more.
    pub(crate) fn on_idle(
        &mut self,
        framer: &mut RawInputByteFramer,
        released: Option<Vec<u8>>,
        framer_due: bool,
        mouse_capture: bool,
    ) -> Vec<Vec<u8>> {
        if let Some(data) = released {
            let chunks = framer.push(&data);
            self.arm_idle_flush(framer, mouse_capture);
            chunks
        } else if framer_due {
            self.flush(framer)
        } else {
            Vec::new()
        }
    }

    fn arm_idle_flush(&mut self, framer: &RawInputByteFramer, mouse_capture: bool) {
        self.idle_flush_at = Some(
            framer.now()
                + Duration::from_millis(framer.idle_flush_timeout_ms(mouse_capture) as u64),
        );
    }

    fn flush(&mut self, framer: &mut RawInputByteFramer) -> Vec<Vec<u8>> {
        let chunks = framer.flush_timeout();
        let now = framer.now();
        self.idle_flush_at = framer
            .held_input_release_ms()
            .map(|ms| now + Duration::from_millis(ms.max(0) as u64));
        chunks
    }
}

/// `duration` in whole milliseconds, rounded up, so a wait of it ends at or
/// after the instant it was measured to.
#[cfg(any(unix, test))]
fn ceil_ms(duration: Duration) -> i32 {
    i32::try_from(duration.as_micros().div_ceil(1_000)).unwrap_or(i32::MAX)
}

impl RawInputByteFramer {
    pub(crate) fn for_host_input() -> Self {
        Self::with_host_input_policy(
            crate::platform::capabilities().preserve_legacy_doubled_escape_input,
        )
    }

    pub(crate) fn with_host_input_policy(preserve_legacy_doubled_escape_input: bool) -> Self {
        Self {
            split_coalesced_escape: !preserve_legacy_doubled_escape_input,
            ..Self::default()
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        // Bounds are checked before the new bytes can be classified with the
        // old ones (ADR-0012 D2).
        let mut chunks = self.release_due();
        if data.is_empty() {
            chunks.extend(self.push_extended());
            return chunks;
        }
        // Bytes after the idle limit are never part of the abandoned reply.
        self.drop_abandoned_host_color_reply();
        self.buffer.extend_from_slice(data);
        self.record_arrival(data.len());
        chunks.extend(self.push_extended());
        let now = self.now();
        self.partial_host_color_reply_read_at =
            starts_with_incomplete_default_color_response(&self.buffer).then_some(now);
        chunks
    }

    fn push_extended(&mut self) -> Vec<Vec<u8>> {
        #[cfg(unix)]
        if let Some(HoldAfter::DisambiguatedPrefix { len }) =
            self.tail_hold.as_ref().map(|hold| hold.after.clone())
        {
            self.tail_hold = None;
            if !continues_escape_sequence(&self.buffer) {
                // The prefix already outlived keyboard timing; it was a key.
                let mut chunks = vec![self.buffer.drain(..len).collect()];
                if len == 1 {
                    // A released ESC: what follows it is classified as its
                    // possible orphan tail (ADR-0012 D3). `ESC [` keeps the
                    // binding policy of #4751: the next key is typed.
                    self.open_tail_hold(HoldAfter::Escape { color_reply: false });
                }
                chunks.extend(self.drain_available_chunks());
                return chunks;
            }
        }
        self.drain_available_chunks()
    }

    /// Runs every release that time alone has made due: a byte past the
    /// backstop is released as past its last bound, an expired hold as if the
    /// reader had flushed at its deadline. `push` and every reader wake run
    /// it, whatever else they do (ADR-0013 D3).
    pub(crate) fn release_due(&mut self) -> Vec<Vec<u8>> {
        if self.pending_input_expired() {
            self.release_everything(Release::Backstop)
        } else if self.tail_hold_expired() {
            self.flush_timeout()
        } else {
            Vec::new()
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.test_clock {
            return now + self.test_clock_skew;
        }
        #[cfg(test)]
        return Instant::now() + self.test_clock_skew;
        #[cfg(not(test))]
        Instant::now()
    }

    #[cfg(any(windows, test))]
    fn set_pump_policy(&mut self) {
        self.pump_policy = true;
    }

    fn pump_policy(&self) -> bool {
        #[cfg(any(windows, test))]
        return self.pump_policy;
        #[cfg(not(any(windows, test)))]
        false
    }

    /// Whether the newest pending byte arrived at least `PUMP_IDLE_QUIET`
    /// ago: only then is a pump idle evidence that no more of it is coming.
    fn newest_pending_quiet(&self) -> bool {
        let now = self.now();
        self.buffer.is_empty()
            || self
                .arrivals
                .back()
                .is_none_or(|(_, at)| now.saturating_duration_since(*at) >= PUMP_IDLE_QUIET)
    }

    /// The open context of a released `ESC` / `ESC [` holds an incomplete X10
    /// report tail (ADR-0013 D4).
    #[cfg(any(windows, test))]
    fn holds_x10_tail(&self) -> bool {
        self.tail_hold
            .as_ref()
            .is_some_and(|hold| hold.after.incomplete_x10_tail_len(&self.buffer).is_some())
            && !self.tail_hold_expired()
    }

    /// Bytes an interruption leaves pending: a colour reply prefix while its
    /// reply is still awaited (the Windows default-colour window, or the
    /// host reply window of a colour query). They can only complete the
    /// reply or be dropped under its own bound, never reach the pane as
    /// input (ADR-0013 D1).
    fn interruption_keeps_pending(&self) -> bool {
        if self.buffer.len() > MAX_DISCARDED_CONTROL_TAIL_BYTES {
            return false;
        }
        if self.interruption_keeps_tail_hold() {
            return true;
        }
        #[cfg(any(windows, test))]
        if self.host_default_color_query_issued
            && self
                .host_default_color_query_deadline
                .is_some_and(|deadline| self.now() < deadline)
            && is_incomplete_host_default_color_prefix(&self.buffer)
        {
            return true;
        }
        self.host_color_replies_awaited > 0
            && self.awaiting_host_reply()
            && is_color_reply_shape_prefix(&self.buffer)
            && !(starts_with_incomplete_default_color_response(&self.buffer)
                && self.host_color_reply_abandoned())
    }

    /// An open colour reply tail context an interruption leaves open: the
    /// header of an OSC reply was dropped, or the ESC of an awaited colour
    /// reply went out as Escape, and what is held can still complete that
    /// reply. A semantic key is the user's, not the host's: it does not end
    /// what the host is sending, so the tail is still dropped if it completes
    /// the reply, and framed as input only at its deadline or when it cannot
    /// (ADR-0013 D1). Key and report contexts still end (#4048).
    fn interruption_keeps_tail_hold(&self) -> bool {
        let Some(hold) = &self.tail_hold else {
            return false;
        };
        // The bytes decide: what was dropped or released before the held
        // tail, and the tail, must still spell the start of a colour reply.
        let reply_so_far = match &hold.after {
            // Only the `\` of its ST is missing.
            HoldAfter::OscHeader { split_st: true, .. } if self.buffer.is_empty() => {
                return !self.tail_hold_expired();
            }
            HoldAfter::OscHeader { header, .. } => [header.as_slice(), &self.buffer].concat(),
            HoldAfter::Escape { color_reply: true } if self.buffer.first() == Some(&b']') => {
                [&[ESC][..], &self.buffer].concat()
            }
            _ => return false,
        };
        is_color_reply_shape_prefix(&reply_so_far)
            && self.buffer.len() <= MAX_DISCARDED_CONTROL_TAIL_BYTES
            && !self.tail_hold_expired()
            && (self.buffer.is_empty()
                || classify_held_tail(&hold.after, &self.buffer, false, self.pump_policy())
                    == Verdict::Hold)
    }

    /// Freezes this framer's clock at `now`, for deterministic tests.
    #[cfg(test)]
    pub(crate) fn set_test_clock(&mut self, now: Instant) {
        self.test_clock = Some(now);
    }

    /// Logs when the `len` bytes just appended to `buffer` arrived.
    fn record_arrival(&mut self, len: usize) {
        let now = self.now();
        self.pushed += len as u64;
        // Reads with no byte left in `buffer` are never looked up again.
        let oldest = self.pushed.saturating_sub(self.buffer.len() as u64);
        while self.arrivals.front().is_some_and(|(end, _)| *end <= oldest) {
            self.arrivals.pop_front();
        }
        self.arrivals.push_back((self.pushed, now));
    }

    /// When the oldest pending byte arrived. Pending bytes are the last
    /// `buffer.len()` bytes pushed, so this is a lookup in `arrivals`.
    fn pending_arrival(&self) -> Option<Instant> {
        if self.buffer.is_empty() {
            return None;
        }
        let oldest = self.pushed.saturating_sub(self.buffer.len() as u64);
        self.arrivals
            .iter()
            .find(|(end, _)| *end > oldest)
            .map(|(_, at)| *at)
    }

    /// An unterminated bracketed paste, the one pending input without a
    /// time bound (ADR-0012 Residuals).
    fn waiting_for_paste_terminator(&self) -> bool {
        self.buffer.starts_with(BRACKETED_PASTE_START)
            && find_subsequence(&self.buffer, BRACKETED_PASTE_END).is_none()
    }

    /// When the backstop releases the oldest pending byte.
    fn pending_input_deadline(&self) -> Option<Instant> {
        if self.waiting_for_paste_terminator() {
            return None;
        }
        self.pending_arrival()
            .map(|at| at + PENDING_INPUT_AGE_LIMIT)
    }

    fn pending_input_expired(&self) -> bool {
        let now = self.now();
        self.pending_input_deadline()
            .is_some_and(|deadline| now >= deadline)
    }

    /// The hold's deadline, derived from arrivals (ADR-0012 D2): the oldest
    /// held byte's arrival plus the context's limit, or, while nothing is
    /// held yet, the context's opening plus that limit (how late the tail may
    /// start).
    fn tail_hold_deadline(&self) -> Option<Instant> {
        let hold = self.tail_hold.as_ref()?;
        let limit = hold.after.limit();
        #[cfg(unix)]
        if matches!(hold.after, HoldAfter::DisambiguatedPrefix { .. }) {
            // Its bytes are the prefix itself; the wait starts at the flush.
            return Some(hold.opened_at + limit);
        }
        let from = self
            .pending_arrival()
            .map_or(hold.opened_at, |at| at.max(hold.opened_at));
        Some(from + limit)
    }

    fn tail_hold_expired(&self) -> bool {
        let now = self.now();
        self.tail_hold_deadline()
            .is_some_and(|deadline| now >= deadline)
    }

    /// Opens the hold for the tail of what was just disposed of.
    fn open_tail_hold(&mut self, after: HoldAfter) {
        self.tail_hold = Some(TailHold {
            after,
            opened_at: self.now(),
            interrupted: false,
        });
    }

    /// Opens the hold only on an idle release: an interruption leaves no
    /// context behind (ADR-0012 D1).
    fn open_tail_hold_on(&mut self, mode: Release, after: HoldAfter) {
        if mode.idle() {
            self.open_tail_hold(after);
        }
    }

    /// Hold a lone trailing ESC for one idle flush so an OSC 10/11 reply split
    /// at its ESC introducer stitches back together instead of leaking (#549).
    /// Overlapping queries each add their own replies to the window.
    pub(crate) fn host_color_query_sent(&mut self) {
        self.host_color_query_sent_with_replies(HOST_COLOR_QUERY_REPLIES);
    }

    #[cfg(any(windows, test))]
    pub(crate) fn host_default_color_query_sent(&mut self, reply_window: Duration) {
        self.host_color_query_sent_with_replies(2);
        self.host_default_color_query_issued = true;
        self.host_default_color_query_deadline = Some(self.now() + reply_window);
    }

    fn host_color_query_sent_with_replies(&mut self, replies: u16) {
        self.host_color_replies_awaited = self.host_color_replies_awaited.saturating_add(replies);
        self.held_pending_host_reply_esc = false;
        self.host_reply_activity_at = Some(self.now());
    }

    /// Whether a buffered unfinished OSC 10/11 reply has waited past its idle
    /// limit or grown past any real reply, so it can no longer hold input.
    fn host_color_reply_abandoned(&self) -> bool {
        // The Windows default color query keeps its own reply deadline.
        #[cfg(any(windows, test))]
        if self.host_default_color_query_deadline.is_some() {
            return false;
        }
        // A reply that came to the front of `buffer` only after the bytes
        // before it in its read were dropped (an orphan UTF-8 continuation)
        // has no read time of its own: its newest byte's arrival is that time
        // (ADR-0013 D1).
        let now = self.now();
        let last_read = self
            .partial_host_color_reply_read_at
            .or_else(|| self.arrivals.back().map(|(_, at)| *at));
        self.buffer.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES
            || last_read
                .is_none_or(|at| now.saturating_duration_since(at) >= HOST_COLOR_REPLY_IDLE_LIMIT)
    }

    fn drop_abandoned_host_color_reply(&mut self) {
        if self.partial_host_color_reply_read_at.is_none()
            || !starts_with_incomplete_default_color_response(&self.buffer)
            || !self.host_color_reply_abandoned()
        {
            return;
        }
        tracing::debug!(
            len = self.buffer.len(),
            "dropping unfinished host color response before new input"
        );
        self.buffer.clear();
        self.partial_host_color_reply_read_at = None;
    }

    fn host_appearance_query_sent(&mut self) {
        self.host_appearance_reply_awaited = true;
        self.held_pending_host_reply_esc = false;
        self.host_reply_activity_at = Some(self.now());
    }

    /// Same hold window as `host_color_query_sent`, for the XTWINOPS cell size
    /// reply. Only the Unix client sends this query.
    #[cfg(any(unix, test))]
    pub(crate) fn host_cell_size_query_sent(&mut self) {
        self.host_cell_size_replies_awaited = HOST_CELL_SIZE_QUERY_REPLIES;
        self.held_pending_host_reply_esc = false;
        self.host_reply_activity_at = Some(self.now());
    }

    fn awaiting_host_reply(&self) -> bool {
        let now = self.now();
        (self.host_color_replies_awaited > 0
            || self.host_cell_size_replies_awaited > 0
            || self.host_appearance_reply_awaited)
            && self
                .host_reply_activity_at
                .is_some_and(|at| now.saturating_duration_since(at) < HOST_REPLY_QUIET_WINDOW)
    }

    /// Stop expecting replies the host never sent, so their reply handling no
    /// longer holds or discards what is now ordinary input.
    fn expire_unanswered_host_queries(&mut self) {
        if self.awaiting_host_reply() || self.host_reply_activity_at.is_none() {
            return;
        }
        // The Windows default color query keeps its own reply deadline.
        #[cfg(any(windows, test))]
        if self.host_default_color_query_deadline.is_some() {
            return;
        }
        self.host_color_replies_awaited = 0;
        self.host_cell_size_replies_awaited = 0;
        self.host_appearance_reply_awaited = false;
        self.held_pending_host_reply_esc = false;
        self.host_reply_activity_at = None;
    }

    #[cfg(test)]
    fn age_partial_host_color_reply(&mut self, by: Duration) {
        self.partial_host_color_reply_read_at = self
            .partial_host_color_reply_read_at
            .and_then(|at| at.checked_sub(by));
    }

    /// Moves every time-bounded hold `by` into the past: the hold's opening
    /// and the arrival of every pending byte.
    #[cfg(test)]
    fn age_held_input(&mut self, by: Duration) {
        if let Some(hold) = &mut self.tail_hold {
            hold.opened_at = hold.opened_at.checked_sub(by).unwrap_or(hold.opened_at);
        }
        for (_, at) in &mut self.arrivals {
            *at = at.checked_sub(by).unwrap_or(*at);
        }
    }

    #[cfg(test)]
    fn age_host_reply_activity(&mut self, by: Duration) {
        self.host_reply_activity_at = self
            .host_reply_activity_at
            .and_then(|at| at.checked_sub(by));
    }

    #[cfg(any(unix, test))]
    pub(crate) fn enable_host_color_scheme_change_tracking(&mut self) {
        self.host_color_scheme_change_tracking = true;
    }

    /// Arm the bounded host-reply window when focus gain will emit an appearance query.
    /// If the write or reply fails, a lone Escape is delayed for only one extra flush.
    #[cfg(any(not(windows), test))]
    pub(crate) fn enable_host_appearance_query_on_focus(&mut self) {
        self.host_appearance_query_on_focus = true;
    }

    /// Every held byte is in `buffer`, so this is all the input not yet
    /// emitted or dropped.
    #[cfg(any(unix, test))]
    pub(crate) fn has_pending_input(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Milliseconds until `flush_timeout` must run even if no further input
    /// arrives: the hold's deadline while it holds bytes, or one idle interval
    /// while a lone ESC (or `ESC [`) is held one flush for a host reply. `None`
    /// means only more input can change what is pending.
    #[cfg(any(unix, test))]
    pub(crate) fn held_input_release_ms(&self) -> Option<i32> {
        if self.buffer.is_empty() {
            return None;
        }
        let now = self.now();
        let until = |deadline: Instant| {
            // Round up so the deadline has passed when the wait ends.
            i32::try_from(deadline.saturating_duration_since(now).as_millis())
                .unwrap_or(i32::MAX)
                .saturating_add(1)
        };
        [
            self.tail_hold_deadline().map(until),
            self.holds_reply_prefix_one_flush()
                .then_some(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS),
            self.pending_input_deadline().map(until),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// A lone ESC or `ESC [` held for exactly one more flush while a host
    /// reply is awaited (see `flush_timeout`).
    #[cfg(any(unix, test))]
    fn holds_reply_prefix_one_flush(&self) -> bool {
        self.held_pending_host_reply_esc
            && (matches!(self.buffer.as_slice(), [ESC] | [ESC, b'['])
                || starts_with_incomplete_host_color_scheme_report(&self.buffer))
    }

    #[cfg(unix)]
    pub(crate) fn set_host_escape_disambiguation_active(&mut self, active: bool) {
        self.host_escape_disambiguation_active = active;
    }

    /// Pending bytes that could still be a key on their own: a lone ESC or ESC
    /// plus one introducer. Legacy hosts send those for Escape and Alt+key, and
    /// host text bindings (e.g. Ghostty `text:\x1bO`) send them even when the
    /// host disambiguates escapes.
    #[cfg(any(unix, test))]
    fn pending_could_be_escape_key(&self) -> bool {
        is_escape_key_prefix(&self.buffer)
    }

    /// How long to wait for more input before `flush_timeout`. Only bytes that
    /// could still be a key get the short keyboard window; anything already
    /// inside a sequence waits long enough for split reads over slow links.
    #[cfg(any(unix, test))]
    pub(crate) fn idle_flush_timeout_ms(&self, host_mouse_capture_active: bool) -> i32 {
        if self.buffer.is_empty() {
            return RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS;
        }
        // A reply Herdr asked for can split right after its introducer.
        if self.awaiting_host_reply() && matches!(self.buffer.as_slice(), b"\x1b[" | b"\x1b]") {
            return INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS;
        }
        if self.pending_could_be_escape_key() {
            if !host_mouse_capture_active {
                return RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS;
            }
            return match self.buffer.as_slice() {
                [ESC] => MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS,
                // A mouse report split after ESC[ is still ambiguous with legacy Alt+[.
                b"\x1b[" => MOUSE_ACTIVE_CSI_INTRODUCER_FLUSH_TIMEOUT_MS,
                _ => RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
            };
        }
        if self.buffer.first() == Some(&ESC) || starts_with_incomplete_utf8_char(&self.buffer) {
            return INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS;
        }
        RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
    }

    /// How long to keep holding input after a first idle flush held it.
    #[cfg(unix)]
    pub(crate) fn held_input_flush_timeout_ms(&self) -> i32 {
        self.held_input_release_ms()
            .unwrap_or(RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
    }

    /// Ends every report and key hold at once, for semantic input (Windows
    /// key, focus and mouse records): what was held is input and is emitted,
    /// in order, before the caller emits that event. A host reply prefix
    /// inside its reply window is not input and stays (ADR-0013 D1, I4').
    #[cfg(any(windows, test))]
    pub(crate) fn flush_interrupted(&mut self) -> Vec<Vec<u8>> {
        self.release_everything(Release::Interrupt)
    }

    /// Repeats an interrupting release until nothing is pending but an
    /// unterminated bracketed paste or, on an interruption, a reply prefix
    /// inside its window. Opens no context.
    fn release_everything(&mut self, mode: Release) -> Vec<Vec<u8>> {
        debug_assert!(mode.interrupt());
        let mut chunks = Vec::new();
        // Each pass emits or drops at least the front of `buffer`; the bound
        // only guards against a rule that would not.
        for _ in 0..=MAX_DISCARDED_CONTROL_TAIL_BYTES {
            chunks.extend(self.release(mode));
            if self.buffer.is_empty()
                || self.waiting_for_paste_terminator()
                || (mode == Release::Interrupt && self.interruption_keeps_pending())
            {
                break;
            }
        }
        chunks
    }

    #[cfg(any(windows, test))]
    pub(crate) fn has_pending_bracketed_paste(&self) -> bool {
        self.waiting_for_paste_terminator()
    }

    /// The idle release: the reader saw no input for its idle wait (on the
    /// pump: the pump says so, see `Release::PumpIdle`). A byte older than
    /// the backstop is released as past its last bound first.
    pub(crate) fn flush_timeout(&mut self) -> Vec<Vec<u8>> {
        if self.pending_input_expired() {
            tracing::debug!(
                len = self.buffer.len(),
                "releasing pending input past its age limit"
            );
            return self.release_everything(Release::Backstop);
        }
        self.release(if self.pump_policy() {
            Release::PumpIdle
        } else {
            Release::Idle
        })
    }

    fn release(&mut self, mode: Release) -> Vec<Vec<u8>> {
        let interrupt = mode.interrupt();
        // A pump idle right after a push is no evidence the sequence ended:
        // what would drop bytes waits for one quiet interval (ADR-0013 D2).
        let defer_loss = mode == Release::PumpIdle && !self.newest_pending_quiet();
        // The deadline is checked before anything is classified. An expired
        // hold's bytes are then framed as ordinary input, in order. An
        // interruption ends the hold whatever its deadline.
        let expired_hold =
            if (interrupt && !self.interruption_keeps_tail_hold()) || self.tail_hold_expired() {
                self.tail_hold.take()
            } else {
                None
            };
        // A colour reply's tail context survives an interruption (ADR-0013 D1).
        let kept_hold = interrupt && self.tail_hold.is_some();
        if let Some(hold) = self.tail_hold.as_mut().filter(|_| kept_hold) {
            hold.interrupted = true;
        }
        // On the pump an X10 report tail whose prefix went out as a key is
        // the report, and the pump replays the key records it staged for it:
        // unfinished, it is dropped like an unfinished `ESC [ M` (ADR-0013 D4).
        if let Some(len) = expired_hold
            .as_ref()
            .filter(|_| self.pump_policy())
            .and_then(|hold| hold.after.incomplete_x10_tail_len(&self.buffer))
        {
            tracing::debug!(len, "dropping unfinished X10 report tail");
            self.buffer.drain(..len);
        }
        // What a kept colour reply context held is not input (ADR-0013 D1).
        if let Some(hold) = expired_hold.as_ref().filter(|hold| hold.interrupted) {
            let len = hold.after.reply_shaped_len(&self.buffer);
            tracing::debug!(len, "dropping unfinished colour reply tail at its deadline");
            self.buffer.drain(..len);
        }
        if let Some(hold) = &expired_hold {
            tracing::debug!(
                after = ?hold.after,
                len = self.buffer.len(),
                "releasing held input at its deadline"
            );
        }
        // A finished mouse wait also counts as this prefix's one-flush host
        // reply hold, so the two holds never stack.
        // An interruption serves every one-flush hold as well.
        #[cfg(unix)]
        let mouse_wait_served = interrupt
            || matches!(
                expired_hold.map(|hold| hold.after),
                Some(HoldAfter::DisambiguatedPrefix { .. })
            );
        #[cfg(not(unix))]
        let mouse_wait_served = {
            let _ = expired_hold;
            interrupt
        };
        // Colour-tail recovery that holds bytes ends as if its grace had
        // passed: they are framed as input. Recovery that holds nothing
        // survives, so a reply tail after the semantic event is still dropped.
        #[cfg(any(windows, test))]
        if interrupt && self.host_color_tail_deadline.is_some() && !self.buffer.is_empty() {
            self.end_host_color_tail_recovery();
        }
        let mut chunks = self.drain_available_chunks();
        if interrupt && !kept_hold {
            // A hold opened while draining belongs to bytes already framed.
            self.tail_hold = None;
        }
        self.expire_unanswered_host_queries();

        #[cfg(any(windows, test))]
        let host_color_query_expired = self
            .host_default_color_query_deadline
            .is_some_and(|deadline| self.now() >= deadline)
            || (interrupt && self.buffer.as_slice() == [ESC]);
        #[cfg(any(windows, test))]
        if host_color_query_expired {
            self.host_default_color_query_deadline = None;
            self.host_color_replies_awaited = 0;
        }

        // Idle is not evidence that a held tail has ended: it waits for the
        // deadline, or for a byte that decides it.
        if self.tail_hold.is_some() {
            return chunks;
        }
        // A reply still arriving is not input: an interruption leaves it to
        // its own bound (ADR-0013 D1).
        if mode == Release::Interrupt && self.interruption_keeps_pending() {
            return chunks;
        }

        if let Some(family) = self.discard_until {
            // An interruption drops what a discard still holds and ends it;
            // a discard holding nothing is kept for the reply tail to come.
            if interrupt && !self.buffer.is_empty() {
                tracing::debug!(
                    len = self.buffer.len(),
                    "dropping discarded reply bytes on interruption"
                );
                self.buffer.clear();
                self.discard_until = None;
                self.discarded_tail_bytes = 0;
                return chunks;
            }
            if family == ControlStringFamily::HostReplyCsi {
                return chunks;
            }
            #[cfg(any(windows, test))]
            if self.host_color_tail_deadline.is_some() {
                return chunks;
            }
            if defer_loss {
                return chunks;
            }
            let keep_split_st = self.buffer.last() == Some(&ESC);
            let keep_discarding = keep_split_st;
            self.discarded_tail_bytes = self.discarded_tail_bytes.saturating_add(self.buffer.len());
            self.buffer.clear();
            if keep_discarding && self.discarded_tail_bytes <= MAX_DISCARDED_CONTROL_TAIL_BYTES {
                if keep_split_st {
                    self.buffer.push(ESC);
                }
            } else {
                self.discard_until = None;
                self.discarded_tail_bytes = 0;
            }
            return chunks;
        }

        if self.buffer.is_empty() {
            return chunks;
        }

        // A confirmed host sends Escape as `CSI 27u`, so a mouse report prefix
        // that outlives keyboard timing may still get a delayed tail (#3480).
        // Keep it until its deadline; `push` releases it if other input follows.
        // Prefixes already inside a sequence waited the long window; only the
        // ambiguous ESC / ESC[ prefixes need this extra mouse-tail hold.
        #[cfg(unix)]
        if !mouse_wait_served
            && self.host_escape_disambiguation_active
            && self.pending_could_be_escape_key()
            && could_continue_as_mouse_report(&self.buffer)
        {
            let len = self.buffer.len();
            self.open_tail_hold(HoldAfter::DisambiguatedPrefix { len });
            return chunks;
        }

        if starts_with_incomplete_sgr_mouse_sequence(&self.buffer) {
            if defer_loss {
                return chunks;
            }
            // `CSI <` starts no key: these bytes are a mouse report fragment.
            // They are dropped, and what follows them is held as its tail.
            tracing::debug!(
                bytes = ?self.buffer,
                "discarding incomplete SGR mouse sequence after input timeout"
            );
            let prefix = std::mem::take(&mut self.buffer);
            if prefix.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES
                && plausible_sgr_mouse_prefix(&prefix)
            {
                self.open_tail_hold_on(mode, HoldAfter::DroppedPrefix(prefix));
            }
            return chunks;
        }

        if self.buffer.starts_with(BRACKETED_PASTE_START)
            && find_subsequence(&self.buffer, BRACKETED_PASTE_END).is_none()
        {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for bracketed paste terminator"
            );
            return chunks;
        }

        // Past the reply window and its recovery, a reply prefix takes the
        // generic paths below, which hold its tail (#5251).
        #[cfg(any(windows, test))]
        if self.host_default_color_query_issued
            && (self.host_default_color_query_deadline.is_some() || host_color_query_expired)
            && is_incomplete_host_default_color_prefix(&self.buffer)
        {
            // Inside its window an interruption keeps it too: it can only
            // complete the reply or be dropped (ADR-0013 D1).
            if mode != Release::Backstop
                && self.host_default_color_query_deadline.is_some()
                && self.buffer.len() <= MAX_DISCARDED_CONTROL_TAIL_BYTES
            {
                return chunks;
            }
            if defer_loss {
                return chunks;
            }
            if !interrupt
                && host_color_query_expired
                && self.buffer.len() <= MAX_DISCARDED_CONTROL_TAIL_BYTES
            {
                self.discard_until = Some(ControlStringFamily::Osc);
                let split_st = self.buffer.last() == Some(&ESC);
                self.discarded_tail_bytes = self.buffer.len() - usize::from(split_st);
                self.host_color_tail_deadline = Some(self.now() + HOST_COLOR_TAIL_RECOVERY_GRACE);
                self.host_color_tail_split_st = split_st;
                self.buffer.clear();
                if split_st {
                    self.buffer.push(ESC);
                }
                return chunks;
            }
            self.buffer.clear();
            return chunks;
        }

        if starts_with_incomplete_default_color_response(&self.buffer) {
            if defer_loss {
                return chunks;
            }
            if interrupt || self.host_color_reply_abandoned() {
                // The host went quiet mid-reply; never let it hold input (#5052).
                tracing::debug!(
                    len = self.buffer.len(),
                    "dropping unfinished host color response after idle limit"
                );
                self.buffer.clear();
                self.partial_host_color_reply_read_at = None;
                return chunks;
            }
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for host color response terminator"
            );
            return chunks;
        }

        if (self.host_cell_size_replies_awaited > 0 || self.host_appearance_reply_awaited)
            && self.buffer.as_slice() == b"\x1b["
        {
            if !self.held_pending_host_reply_esc && !mouse_wait_served {
                self.held_pending_host_reply_esc = true;
                tracing::trace!("holding incomplete host CSI reply one flush");
                return chunks;
            }
            self.host_cell_size_replies_awaited = 0;
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
        }

        if self.host_cell_size_replies_awaited > 0
            && starts_with_incomplete_host_cell_size_report(&self.buffer)
        {
            if defer_loss {
                return chunks;
            }
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host cell size report after input timeout"
            );
            self.host_cell_size_replies_awaited = 0;
            self.held_pending_host_reply_esc = false;
            if !interrupt {
                self.discard_until = Some(ControlStringFamily::HostReplyCsi);
            }
            self.discarded_tail_bytes = 0;
            self.buffer.clear();
            return chunks;
        }

        if starts_with_incomplete_host_color_scheme_report(&self.buffer) {
            if !interrupt && self.host_appearance_reply_awaited && !self.held_pending_host_reply_esc
            {
                self.held_pending_host_reply_esc = true;
                tracing::trace!(
                    len = self.buffer.len(),
                    "holding incomplete host color scheme report one flush"
                );
                return chunks;
            }
            if defer_loss {
                return chunks;
            }
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host color scheme report after input timeout"
            );
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
            if !interrupt {
                self.discard_until = Some(ControlStringFamily::HostReplyCsi);
            }
            self.discarded_tail_bytes = 0;
            self.buffer.clear();
            return chunks;
        }

        // A legacy host sends Alt+[, Alt+O and Alt+Escape as ESC plus that byte. When nothing
        // followed and no reply is expected, it was the key. String introducers
        // (OSC, DCS, APC, PM, SOS) also start host replies, so they keep the
        // control-string path and are never forwarded as Alt keys (#344).
        if is_alt_key_introducer(&self.buffer) && !self.awaiting_host_reply() {
            let csi = self.buffer.as_slice() == b"\x1b[";
            chunks.push(std::mem::take(&mut self.buffer));
            if csi {
                // The rest of a split mouse report can still follow.
                self.open_tail_hold_on(mode, HoldAfter::CsiIntroducer);
            }
            return chunks;
        }

        if let Some(ControlString::Incomplete { family }) = control_string(&self.buffer) {
            if defer_loss {
                return chunks;
            }
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host control string after input timeout"
            );
            let split_st = self.buffer.last() == Some(&ESC);
            let mut header = std::mem::take(&mut self.buffer);
            header.truncate(MAX_DISCARDED_CONTROL_TAIL_BYTES + 1);
            if interrupt {
                return chunks;
            }
            if family == ControlStringFamily::Osc {
                // Its tail is held under a deadline and dropped only if it
                // completes a reply; anything else is input (#5251).
                self.open_tail_hold(HoldAfter::OscHeader { split_st, header });
                return chunks;
            }
            // This intentionally gives host control replies precedence over legacy
            // Alt forms like Alt+] after timeout, so later reply tails cannot leak.
            self.discard_until = Some(family);
            self.discarded_tail_bytes = 0;
            return chunks;
        }

        if self.buffer.as_slice() == [ESC] {
            #[cfg(any(windows, test))]
            if !interrupt
                && self.host_color_replies_awaited > 0
                && self.host_default_color_query_deadline.is_some()
            {
                // Native console reply fragments can arrive across several idle polls.
                return chunks;
            }
            if self.awaiting_host_reply() && !self.held_pending_host_reply_esc && !mouse_wait_served
            {
                self.held_pending_host_reply_esc = true;
                tracing::trace!("holding lone escape one flush while awaiting host reply");
                return chunks;
            }
            // The reply can still follow without its ESC; see `classify_held_tail`.
            let color_reply = self.host_color_replies_awaited > 0 && self.awaiting_host_reply();
            // No continuation arrived; give up the window so Escape is not delayed again.
            self.host_color_replies_awaited = 0;
            #[cfg(any(windows, test))]
            {
                self.host_default_color_query_deadline = None;
            }
            self.host_cell_size_replies_awaited = 0;
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
            tracing::warn!(
                bytes = ?self.buffer,
                "flushing lone escape after input timeout; if this follows an alt chord or focus switch it may reach the pane as plain esc"
            );
            chunks.push(std::mem::take(&mut self.buffer));
            self.open_tail_hold_on(mode, HoldAfter::Escape { color_reply });
            return chunks;
        }

        if let Ok(text) = std::str::from_utf8(&self.buffer) {
            if parse_terminal_key_sequence(text).is_some() {
                // While a reply is awaited, `ESC [` is released here rather
                // than as an Alt introducer above; its tail is held the same.
                let csi = self.buffer.as_slice() == b"\x1b[";
                chunks.push(std::mem::take(&mut self.buffer));
                if csi {
                    self.open_tail_hold_on(mode, HoldAfter::CsiIntroducer);
                }
                return chunks;
            }
        }

        if !interrupt && starts_with_incomplete_utf8_char(&self.buffer) {
            tracing::trace!(bytes = ?self.buffer, "waiting for UTF-8 continuation bytes");
            return chunks;
        }

        if !interrupt
            && self.buffer.first() == Some(&ESC)
            && starts_with_incomplete_utf8_char(&self.buffer[1..])
        {
            tracing::trace!(bytes = ?self.buffer, "waiting for escaped UTF-8 continuation bytes");
            return chunks;
        }

        // A byte that starts no UTF-8 character (a continuation whose lead
        // byte was dropped) is not input; it goes alone, not with the input
        // after it.
        if self.buffer.first() != Some(&ESC) {
            if let Err(err) = std::str::from_utf8(&self.buffer) {
                if err.valid_up_to() == 0 {
                    let invalid = err.error_len().unwrap_or(self.buffer.len());
                    tracing::debug!(len = invalid, "dropping invalid UTF-8 input");
                    self.buffer.drain(..invalid);
                    chunks.extend(self.drain_available_chunks());
                    if !self.buffer.is_empty() {
                        chunks.extend(self.release(mode));
                    }
                    return chunks;
                }
            }
        }

        // An incomplete sequence that cannot be a key, waited out or
        // interrupted: never input. Its CSI prefix may still get a tail.
        if defer_loss {
            return chunks;
        }
        tracing::debug!(bytes = ?self.buffer, "dropping incomplete raw input buffer after timeout");
        let prefix = std::mem::take(&mut self.buffer);
        if is_dropped_csi_key_prefix(&prefix) {
            self.open_tail_hold_on(mode, HoldAfter::DroppedPrefix(prefix));
        }
        chunks
    }

    /// Applies the open tail hold to the front of `buffer`. Returns false
    /// when the bytes must stay held.
    fn resolve_tail_hold(&mut self) -> bool {
        let Some(hold) = &self.tail_hold else {
            return true;
        };
        #[cfg(unix)]
        if matches!(hold.after, HoldAfter::DisambiguatedPrefix { .. }) {
            return false;
        }
        if self.buffer.is_empty() {
            return false;
        }
        // One read, as the reader sees it: every held byte arrived within
        // the idle flush interval of the first (ADR-0012 D3).
        let burst = match (self.pending_arrival(), self.arrivals.back()) {
            (Some(first), Some((_, last))) => {
                last.saturating_duration_since(first) <= ORPHAN_TAIL_BURST
            }
            _ => false,
        };
        let prefix = hold.after.released_prefix();
        let reply_shaped = if hold.interrupted {
            hold.after.reply_shaped_len(&self.buffer)
        } else {
            0
        };
        match classify_held_tail(&hold.after, &self.buffer, burst, self.pump_policy()) {
            // The deadline follows from the held bytes' arrival.
            Verdict::Hold => false,
            Verdict::Drop(len) => {
                tracing::debug!(len, "discarding report tail after its timed-out prefix");
                self.buffer.drain(..len);
                self.tail_hold = None;
                true
            }
            Verdict::Rejoin(_) => {
                // Put the released prefix back: the reply (or pump X10
                // report) is complete, so it parses as one and never as input.
                self.buffer.splice(0..0, prefix.iter().copied());
                self.tail_hold = None;
                true
            }
            Verdict::Emit => {
                // After a semantic key, the reply-shaped bytes before the
                // byte that ended the reply are not typed (ADR-0013 D1).
                self.buffer.drain(..reply_shaped);
                self.tail_hold = None;
                true
            }
        }
    }

    fn drain_available_chunks(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();

        loop {
            if !self.resolve_tail_hold() {
                break;
            }

            if let Some(family) = self.discard_until {
                #[cfg(any(windows, test))]
                if let Some(deadline) = self.host_color_tail_deadline {
                    if self.host_color_tail_split_st
                        && self.buffer.len() > 1
                        && self.buffer[1] != b'\\'
                    {
                        self.buffer.drain(..1);
                        self.end_host_color_tail_recovery();
                        continue;
                    }
                    let terminator_len = osc_string_terminator(&self.buffer);
                    if let Some(new_escape) = self
                        .buffer
                        .windows(2)
                        .position(|pair| pair[0] == ESC && pair[1] != b'\\')
                    {
                        if terminator_len.is_none_or(|len| new_escape < len) {
                            self.buffer.drain(..new_escape);
                            self.end_host_color_tail_recovery();
                            continue;
                        }
                    }
                    let body_len = terminator_len
                        .map(|len| len - if self.buffer[len - 1] == 0x07 { 1 } else { 2 })
                        .unwrap_or(self.buffer.len());
                    if let Some(invalid) = self.buffer[..body_len]
                        .iter()
                        .position(|byte| !plausible_host_color_tail_byte(*byte))
                    {
                        self.buffer.drain(..invalid);
                        self.end_host_color_tail_recovery();
                        continue;
                    }
                    if let Some(terminator_len) = terminator_len {
                        if self.discarded_tail_bytes + terminator_len
                            <= MAX_DISCARDED_CONTROL_TAIL_BYTES
                        {
                            self.buffer.drain(..terminator_len);
                            self.end_host_color_tail_recovery();
                            continue;
                        }
                    }
                    if self.now() >= deadline
                        || self.discarded_tail_bytes + self.buffer.len()
                            >= MAX_DISCARDED_CONTROL_TAIL_BYTES
                    {
                        self.end_host_color_tail_recovery();
                        continue;
                    }
                    break;
                }
                if family == ControlStringFamily::HostReplyCsi {
                    if discard_host_reply_csi_tail(&mut self.buffer, &mut self.discarded_tail_bytes)
                    {
                        self.discard_until = None;
                        self.discarded_tail_bytes = 0;
                        continue;
                    }
                    break;
                }
                let Some(terminator_len) =
                    control_string_terminator_for_family(&self.buffer, family)
                else {
                    break;
                };
                self.buffer.drain(..terminator_len);
                self.discard_until = None;
                self.discarded_tail_bytes = 0;
                continue;
            }

            // Kitty key reports carry Alt in their modifiers and are never
            // Alt-prefixed, so ESC before a complete report is its own key. WezTerm
            // sends an Escape press as bare ESC and the release as a report,
            // and a quick tap delivers both together (#1266).
            if self.buffer.starts_with(b"\x1b\x1b")
                && (self.split_coalesced_escape || starts_with_kitty_key_report(&self.buffer[1..]))
            {
                chunks.push(vec![ESC]);
                self.buffer.drain(..1);
                continue;
            }

            let Some((event, consumed)) = extract_one_event(&self.buffer) else {
                break;
            };
            if matches!(
                event,
                RawInputEvent::HostDefaultColor { .. } | RawInputEvent::HostPaletteColors { .. }
            ) {
                self.host_color_replies_awaited = self.host_color_replies_awaited.saturating_sub(1);
            } else if matches!(event, RawInputEvent::HostCellSizeReport { .. }) {
                self.host_cell_size_replies_awaited =
                    self.host_cell_size_replies_awaited.saturating_sub(1);
            } else if self.host_appearance_query_on_focus
                && matches!(event, RawInputEvent::OuterFocusGained)
            {
                self.host_appearance_query_sent();
            } else if matches!(event, RawInputEvent::HostColorSchemeChanged(_)) {
                self.host_appearance_reply_awaited = false;
                if self.host_color_scheme_change_tracking {
                    self.host_color_query_sent();
                }
            }
            if is_host_reply_event(&event) && self.host_reply_activity_at.is_some() {
                // Long reply bursts (256 palette entries) stay protected.
                self.host_reply_activity_at = Some(self.now());
            }
            self.held_pending_host_reply_esc = false;
            chunks.push(self.buffer[..consumed].to_vec());
            self.buffer.drain(..consumed);
        }

        chunks
    }

    #[cfg(any(windows, test))]
    fn end_host_color_tail_recovery(&mut self) {
        self.discard_until = None;
        self.discarded_tail_bytes = 0;
        self.host_color_tail_deadline = None;
        self.host_color_tail_split_st = false;
    }
}

const MAX_DISCARDED_CONTROL_TAIL_BYTES: usize = 128;

/// Whether `bytes` starts with a complete sequence shaped like a kitty key
/// report, `CSI <code>[:alternates][;mods[:event][;text]] u`. Only the shape is
/// checked: digit-led parameters made of digits, `;` and `:`, ending in `u`.
/// That excludes `CSI u` (cursor restore) and private replies like `CSI ? 7 u`.
fn starts_with_kitty_key_report(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"\x1b[") else {
        return false;
    };
    let params = rest
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        .count();
    rest.first().is_some_and(u8::is_ascii_digit) && rest.get(params) == Some(&b'u')
}

#[cfg(any(windows, test))]
const HOST_COLOR_TAIL_RECOVERY_GRACE: Duration = Duration::from_millis(100);

#[cfg(any(windows, test))]
fn plausible_host_color_tail_byte(byte: u8) -> bool {
    plausible_color_reply_payload_byte(byte) || byte == ESC
}

/// Bytes an OSC 4/10/11 color reply carries between its header and terminator.
fn plausible_color_reply_payload_byte(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
        || matches!(
            byte,
            b';' | b':'
                | b'/'
                | b'#'
                | b'?'
                | b'.'
                | b'_'
                | b'-'
                | b'+'
                | b'r'
                | b'g'
                | b'b'
                | b'R'
                | b'G'
                | b'B'
        )
}

enum DiscardedOscTail {
    /// Could still be a color reply tail; wait for more bytes.
    Pending,
    /// A color reply tail through its BEL or ST terminator.
    Terminated(usize),
    /// Holds a byte no color reply tail can carry, so it is all input.
    Foreign,
}

fn classify_discarded_osc_tail(tail: &[u8]) -> DiscardedOscTail {
    for (index, byte) in tail.iter().enumerate() {
        if index >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
            return DiscardedOscTail::Foreign;
        }
        match *byte {
            0x07 => return DiscardedOscTail::Terminated(index + 1),
            ESC => {
                return match tail.get(index + 1) {
                    None => DiscardedOscTail::Pending,
                    Some(b'\\') => DiscardedOscTail::Terminated(index + 2),
                    Some(_) => DiscardedOscTail::Foreign,
                }
            }
            byte if plausible_color_reply_payload_byte(byte) => {}
            _ => return DiscardedOscTail::Foreign,
        }
    }
    DiscardedOscTail::Pending
}

#[derive(Debug, PartialEq, Eq)]
enum OrphanedColorReply {
    /// Starts with a full color reply header of this many bytes.
    Header(usize),
    /// Could still grow into one.
    Prefix,
    Unrelated,
}

/// Classifies bytes that followed a lone ESC already released as a key.
fn orphaned_color_reply_tail(buffer: &[u8]) -> OrphanedColorReply {
    const HEADERS: [&[u8]; 3] = [b"]10;", b"]11;", b"]4;"];
    if let Some(header) = HEADERS.iter().find(|header| buffer.starts_with(header)) {
        OrphanedColorReply::Header(header.len())
    } else if !buffer.is_empty() && HEADERS.iter().any(|header| header.starts_with(buffer)) {
        OrphanedColorReply::Prefix
    } else {
        OrphanedColorReply::Unrelated
    }
}

#[cfg(any(unix, test))]
pub(crate) fn events_require_host_surface_redraw(
    events: &[RawInputEvent],
    redraw_on_focus_gained: bool,
) -> bool {
    redraw_on_focus_gained
        && events
            .iter()
            .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(unix, test))]
pub(crate) fn events_require_host_mode_refresh(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(not(windows), test))]
pub(crate) fn events_require_host_terminal_appearance_query(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(not(windows), test))]
pub(crate) fn events_require_host_terminal_theme_query(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::HostColorSchemeChanged(_)))
}

fn extract_one_event(buffer: &[u8]) -> Option<(RawInputEvent, usize)> {
    if buffer.is_empty() {
        return None;
    }

    if buffer.starts_with(BRACKETED_PASTE_START) {
        let end = find_subsequence(buffer, BRACKETED_PASTE_END)?;
        let content = std::str::from_utf8(&buffer[BRACKETED_PASTE_START.len()..end]).ok()?;
        return Some((
            RawInputEvent::Paste(content.to_string()),
            end + BRACKETED_PASTE_END.len(),
        ));
    }

    if buffer[0] == ESC {
        let seq_len = complete_escape_sequence_len(buffer)?;
        if buffer[..seq_len].starts_with(b"\x1b[M") {
            let event = parse_default_mouse(&buffer[..seq_len])
                .map(RawInputEvent::Mouse)
                .unwrap_or(RawInputEvent::Unsupported);
            return Some((event, seq_len));
        }
        let seq = std::str::from_utf8(&buffer[..seq_len]).ok()?;

        if let Some((kind, color)) = parse_default_color_response(seq) {
            return Some((RawInputEvent::HostDefaultColor { kind, color }, seq_len));
        }
        if let Some((index, color)) = parse_palette_color_response(seq) {
            return Some((
                RawInputEvent::HostPaletteColors {
                    colors: vec![(index, color)],
                },
                seq_len,
            ));
        }

        match seq {
            "\x1b[I" => return Some((RawInputEvent::OuterFocusGained, seq_len)),
            "\x1b[O" => return Some((RawInputEvent::OuterFocusLost, seq_len)),
            _ => {}
        }

        if let Some(appearance) = parse_host_color_scheme_report(&buffer[..seq_len]) {
            return Some((RawInputEvent::HostColorSchemeChanged(appearance), seq_len));
        }

        if let Some((width_px, height_px)) = parse_host_cell_size_report(&buffer[..seq_len]) {
            return Some((
                RawInputEvent::HostCellSizeReport {
                    width_px,
                    height_px,
                },
                seq_len,
            ));
        }

        if let Some(mouse) = parse_sgr_mouse(seq) {
            return Some((RawInputEvent::Mouse(mouse), seq_len));
        }

        if let Some(key) = parse_terminal_key_sequence(seq) {
            return Some((
                RawInputEvent::Key(key.with_vt_bytes(buffer[..seq_len].to_vec())),
                seq_len,
            ));
        }

        tracing::debug!(sequence = ?seq, "dropping unsupported escape sequence");
        return Some((RawInputEvent::Unsupported, seq_len));
    }

    let consumed = first_complete_utf8_char_len(buffer)?;
    let text = std::str::from_utf8(&buffer[..consumed]).ok()?;
    let key = parse_terminal_key_sequence(text)?
        .with_text_commit()
        .with_vt_bytes(buffer[..consumed].to_vec());
    Some((RawInputEvent::Key(key), consumed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlStringFamily {
    Osc,
    StTerminated,
    HostReplyCsi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlString {
    Complete {
        len: usize,
        family: ControlStringFamily,
    },
    Incomplete {
        family: ControlStringFamily,
    },
}

fn parse_host_color_scheme_report(buffer: &[u8]) -> Option<HostAppearance> {
    match buffer {
        GHOSTTY_COLOR_SCHEME_DARK_REPORT => Some(HostAppearance::Dark),
        GHOSTTY_COLOR_SCHEME_LIGHT_REPORT => Some(HostAppearance::Light),
        _ => None,
    }
}

/// Parses an XTWINOPS cell size report (`CSI 6 ; height ; width t`) into
/// `(width_px, height_px)`; note the reply orders height first.
fn parse_host_cell_size_report(buffer: &[u8]) -> Option<(u32, u32)> {
    let body = buffer.strip_prefix(b"\x1b[")?.strip_suffix(b"t")?;
    let text = std::str::from_utf8(body).ok()?;
    let mut params = text.split(';');
    if params.next()? != "6" {
        return None;
    }
    let height_px = params.next()?.parse::<u32>().ok()?;
    let width_px = params.next()?.parse::<u32>().ok()?;
    if params.next().is_some() || width_px == 0 || height_px == 0 {
        return None;
    }
    Some((width_px, height_px))
}

fn starts_with_incomplete_default_color_response(buffer: &[u8]) -> bool {
    // The prefix check keeps this constant-cost for ordinary input.
    matches!(buffer.get(..5), Some(b"\x1b]10;" | b"\x1b]11;"))
        && matches!(
            control_string(buffer),
            Some(ControlString::Incomplete {
                family: ControlStringFamily::Osc
            })
        )
}

#[cfg(any(windows, test))]
fn is_incomplete_host_default_color_prefix(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b]")
        && matches!(
            control_string(buffer),
            Some(ControlString::Incomplete {
                family: ControlStringFamily::Osc
            })
        )
        && [b"\x1b]10;".as_slice(), b"\x1b]11;".as_slice()]
            .iter()
            .any(|prefix| prefix.starts_with(buffer) || buffer.starts_with(prefix))
}

/// An unfinished OSC 10, 11 or 4 colour reply, or a prefix of one (`ESC ]`,
/// `ESC ] 1`, ...).
/// Whether `bytes` is the start of a colour reply, `ESC ] 10 ;`, `ESC ] 11 ;`
/// or `ESC ] 4 ; <index> ;`, then `rgb:` and up to three groups of one to
/// four hex digits, possibly up to the ESC of its ST, and nothing past it:
/// text such as `ESC ] 4 ; 50 ; a` is not one (ADR-0013 D1).
fn is_color_reply_shape_prefix(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"\x1b]") else {
        return false;
    };
    let headers: [&[u8]; 3] = [b"10;", b"11;", b"4;"];
    if headers.iter().any(|header| header.starts_with(rest)) {
        return true;
    }
    let body = if let Some(body) = rest.strip_prefix(b"10;").or(rest.strip_prefix(b"11;")) {
        body
    } else if let Some(index) = rest.strip_prefix(b"4;") {
        let digits = index
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=3).contains(&digits) {
            return false;
        }
        match index.get(digits) {
            None => return true,
            Some(b';') => &index[digits + 1..],
            Some(_) => return false,
        }
    } else {
        return false;
    };
    if b"rgb:".starts_with(body) {
        return true;
    }
    let Some(groups) = body.strip_prefix(b"rgb:") else {
        return false;
    };
    let (groups, split_st) = match groups.strip_suffix(&[ESC]) {
        Some(groups) => (groups, true),
        None => (groups, false),
    };
    let groups: Vec<&[u8]> = groups.split(|byte| *byte == b'/').collect();
    let last = groups.len() - 1;
    groups.len() <= 3
        && groups.iter().enumerate().all(|(index, group)| {
            group.iter().all(u8::is_ascii_hexdigit)
                && group.len() <= 4
                && (index == last && !split_st || !group.is_empty())
        })
        && (!split_st || groups.len() == 3)
}

fn starts_with_incomplete_host_color_scheme_report(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[?")
        && (GHOSTTY_COLOR_SCHEME_DARK_REPORT.starts_with(buffer)
            || GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.starts_with(buffer))
        && buffer.len() < GHOSTTY_COLOR_SCHEME_DARK_REPORT.len()
}

fn starts_with_incomplete_host_cell_size_report(buffer: &[u8]) -> bool {
    let Some(body) = buffer.strip_prefix(b"\x1b[") else {
        return false;
    };
    if body.is_empty() || body.last() == Some(&b't') {
        return false;
    }

    let mut params = body.split(|byte| *byte == b';');
    if params.next() != Some(b"6".as_slice()) {
        return false;
    }
    let height = params.next();
    let width = params.next();
    params.next().is_none()
        && height.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && width.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && !(height.is_some_and(<[u8]>::is_empty) && width.is_some())
}

fn is_host_reply_event(event: &RawInputEvent) -> bool {
    matches!(
        event,
        RawInputEvent::HostDefaultColor { .. }
            | RawInputEvent::HostPaletteColors { .. }
            | RawInputEvent::HostCellSizeReport { .. }
            | RawInputEvent::HostColorSchemeChanged(_)
    )
}

/// ESC plus a byte that can also start a longer sequence: Alt+[, Alt+O, and
/// Alt+Escape (ESC ESC, kept whole where doubled escapes are preserved).
fn is_alt_key_introducer(buffer: &[u8]) -> bool {
    matches!(buffer, [ESC, b'[' | b'O' | ESC])
}

#[cfg(any(unix, test))]
fn is_escape_key_prefix(buffer: &[u8]) -> bool {
    match buffer {
        [ESC] => true,
        [ESC, introducer] => matches!(
            introducer,
            b'[' | b'O' | b'P' | b']' | b'X' | b'^' | b'_' | 0x1b
        ),
        _ => false,
    }
}

fn control_string(buffer: &[u8]) -> Option<ControlString> {
    let family = match buffer.get(..2)? {
        b"\x1b]" => ControlStringFamily::Osc,
        b"\x1bP" | b"\x1b_" | b"\x1b^" | b"\x1bX" => ControlStringFamily::StTerminated,
        _ => return None,
    };

    Some(match control_string_terminator_for_family(buffer, family) {
        Some(len) => ControlString::Complete { len, family },
        None => ControlString::Incomplete { family },
    })
}

fn first_complete_utf8_char_len(buffer: &[u8]) -> Option<usize> {
    let width = utf8_char_width(*buffer.first()?)?;

    if buffer.len() < width {
        return None;
    }

    std::str::from_utf8(&buffer[..width]).ok()?;
    Some(width)
}

fn starts_with_incomplete_utf8_char(buffer: &[u8]) -> bool {
    match std::str::from_utf8(buffer) {
        Ok(_) => false,
        Err(err) => err.valid_up_to() == 0 && err.error_len().is_none(),
    }
}

fn utf8_char_width(first: u8) -> Option<usize> {
    if first < 0x80 {
        Some(1)
    } else if first & 0b1110_0000 == 0b1100_0000 {
        Some(2)
    } else if first & 0b1111_0000 == 0b1110_0000 {
        Some(3)
    } else if first & 0b1111_1000 == 0b1111_0000 {
        Some(4)
    } else {
        None
    }
}

fn complete_escape_sequence_len(buffer: &[u8]) -> Option<usize> {
    if buffer.len() == 1 {
        return None;
    }

    if buffer.starts_with(b"\x1b\x1b[<") {
        if let Some(mouse_len) = find_csi_final(&buffer[1..], b"Mm") {
            let mouse_sequence = std::str::from_utf8(&buffer[1..1 + mouse_len]).ok()?;
            if parse_sgr_mouse(mouse_sequence).is_some() {
                return Some(1);
            }
        }
    }

    if buffer.len() >= 7
        && buffer.starts_with(b"\x1b\x1b[M")
        && parse_default_mouse(&buffer[1..7]).is_some()
    {
        return Some(1);
    }

    if buffer.starts_with(b"\x1b\x1b") {
        return complete_escape_sequence_len(&buffer[1..]).map(|len| len + 1);
    }

    // No sequence carries ESC right after its CSI or SS3 introducer: the
    // introducer was a legacy Alt+[ / Alt+O and a new sequence starts.
    if matches!(buffer, [ESC, b'[' | b'O', ESC, ..]) {
        return Some(2);
    }

    if buffer.starts_with(b"\x1b[") {
        if buffer.starts_with(b"\x1b[<") {
            return find_csi_final(buffer, b"Mm");
        }
        if buffer.starts_with(b"\x1b[M") {
            return (buffer.len() >= 6).then_some(6);
        }
        return find_csi_final(
            buffer,
            b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~",
        );
    }

    if let Some(control) = control_string(buffer) {
        return match control {
            ControlString::Complete { len, .. } => Some(len),
            ControlString::Incomplete { .. } => None,
        };
    }

    if buffer.starts_with(b"\x1bO") {
        return (buffer.len() >= 3).then_some(3);
    }

    let escaped_char_width = utf8_char_width(buffer[1])?;
    if buffer.len() < 1 + escaped_char_width {
        return None;
    }
    std::str::from_utf8(&buffer[1..1 + escaped_char_width]).ok()?;
    Some(1 + escaped_char_width)
}

fn starts_with_incomplete_sgr_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[<")
        && buffer[3..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

/// Whether bytes after a held ESC or ESC[ still read as one escape sequence,
/// such as a mouse report, host reply, or bracketed paste, rather than a key.
#[cfg(unix)]
fn continues_escape_sequence(buffer: &[u8]) -> bool {
    match buffer {
        [ESC] | [ESC, b'['] => true,
        // OSC, DCS and APC host replies can also split after their ESC.
        [ESC, b']' | b'P' | b'_', ..] => true,
        [ESC, b'[', next, ..] => *next == b'M' || matches!(*next, 0x20..=0x3f),
        _ => false,
    }
}

#[cfg(unix)]
fn could_continue_as_mouse_report(buffer: &[u8]) -> bool {
    [b"\x1b[<".as_slice(), b"\x1b[M"]
        .iter()
        .any(|prefix| buffer.starts_with(prefix) || prefix.starts_with(buffer))
}

#[cfg(any(unix, windows, test))]
#[cfg(any(windows, test))]
fn starts_with_incomplete_default_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[M") && buffer.len() < 6
}

fn discard_host_reply_csi_tail(buffer: &mut Vec<u8>, discarded_tail_bytes: &mut usize) -> bool {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(*discarded_tail_bytes);
    let inspected = buffer.len().min(remaining);

    for index in 0..inspected {
        match buffer[index] {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                buffer.drain(..=index);
                return true;
            }
            _ => {
                buffer.drain(..index);
                return true;
            }
        }
    }

    buffer.drain(..inspected);
    *discarded_tail_bytes = discarded_tail_bytes.saturating_add(inspected);
    *discarded_tail_bytes >= MAX_DISCARDED_CONTROL_TAIL_BYTES
}

enum SgrMouseContinuation {
    Incomplete,
    Complete(usize),
    Invalid,
}

fn classify_sgr_mouse_continuation(prefix: &[u8], tail: &[u8]) -> SgrMouseContinuation {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(prefix.len());
    let tail = &tail[..tail.len().min(remaining)];

    // A prefix can stop before the `[<` introducer is complete, and a further
    // read supplies the rest.
    let mut report = prefix.to_vec();
    let mut consumed_from_tail = 0;
    if let Some(rest) = b"\x1b[<"
        .strip_prefix(prefix)
        .filter(|rest| !rest.is_empty())
    {
        if tail.len() < rest.len() {
            return SgrMouseContinuation::Incomplete;
        }
        if !tail.starts_with(rest) {
            return SgrMouseContinuation::Invalid;
        }
        report.extend_from_slice(rest);
        consumed_from_tail = rest.len();
    }
    let tail = &tail[consumed_from_tail..];

    let final_index = tail
        .iter()
        .position(|byte| !byte.is_ascii_digit() && *byte != b';');
    let payload_len = final_index.unwrap_or(tail.len());
    report.extend_from_slice(&tail[..payload_len]);
    if !plausible_sgr_mouse_prefix(&report) {
        return SgrMouseContinuation::Invalid;
    }
    if let Some(index) = final_index {
        report.push(tail[index]);
        let valid = std::str::from_utf8(&report)
            .ok()
            .and_then(parse_sgr_mouse)
            .is_some();
        return if valid {
            SgrMouseContinuation::Complete(consumed_from_tail + index + 1)
        } else {
            SgrMouseContinuation::Invalid
        };
    }
    if report.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
        SgrMouseContinuation::Invalid
    } else {
        SgrMouseContinuation::Incomplete
    }
}

/// An incomplete CSI dropped at its timeout whose tail could still make it a
/// key or focus report: `ESC [` and parameter or intermediate bytes. X10
/// (`M`) and SGR (`<`) prefixes are handled elsewhere (ADR-0012 D3).
fn is_dropped_csi_key_prefix(prefix: &[u8]) -> bool {
    let Some(params) = prefix.strip_prefix(b"\x1b[") else {
        return false;
    };
    !params.is_empty()
        && prefix.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES
        && !matches!(params.first(), Some(b'M' | b'<'))
        && params.iter().all(|byte| (0x20..=0x3f).contains(byte))
}

// Reject impossible continuations early, without changing the general mouse
// parser. A partial last field (including zero) can still become valid.
fn plausible_sgr_mouse_prefix(report: &[u8]) -> bool {
    // The prefix may stop before the `[<` introducer is complete; the next
    // read can still supply the rest of it.
    if report == b"\x1b[" {
        return true;
    }
    let Some(body) = report.strip_prefix(b"\x1b[<") else {
        return false;
    };
    let mut fields = body.split(|byte| *byte == b';').enumerate().peekable();
    while let Some((field, digits)) = fields.next() {
        if field > 2 {
            return false;
        }
        if digits.is_empty() {
            return fields.peek().is_none();
        }
        if !digits.iter().all(u8::is_ascii_digit) {
            return false;
        }
        let Some(value) = std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| digits.parse::<u16>().ok())
        else {
            return false;
        };
        if field == 0 && value > u16::from(u8::MAX) {
            return false;
        }
        if fields.peek().is_some()
            && ((field == 0 && parse_mouse_cb(value as u8).is_none()) || (field == 1 && value == 0))
        {
            return false;
        }
    }
    true
}

fn osc_string_terminator(buffer: &[u8]) -> Option<usize> {
    let st = find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2);
    let bel = buffer
        .iter()
        .position(|byte| *byte == b'\x07')
        .map(|idx| idx + 1);

    match (st, bel) {
        (Some(st), Some(bel)) => Some(st.min(bel)),
        (Some(st), None) => Some(st),
        (None, Some(bel)) => Some(bel),
        (None, None) => None,
    }
}

fn st_string_terminator(buffer: &[u8]) -> Option<usize> {
    find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2)
}

fn control_string_terminator_for_family(
    buffer: &[u8],
    family: ControlStringFamily,
) -> Option<usize> {
    match family {
        ControlStringFamily::Osc => osc_string_terminator(buffer),
        ControlStringFamily::StTerminated => st_string_terminator(buffer),
        ControlStringFamily::HostReplyCsi => None,
    }
}

fn find_csi_final(buffer: &[u8], finals: &[u8]) -> Option<usize> {
    for (idx, byte) in buffer.iter().enumerate().skip(2) {
        if finals.contains(byte) {
            return Some(idx + 1);
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_default_mouse(sequence: &[u8]) -> Option<MouseEvent> {
    let &[ESC, b'[', b'M', encoded_cb, encoded_column, encoded_row] = sequence else {
        return None;
    };
    let cb = encoded_cb.checked_sub(32)?;
    let column = u16::from(encoded_column).checked_sub(33)?;
    let row = u16::from(encoded_row).checked_sub(33)?;
    let (kind, modifiers) = parse_mouse_cb(cb)?;

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_sgr_mouse(sequence: &str) -> Option<MouseEvent> {
    let report = parse_sgr_mouse_report(sequence.as_bytes())?;
    let column = u16::try_from(report.x.checked_sub(1)?).ok()?;
    let row = u16::try_from(report.y.checked_sub(1)?).ok()?;
    Some(report.at_cell(column, row))
}

/// One SGR mouse report (`CSI < Cb ; x ; y M/m`) with its raw 1-based
/// coordinates: cells normally, pixels when the host reports SGR pixels (1016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SgrMouseReport {
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    pub(crate) x: u32,
    pub(crate) y: u32,
}

impl SgrMouseReport {
    /// The report as a mouse event at a 0-based cell.
    pub(crate) fn at_cell(self, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: self.kind,
            column,
            row,
            modifiers: self.modifiers,
        }
    }
}

pub(crate) fn parse_sgr_mouse_report(sequence: &[u8]) -> Option<SgrMouseReport> {
    let body = sequence.strip_prefix(b"\x1b[<")?;
    let (&final_byte, payload) = body.split_last()?;
    if final_byte != b'M' && final_byte != b'm' {
        return None;
    }
    let mut parts = payload.split(|byte| *byte == b';');
    let cb = u8::try_from(parse_report_number(parts.next()?)?).ok()?;
    let x = parse_report_number(parts.next()?)?;
    let y = parse_report_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let (kind, modifiers) = parse_mouse_cb(cb)?;
    let kind = match (final_byte, kind) {
        (b'm', MouseEventKind::Down(button)) => MouseEventKind::Up(button),
        (_, kind) => kind,
    };
    Some(SgrMouseReport {
        kind,
        modifiers,
        x,
        y,
    })
}

fn parse_report_number(value: &[u8]) -> Option<u32> {
    (!value.is_empty() && value.iter().all(u8::is_ascii_digit))
        .then(|| std::str::from_utf8(value).ok()?.parse().ok())
        .flatten()
}

fn parse_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;

    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        // Crossterm cannot represent extended-button drags. Preserve their
        // position as motion so a stuck host button cannot suppress hover.
        (3, true) | (4, true) | (5, true) | (8, true) | (9, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };

    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }

    Some((kind, modifiers))
}

#[cfg(test)]
mod hold_property_tests;
#[cfg(test)]
mod review_5251_r1_tests;
#[cfg(test)]
mod review_5251_r2_tests;
#[cfg(test)]
mod review_nxa8_r3_tests;
#[cfg(test)]
mod review_nxa8_r4_tests;
#[cfg(test)]
mod review_nxa8_r5_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind};

    fn assert_raw_key(event: RawInputEvent, code: KeyCode, modifiers: KeyModifiers) {
        let RawInputEvent::Key(key) = event else {
            panic!("expected key");
        };
        assert_eq!(key.code, code);
        assert_eq!(key.modifiers, modifiers);
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        let hex = hex.trim();
        assert_eq!(hex.len() % 2, 0, "hex string must have even length");
        (0..hex.len())
            .step_by(2)
            .map(|idx| u8::from_str_radix(&hex[idx..idx + 2], 16).unwrap())
            .collect()
    }

    fn parse_fixture_key_code(value: &str) -> KeyCode {
        match value {
            "enter" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "esc" => KeyCode::Esc,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            "delete" => KeyCode::Delete,
            value if value.starts_with("char:") => {
                KeyCode::Char(value.trim_start_matches("char:").chars().next().unwrap())
            }
            other => panic!("unsupported fixture key code: {other}"),
        }
    }

    fn parse_fixture_modifiers(value: &str) -> KeyModifiers {
        if value == "-" || value.is_empty() {
            return KeyModifiers::empty();
        }

        let mut modifiers = KeyModifiers::empty();
        for part in value.split('+') {
            match part {
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "alt" => modifiers |= KeyModifiers::ALT,
                "control" => modifiers |= KeyModifiers::CONTROL,
                "super" => modifiers |= KeyModifiers::SUPER,
                "hyper" => modifiers |= KeyModifiers::HYPER,
                "meta" => modifiers |= KeyModifiers::META,
                other => panic!("unsupported fixture modifier: {other}"),
            }
        }
        modifiers
    }

    #[test]
    fn parses_kitty_shift_letter_release() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[108:76;2:3u").unwrap()
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 13);
        assert_eq!(key.code, KeyCode::Char('l'));
        assert_eq!(key.modifiers, KeyModifiers::SHIFT);
        assert_eq!(key.kind, KeyEventKind::Release);
        assert_eq!(key.shifted_codepoint, Some('L' as u32));
    }

    #[test]
    fn parses_bracketed_paste() {
        let (RawInputEvent::Paste(text), consumed) =
            extract_one_event(b"\x1b[200~hello\x1b[201~rest").unwrap()
        else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
        assert_eq!(consumed, 17);
    }

    #[test]
    fn complete_text_bracketed_paste_requires_one_exact_utf8_sequence() {
        assert_eq!(
            complete_text_bracketed_paste(b"\x1b[200~hello\x1b[201~"),
            Some("hello")
        );
        assert!(!is_complete_text_bracketed_paste(b"\x1b[200~hello"));
        assert!(!is_complete_text_bracketed_paste(
            b"\x1b[200~hello\x1b[201~rest"
        ));
        assert!(!is_complete_text_bracketed_paste(
            b"\x1b[200~one\x1b[201~\x1b[200~two\x1b[201~"
        ));
        assert!(!is_complete_text_bracketed_paste(b"\x1b[200~\xff\x1b[201~"));
    }

    #[test]
    fn sgr_mouse_report_parser_accepts_only_complete_reports() {
        for (input, expected) in [
            (b"\x1b[<35;321;241M".as_slice(), Some((321, 241))),
            (b"\x1b[<0;1;2m".as_slice(), Some((1, 2))),
            (b"key".as_slice(), None),
            (b"\x1b[<0;1;2Mkey".as_slice(), None),
            (b"\x1b[<0;1M".as_slice(), None),
            (b"\x1b[<0;1;2;3M".as_slice(), None),
        ] {
            assert_eq!(
                parse_sgr_mouse_report(input).map(|report| (report.x, report.y)),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn parses_sgr_mouse() {
        let (RawInputEvent::Mouse(mouse), consumed) = extract_one_event(b"\x1b[<0;20;10M").unwrap()
        else {
            panic!("expected mouse");
        };
        assert_eq!(consumed, 11);
        assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!(mouse.column, 19);
        assert_eq!(mouse.row, 9);
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parses_default_mouse_encoding() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[MCN1");
        let [RawInputEvent::Mouse(mouse)] = events.as_slice() else {
            panic!("expected one mouse event");
        };
        assert_eq!(mouse.kind, MouseEventKind::Moved);
        assert_eq!((mouse.column, mouse.row), (45, 16));
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn rejected_default_mouse_frame_preserves_trailing_input() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[M\x82AAx");

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RawInputEvent::Unsupported));
        assert_raw_key(
            events.into_iter().nth(1).unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn parses_extended_button_drag_as_mouse_motion() {
        for input in [
            b"\x1b[<160;20;10M".as_slice(),
            b"\x1b[<161;20;10M".as_slice(),
        ] {
            let (RawInputEvent::Mouse(mouse), _) = extract_one_event(input).unwrap() else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.kind, MouseEventKind::Moved);
            assert_eq!((mouse.column, mouse.row), (19, 9));
        }
    }

    #[test]
    fn parses_sgr_mouse_observable_modifiers() {
        let cases = [
            (b"\x1b[<8;20;10M".as_slice(), KeyModifiers::ALT),
            (b"\x1b[<16;20;10M".as_slice(), KeyModifiers::CONTROL),
            (
                b"\x1b[<24;20;10M".as_slice(),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
        ];

        for (input, expected) in cases {
            let (RawInputEvent::Mouse(mouse), _) = extract_one_event(input).unwrap() else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.modifiers, expected);
            assert!(!mouse.modifiers.contains(KeyModifiers::SUPER));
        }
    }

    #[test]
    fn parses_host_default_color_response_with_st() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]10;rgb:cccc/dddd/eeee\x1b\\").unwrap()
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 25);
        assert_eq!(kind, DefaultColorKind::Foreground);
        assert_eq!(
            color,
            RgbColor {
                r: 0xcc,
                g: 0xdd,
                b: 0xee
            }
        );
    }

    #[test]
    fn parses_host_default_color_response_with_bel() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]11;#112233\x07").unwrap()
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 13);
        assert_eq!(kind, DefaultColorKind::Background);
        assert_eq!(
            color,
            RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33
            }
        );
    }

    #[test]
    fn parses_host_palette_color_response() {
        let (RawInputEvent::HostPaletteColors { colors }, consumed) =
            extract_one_event(b"\x1b]4;7;rgb:1111/2222/3333\x1b\\").unwrap()
        else {
            panic!("expected host palette response");
        };
        assert_eq!(consumed, 26);
        assert_eq!(
            colors,
            vec![(
                7,
                RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
            )]
        );
    }

    #[test]
    fn parses_legacy_up_arrow() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[A").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 3);
        assert_eq!(key.code, KeyCode::Up);
    }

    #[test]
    fn parses_outer_focus_events() {
        let (event, consumed) = extract_one_event(b"\x1b[I").unwrap();
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusGained));

        let (event, consumed) = extract_one_event(b"\x1b[O").unwrap();
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusLost));
    }

    #[test]
    fn outer_focus_gained_requests_host_surface_redraw() {
        let events = parse_raw_input_bytes_sync(b"\x1b[I");
        assert!(events_require_host_surface_redraw(&events, true));
        assert!(!events_require_host_surface_redraw(&events, false));

        let events = parse_raw_input_bytes_sync(b"\x1b[O");
        assert!(!events_require_host_surface_redraw(&events, true));
    }

    #[test]
    fn outer_focus_gained_requests_host_mode_refresh() {
        assert!(events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[I")
        ));
        assert!(!events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[O")
        ));
    }

    #[test]
    fn outer_focus_gained_requests_host_appearance_query() {
        let gained = parse_raw_input_bytes_sync(b"\x1b[I");
        let lost = parse_raw_input_bytes_sync(b"\x1b[O");
        let scheme_report = parse_raw_input_bytes_sync(b"\x1b[?997;1n");

        assert!(events_require_host_terminal_appearance_query(&gained));
        assert!(!events_require_host_terminal_appearance_query(&lost));
        assert!(!events_require_host_terminal_appearance_query(
            &scheme_report
        ));
        assert!(events_require_host_terminal_theme_query(&scheme_report));
    }

    #[test]
    fn parses_ghostty_color_scheme_reports() {
        for bytes in [
            GHOSTTY_COLOR_SCHEME_DARK_REPORT,
            GHOSTTY_COLOR_SCHEME_LIGHT_REPORT,
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(
                events[0],
                RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark | HostAppearance::Light)
            ));
            assert!(events_require_host_terminal_theme_query(&events));
        }
    }

    #[test]
    fn ghostty_color_scheme_report_parser_is_exact() {
        for bytes in [
            b"\x1b[?997;0n".as_slice(),
            b"\x1b[?997;3n".as_slice(),
            b"\x1b[?998;1n".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(events[0], RawInputEvent::Unsupported));
            assert!(!events_require_host_terminal_theme_query(&events));
        }
    }

    #[test]
    fn raw_input_framer_reassembles_split_color_scheme_report() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        let events = framer.push(b"1n");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark)
        ));
    }

    #[test]
    fn parses_host_cell_size_report() {
        let events = parse_raw_input_bytes_sync(b"\x1b[6;21;10t");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn host_cell_size_report_parser_is_exact() {
        for bytes in [
            // Zero dimensions carry no usable cell size.
            b"\x1b[6;0;10t".as_slice(),
            b"\x1b[6;21;0t".as_slice(),
            // Missing or extra parameters.
            b"\x1b[6;21t".as_slice(),
            b"\x1b[6;21;10;3t".as_slice(),
            // Other XTWINOPS reports must not be mistaken for a cell size.
            b"\x1b[4;1610;777t".as_slice(),
            b"\x1b[8;37;161t".as_slice(),
            // Non-numeric parameters.
            b"\x1b[6;21;1-t".as_slice(),
        ] {
            assert!(
                parse_host_cell_size_report(bytes).is_none(),
                "bytes: {bytes:?}"
            );
        }
    }

    #[test]
    fn split_color_scheme_timeout_does_not_swallow_legacy_alt_bracket() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_discards_timed_out_split_color_scheme_report_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1n").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn parses_xterm_alt_up_arrow() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[1;3A").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 6);
        assert_eq!(key.code, KeyCode::Up);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_legacy_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b\x7f").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 2);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_kitty_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[127;3u").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_enhanced_pageup_press() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[5;1:1~").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageUp);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Press);
    }

    #[test]
    fn parses_enhanced_pagedown_release() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[6;1:3~").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageDown);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Release);
    }

    #[test]
    fn raw_input_family_matrix_is_covered() {
        let cases: &[(&[u8], KeyCode, KeyModifiers)] = &[
            (b"\x02", KeyCode::Char('b'), KeyModifiers::CONTROL),
            (b"\r", KeyCode::Enter, KeyModifiers::empty()),
            (b"\t", KeyCode::Tab, KeyModifiers::empty()),
            (b"\x7f", KeyCode::Backspace, KeyModifiers::empty()),
            (b"\x1b[A", KeyCode::Up, KeyModifiers::empty()),
            (b"\x1b[1;3A", KeyCode::Up, KeyModifiers::ALT),
            (b"\x1b\x7f", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[127;3u", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[57420;1u", KeyCode::Down, KeyModifiers::empty()),
            (b"\x1b[57423;1u", KeyCode::Home, KeyModifiers::empty()),
            (b"\x1bOq", KeyCode::Char('1'), KeyModifiers::empty()),
            (b"\x1b[14~", KeyCode::F(4), KeyModifiers::empty()),
            (b"\x1b[11;2~", KeyCode::F(1), KeyModifiers::SHIFT),
            (b"\x1b[13;1:1~", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[14;3~", KeyCode::F(4), KeyModifiers::ALT),
            (b"\x1b[57364;1u", KeyCode::F(1), KeyModifiers::empty()),
            (b"\x1b[57366;1u", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[57366;2u", KeyCode::F(3), KeyModifiers::SHIFT),
            (b"\x1b[57375;1u", KeyCode::F(12), KeyModifiers::empty()),
            (b"\x1b[57376;1u", KeyCode::F(13), KeyModifiers::empty()),
            (b"\x1b[49:33;2:1u", KeyCode::Char('1'), KeyModifiers::SHIFT),
        ];

        for (bytes, code, modifiers) in cases {
            let (event, consumed) = extract_one_event(bytes).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_raw_key(event, *code, *modifiers);
        }
    }

    #[test]
    fn raw_framer_waits_for_application_keypad_sequence_final_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1bO").is_empty());
        let events = framer.push(b"q");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn unsupported_ss3_sequence_stays_unsupported() {
        let (event, consumed) = extract_one_event(b"\x1bOz").unwrap();

        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::Unsupported));
    }

    #[test]
    fn parses_modified_rxvt_f_key_alias() {
        let (event, consumed) = extract_one_event(b"\x1b[14;3~").unwrap();

        assert_eq!(consumed, 7);
        assert_raw_key(event, KeyCode::F(4), KeyModifiers::ALT);
    }

    #[test]
    fn flushes_lone_escape_after_timeout() {
        let mut framer = RawInputFramer::default();
        assert!(framer.push(&[ESC]).is_empty());

        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn parses_raw_ctrl_b() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x02").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('b'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parses_raw_lf_as_ctrl_j() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\n").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('j'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    fn assert_fixture_extracts_whole_events(corpus: &str, macos_layout: bool) {
        for line in corpus.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut columns: Vec<_> = line.split('\t').collect();
            if columns.len() == 5 {
                columns.push("");
            }

            if macos_layout {
                if columns.len() == 6 {
                    columns.push("");
                }
                assert_eq!(
                    columns.len(),
                    7,
                    "macOS fixture row must have 7 columns: {line}"
                );
                if columns[2].is_empty() {
                    continue;
                }
                let bytes = decode_hex(columns[2]);
                let (event, consumed) = extract_one_event(&bytes).unwrap();
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "fixture should extract a whole event: {line}"
                );
                assert_raw_key(
                    event,
                    parse_fixture_key_code(columns[3]),
                    parse_fixture_modifiers(columns[4]),
                );
            } else {
                if columns.len() == 5 {
                    columns.push("");
                }
                let (bytes_hex, code, modifiers) = match columns.len() {
                    6 => {
                        if columns[1].chars().all(|ch| ch.is_ascii_hexdigit()) {
                            (columns[1], columns[2], columns[3])
                        } else {
                            (columns[2], columns[3], columns[4])
                        }
                    }
                    7 => (columns[2], columns[3], columns[4]),
                    _ => panic!("fixture row must have 6 or 7 columns: {line}"),
                };
                assert!(
                    bytes_hex.chars().all(|ch| ch.is_ascii_hexdigit()),
                    "non-hex fixture bytes: {bytes_hex} in {line}"
                );
                let bytes = decode_hex(bytes_hex);
                let (event, consumed) = extract_one_event(&bytes).unwrap();
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "fixture should extract a whole event: {line}"
                );
                assert_raw_key(
                    event,
                    parse_fixture_key_code(code),
                    parse_fixture_modifiers(modifiers),
                );
            }
        }
    }

    #[test]
    fn raw_input_corpus_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/keyboard_protocol_corpus.tsv");
        assert_fixture_extracts_whole_events(corpus, false);
    }

    #[test]
    fn raw_input_macos_terminal_variants_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/macos_terminal_variants.tsv");
        assert_fixture_extracts_whole_events(corpus, true);
    }

    #[test]
    fn raw_input_linux_terminal_variants_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/linux_terminal_variants.tsv");
        assert_fixture_extracts_whole_events(corpus, false);
    }

    #[test]
    fn chunked_legacy_arrow_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[A");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Up,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn lone_escape_is_buffered_until_timeout_flush() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_arrow_before_flush_does_not_emit_escape() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[B");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Down,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_sgr_mouse_before_flush_does_not_emit_text() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[<65;43;26M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 42,
                row: 25,
                ..
            })
        ));
    }

    #[test]
    fn lone_escape_then_complete_sgr_mouse_report_emits_both_events() {
        for report in [b"\x1b[<35;10;20M".as_slice(), b"\x1b[<35;10;20m".as_slice()] {
            let mut framer = RawInputFramer::default();

            assert!(framer.push(b"\x1b").is_empty());
            let events = framer.push(report);

            assert_eq!(events.len(), 2);
            let mut events = events.into_iter();
            assert_raw_key(events.next().unwrap(), KeyCode::Esc, KeyModifiers::empty());
            assert!(matches!(
                events.next().unwrap(),
                RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 9,
                    row: 19,
                    ..
                })
            ));
            assert!(framer.flush_timeout().is_empty());
        }
    }

    #[test]
    fn lone_escape_then_default_mouse_report_emits_both_events() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"\x1b[MCN1");

        assert_eq!(events.len(), 2);
        let mut events = events.into_iter();
        assert_raw_key(events.next().unwrap(), KeyCode::Esc, KeyModifiers::empty());
        assert!(matches!(
            events.next().unwrap(),
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 45,
                row: 16,
                ..
            })
        ));
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn legacy_doubled_escape_alt_arrow_remains_one_event() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\x1b[A");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Up,
            KeyModifiers::ALT,
        );
        assert!(framer.flush_timeout().is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_host_input_splits_lone_escape_from_arrow() {
        let mut framer = RawInputByteFramer::for_host_input();

        assert_eq!(
            framer.push(b"\x1b\x1b[D"),
            vec![b"\x1b".to_vec(), b"\x1b[D".to_vec()]
        );
    }

    #[test]
    fn macos_host_input_policy_preserves_legacy_doubled_escape_alt_arrow() {
        let mut framer = RawInputByteFramer::with_host_input_policy(true);

        assert_eq!(framer.push(b"\x1b\x1b[D"), vec![b"\x1b\x1b[D".to_vec()]);
    }

    /// WezTerm with `enable_kitty_keyboard` sends an Escape press as a bare
    /// ESC and its release as a kitty report (#1266). A quick tap lands both in
    /// one read, or in two reads before the idle flush.
    const WEZTERM_ESCAPE_TAP: (&[u8], &[u8]) = (b"\x1b", b"\x1b[27;1:3u");

    fn assert_escape_press_then_release(chunks: Vec<Vec<u8>>, taps: usize) {
        let events = events_from_framed_chunks(chunks);
        assert_eq!(events.len(), taps * 2, "{events:?}");
        for pair in events.chunks(2) {
            assert!(
                matches!(&pair[0], RawInputEvent::Key(key)
                    if key.code == KeyCode::Esc
                        && key.modifiers.is_empty()
                        && key.kind == KeyEventKind::Press),
                "{events:?}"
            );
            assert!(
                matches!(&pair[1], RawInputEvent::Key(key)
                    if key.code == KeyCode::Esc
                        && key.modifiers.is_empty()
                        && key.kind == KeyEventKind::Release),
                "{events:?}"
            );
        }
    }

    #[test]
    fn preserved_doubled_escape_policy_splits_escape_press_from_kitty_release() {
        let (press, release) = WEZTERM_ESCAPE_TAP;
        let tap = [press, release].concat();

        // One read.
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        let mut chunks = framer.push(&tap);
        chunks.extend(framer.flush_timeout());
        assert_eq!(chunks, vec![press.to_vec(), release.to_vec()]);
        assert_escape_press_then_release(chunks, 1);

        // Two reads before the idle flush.
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        let mut chunks = framer.push(press);
        chunks.extend(framer.push(release));
        chunks.extend(framer.flush_timeout());
        assert_escape_press_then_release(chunks, 1);

        // Two quick taps (double Esc) in one read.
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        let mut chunks = framer.push(&[tap.clone(), tap].concat());
        chunks.extend(framer.flush_timeout());
        assert_escape_press_then_release(chunks, 2);

        // The release split across reads still separates once it completes.
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        let mut chunks = framer.push(b"\x1b\x1b[27;");
        chunks.extend(framer.push(b"1:3u"));
        chunks.extend(framer.flush_timeout());
        assert_escape_press_then_release(chunks, 1);
    }

    #[test]
    fn preserved_doubled_escape_policy_splits_the_tap_at_every_read_boundary() {
        let (press, release) = WEZTERM_ESCAPE_TAP;
        let tap = [press, release].concat();
        for taps in [tap.clone(), [tap.clone(), tap.clone()].concat()] {
            let count = taps.len() / tap.len();
            for split in 1..taps.len() {
                let mut framer = RawInputByteFramer::with_host_input_policy(true);
                let mut chunks = framer.push(&taps[..split]);
                chunks.extend(framer.push(&taps[split..]));
                chunks.extend(framer.flush_timeout());
                assert_escape_press_then_release(chunks, count);
            }
        }
    }

    #[test]
    fn preserved_doubled_escape_policy_splits_escape_before_alt_kitty_report() {
        // Escape, then Alt+a reported by a kitty host: two keys, not Alt+Alt+a.
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        let mut chunks = framer.push(b"\x1b\x1b[97;3u");
        chunks.extend(framer.flush_timeout());
        assert_eq!(chunks, vec![b"\x1b".to_vec(), b"\x1b[97;3u".to_vec()]);
        let events = events_from_framed_chunks(chunks);
        assert!(
            matches!(&events[..], [RawInputEvent::Key(esc), RawInputEvent::Key(alt_a)]
                if esc.code == KeyCode::Esc && esc.modifiers.is_empty()
                    && alt_a.code == KeyCode::Char('a') && alt_a.modifiers == KeyModifiers::ALT),
            "{events:?}"
        );
    }

    #[test]
    fn preserved_doubled_escape_policy_keeps_legacy_alt_sequences_whole() {
        for bytes in [
            &b"\x1b\x1b[D"[..],
            b"\x1b\x1b[1;3D",
            b"\x1b\x1b[3~",
            b"\x1b\x1bOA",
        ] {
            let mut framer = RawInputByteFramer::with_host_input_policy(true);
            let mut chunks = framer.push(bytes);
            chunks.extend(framer.flush_timeout());
            assert_eq!(chunks, vec![bytes.to_vec()], "{bytes:?}");
        }
        for bytes in [
            &b"\x1b[u"[..],
            b"\x1b[?7u",
            b"\x1b[;3u",
            b"\x1b[27;1:3",
            b"\x1b[27;1:3~",
            b"\x1b[D",
            b"\x1bOA",
        ] {
            assert!(!starts_with_kitty_key_report(bytes), "{bytes:?}");
        }
        for bytes in [&b"\x1b[27u"[..], b"\x1b[27;1:3u", b"\x1b[55:47;2;47u"] {
            assert!(starts_with_kitty_key_report(bytes), "{bytes:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_host_input_delivers_quick_wezterm_escape_taps() {
        let (press, release) = WEZTERM_ESCAPE_TAP;
        let mut framer = RawInputByteFramer::for_host_input();
        let mut chunks = framer.push(&[press, release].concat());
        chunks.extend(framer.flush_timeout());
        assert_escape_press_then_release(chunks, 1);
        // Legacy Alt+arrow keeps its doubled escape on macOS.
        assert_eq!(framer.push(b"\x1b\x1b[D"), vec![b"\x1b\x1b[D".to_vec()]);
    }

    #[test]
    fn sgr_mouse_sequence_split_after_button_prefix_is_reassembled_before_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        let events = framer.push(b"5;58;30M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 57,
                row: 29,
                ..
            })
        ));
    }

    #[test]
    fn timed_out_split_sgr_mouse_tail_is_discarded_and_following_input_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.idle_tick().is_empty());
        let events = framer.push(b"5;58;30Mx");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn captured_sgr_mouse_tail_after_second_idle_flush_is_discarded() {
        let mut framer = RawInputByteFramer::for_host_input();

        // Issue #3911, 2026-09-13 07:02:14 UTC: this prefix timed out,
        // then its tail arrived 33 ms later. The Unix reader flushes again
        // after 10 ms of continued idle following the first discard.
        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"5;28;31M"), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_completion_is_preserved_after_idle() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"M"), vec![b"M".to_vec()]);
    }

    #[test]
    fn timed_out_sgr_mouse_completion_survives_read_splits_and_idle() {
        let tail = b"5;28;31M";
        for split in 0..=tail.len() {
            let mut framer = RawInputByteFramer::for_host_input();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(&tail[..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut rest = tail[split..].to_vec();
            rest.extend_from_slice(b"x\x1b[A");
            assert_eq!(framer.push(&rest), vec![b"x".to_vec(), b"\x1b[A".to_vec()]);
            assert!(framer.tail_hold.is_none());
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_syntax_releases_continuation() {
        for tail in [
            b"5;0;31M".as_slice(), // zero coordinate
            b"5;;31M",             // empty field
            b"5;28;31;1M",         // extra field (the general parser is permissive)
            b"999;28;31M",         // button overflow
            b"5;65536;31M",        // coordinate overflow
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.push(tail).concat(), tail);
            assert!(framer.tail_hold.is_none());
        }
    }

    #[test]
    fn timed_out_sgr_mouse_interruption_preserves_text_and_new_events() {
        for suffix in [
            b"x".as_slice(),
            b"\x1b[A",                  // new key sequence
            b"\x1b[200~paste\x1b[201~", // bracketed paste
            "\u{4f60}".as_bytes(),      // UTF-8 split across reads
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(b"5;28;").is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut chunks = Vec::new();
            for byte in suffix {
                chunks.extend(framer.push(&[*byte]));
            }
            let mut expected = b"5;28;".to_vec();
            expected.extend_from_slice(suffix);
            assert_eq!(chunks.concat(), expected);
            assert!(framer.tail_hold.is_none());
            assert_eq!(framer.push(b"123M").concat(), b"123M");
        }
    }

    #[test]
    fn timed_out_sgr_mouse_budget_includes_prefix_and_preserves_overflow() {
        let prefix = b"\x1b[<35;1;";
        let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES - prefix.len();
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut valid_tail = vec![b'0'; remaining - 2];
        valid_tail.extend_from_slice(b"1M");
        assert!(framer.push(&valid_tail).is_empty()); // complete exactly at limit
        assert!(framer.tail_hold.is_none());

        for length in [remaining, remaining + 1024] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            assert!(framer.flush_timeout().is_empty());
            let tail = vec![b'0'; length];
            assert_eq!(framer.push(&tail).concat(), tail);
            assert!(framer.tail_hold.is_none());
            assert_eq!(framer.push(b"1Mtext").concat(), b"1Mtext");
        }

        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut tail = vec![b'0'; remaining - 1];
        assert!(framer.push(&tail).is_empty());
        assert!(framer.flush_timeout().is_empty());
        tail.extend_from_slice(b"01M");
        assert_eq!(framer.push(b"01M").concat(), tail);
        assert!(framer.tail_hold.is_none());
    }

    #[test]
    fn sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let timeout_events = framer.flush_timeout();
        assert_eq!(timeout_events.len(), 1);
        assert_raw_key(
            timeout_events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );

        assert!(framer.push(b"[<65;43;26M").is_empty());
    }

    #[test]
    fn input_after_discarded_complete_sgr_mouse_tail_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);
        let events = framer.push(b"[<65;43;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn invalid_orphaned_sgr_mouse_tail_after_escape_timeout_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        let events = framer.push(b"[<x");

        assert_eq!(events.len(), 3);
        assert_raw_key(
            events.into_iter().last().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn double_split_sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        assert!(framer.push(b"[<65;4").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"3;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn three_fragment_sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        // #5251: the orphaned tail split again after its bracket. The bare `[`
        // must not be released as a key while the report can still complete.
        assert!(framer.push(b"[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());

        let events = framer.push(b"<35;100;30Mx");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
        assert!(!framer.byte_framer.has_pending_input());
    }

    #[test]
    fn orphaned_sgr_mouse_tail_survives_every_read_split() {
        let tail = b"[<35;100;30M";
        for split in 0..=tail.len() {
            let mut framer = RawInputFramer::default();
            assert!(framer.push(b"\x1b").is_empty());
            assert_eq!(framer.flush_timeout().len(), 1, "split {split}");

            let mut events = Vec::new();
            for byte in &tail[..split] {
                events.extend(framer.push(&[*byte]));
                events.extend(framer.flush_timeout());
                events.extend(framer.flush_timeout());
            }
            events.extend(framer.push(&tail[split..]));
            events.extend(framer.flush_timeout());
            events.extend(framer.flush_timeout());
            events.extend(framer.push(b"x"));
            events.extend(framer.flush_timeout());

            assert_eq!(events.len(), 1, "split {split}: {events:?}");
            assert_raw_key(
                events.into_iter().next().unwrap(),
                KeyCode::Char('x'),
                KeyModifiers::empty(),
            );
            assert!(!framer.byte_framer.has_pending_input(), "split {split}");
        }
    }

    #[test]
    fn orphaned_bracket_prefix_releases_ordinary_input() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        // A bracket that does not become a mouse report is still user input,
        // not something to drop on the next read.
        assert!(framer.push(b"[").is_empty());
        assert!(framer.flush_timeout().is_empty());

        let events = framer.push(b"abc");
        let codes: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                RawInputEvent::Key(key) => Some(key.code),
                _ => None,
            })
            .collect();
        assert_eq!(
            codes,
            vec![
                KeyCode::Char('['),
                KeyCode::Char('a'),
                KeyCode::Char('b'),
                KeyCode::Char('c')
            ]
        );
    }

    #[test]
    fn retained_bracket_is_released_when_semantic_input_interrupts() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);
        assert!(framer.push(b"[").is_empty());
        assert!(framer.flush_timeout().is_empty());

        // Windows flushes before a semantic key record; the bracket was input.
        let events = framer.flush_interrupted();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('['),
            KeyModifiers::empty(),
        );
        assert!(!framer.byte_framer.has_pending_input());
    }

    #[test]
    fn retained_bracket_is_released_after_its_hold_limit() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![vec![ESC]]);
        assert!(framer.push(b"[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());

        // The caller must keep scheduling a flush for the held bracket.
        assert!(framer.has_pending_input());
        let wait = framer.held_input_release_ms().expect("bounded hold");
        assert!(
            (1..=REPORT_TAIL_LIMIT.as_millis() as i32 + 1).contains(&wait),
            "{wait}"
        );

        framer.age_held_input(REPORT_TAIL_LIMIT);
        assert_eq!(framer.flush_timeout(), vec![b"[".to_vec()]);
        assert!(!framer.has_pending_input());
        assert_eq!(framer.held_input_release_ms(), None);
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn retained_mouse_prefix_stops_capturing_input_after_its_hold_limit() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[<35;10").is_empty());
        assert!(framer.flush_timeout().is_empty());
        // The dropped prefix is not pending input; only its hold is open.
        assert!(!framer.has_pending_input());
        assert!(framer.tail_hold.is_some());

        framer.age_held_input(REPORT_TAIL_LIMIT);
        assert!(framer.flush_timeout().is_empty());
        assert!(!framer.has_pending_input());
        // Digits typed later are not taken for the old report's tail.
        assert_eq!(framer.push(b"5"), vec![b"5".to_vec()]);
    }

    #[test]
    fn sgr_mouse_tail_after_timed_out_csi_introducer_is_discarded() {
        let mut framer = RawInputFramer::default();

        // #5251: with mouse capture on, `ESC [` idles out after 50 ms and is
        // released as Alt+[ before the rest of the report arrives.
        assert!(framer.push(b"\x1b[").is_empty());
        let events = framer.flush_timeout();
        assert!(
            matches!(&events[..], [RawInputEvent::Key(key)]
                if key.code == KeyCode::Char('[') && key.modifiers == KeyModifiers::ALT),
            "{events:?}"
        );

        let events = framer.push(b"<35;100;30Mx");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
        assert!(!framer.byte_framer.has_pending_input());
    }

    #[test]
    fn sgr_mouse_tail_after_timed_out_csi_introducer_survives_every_read_split() {
        let tail = b"<35;100;30M";
        for split in 0..=tail.len() {
            let mut framer = RawInputFramer::default();
            assert!(framer.push(b"\x1b[").is_empty());
            assert_eq!(framer.flush_timeout().len(), 1, "split {split}");

            let mut events = Vec::new();
            events.extend(framer.push(&tail[..split]));
            events.extend(framer.flush_timeout());
            events.extend(framer.flush_timeout());
            events.extend(framer.push(&tail[split..]));
            events.extend(framer.push(b"x"));

            assert_eq!(events.len(), 1, "split {split}: {events:?}");
            assert_raw_key(
                events.into_iter().next().unwrap(),
                KeyCode::Char('x'),
                KeyModifiers::empty(),
            );
            assert!(!framer.byte_framer.has_pending_input(), "split {split}");
        }
    }

    #[test]
    fn text_after_timed_out_csi_introducer_is_preserved() {
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert_eq!(framer.push(b"<x"), vec![b"<".to_vec(), b"x".to_vec()]);

        // Past the split-read window a `<` is just typing.
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        framer.age_held_input(REPORT_TAIL_LIMIT);
        assert_eq!(framer.push(b"<"), vec![b"<".to_vec()]);
    }

    #[test]
    fn escape_followed_by_alt_char_before_flush_becomes_alt_key() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"b");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_kitty_sequence_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[49:33;2:").is_empty());
        let events = framer.push(b"1u");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::SHIFT,
        );
    }

    #[test]
    fn chunked_bracketed_paste_waits_for_terminator() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello").is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn incomplete_bracketed_paste_is_not_flushed_on_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello\nworld").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_is_drained() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert_eq!(framer.push(&[]), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn incomplete_utf8_prefix_is_not_flushed_on_timeout() {
        let mut framer = RawInputByteFramer::default();
        let prefix = &"好".as_bytes()[..1];

        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn invalid_utf8_lead_byte_is_flushed_instead_of_buffered_forever() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(&[0xC0]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_survives_timeout_and_next_chunk() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn alt_utf8_char_drains_as_one_event_before_following_input() {
        let events = parse_raw_input_bytes_sync("\x1béx".as_bytes());
        assert_eq!(events.len(), 2);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_alt_utf8_waits_for_continuation_byte_after_escape() {
        let mut framer = RawInputFramer::default();
        let bytes = "\x1bé".as_bytes();

        assert!(framer.push(&bytes[..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_utf8_waits_for_continuation_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(&"é".as_bytes()[..1]).is_empty());
        let events = framer.push(&"é".as_bytes()[1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_cjk_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "好".as_bytes();

        assert!(framer.push(&bytes[..1]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&bytes[1..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('好'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_four_byte_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "🙂".as_bytes();

        for split in 1..bytes.len() {
            assert!(framer.push(&bytes[split - 1..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
        }

        let events = framer.push(&bytes[bytes.len() - 1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('🙂'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn long_multilingual_voice_like_burst_drains_without_truncation() {
        let text = "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.🙂".repeat(128);
        assert!(
            text.len() > 4096,
            "test input should exceed the client read buffer"
        );
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(text.as_bytes());
        let rebuilt: Vec<u8> = chunks.into_iter().flatten().collect();

        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn long_multilingual_burst_survives_one_byte_chunks_and_timeouts() {
        let text = "中文かなカナ한글🙂，。".repeat(64);
        let mut framer = RawInputByteFramer::default();
        let mut rebuilt = Vec::new();

        for byte in text.as_bytes() {
            rebuilt.extend(
                framer
                    .push(std::slice::from_ref(byte))
                    .into_iter()
                    .flatten(),
            );
            if framer.has_pending_input() {
                assert!(framer.flush_timeout().is_empty());
            }
        }

        rebuilt.extend(framer.flush_timeout().into_iter().flatten());
        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn parses_ghostty_default_background_response() {
        let events = parse_raw_input_bytes_sync(b"\x1b]11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x28,
                    g: 0x2a,
                    b: 0x36
                }
            }
        ));
    }

    #[test]
    fn raw_input_framer_reassembles_split_default_background_response() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        let events = framer.push(b"11;#123456\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56,
                }
            }
        ));
    }

    #[test]
    fn raw_input_byte_framer_discards_split_control_string_after_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"11;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_keeps_discarding_tail_across_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_releases_discard_on_implausible_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        // `x` and `y` are outside the OSC color-reply alphabet, so they are
        // input: delivered at once, not swallowed with the dropped header.
        assert_eq!(framer.push(b"x"), vec![b"x".to_vec()]);
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"y"), vec![b"y".to_vec()]);
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn typed_hex_letters_after_discarded_osc_header_reach_the_pane() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        // `a` and `b` could be a color reply payload, so they are held as a
        // bounded candidate, not discarded.
        assert!(framer.push(b"a").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"b").is_empty());
        assert!(framer.has_pending_input());
        let wait = framer.held_input_release_ms().expect("bounded hold");
        assert!(
            (1..=COLOR_REPLY_TAIL_LIMIT.as_millis() as i32 + 1).contains(&wait),
            "{wait}"
        );

        framer.age_held_input(COLOR_REPLY_TAIL_LIMIT);
        assert_eq!(framer.flush_timeout(), vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(!framer.has_pending_input());
        assert_eq!(framer.held_input_release_ms(), None);
        assert_eq!(framer.push(b"c"), vec![b"c".to_vec()]);
    }

    #[test]
    fn typed_hex_letters_after_discarded_osc_header_end_at_next_foreign_byte() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"a").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"bx"),
            vec![b"a".to_vec(), b"b".to_vec(), b"x".to_vec()]
        );
    }

    #[test]
    fn bracketed_paste_after_discarded_osc_header_is_delivered() {
        for held in [b"".as_slice(), b"ab"] {
            let mut framer = RawInputFramer::default();

            assert!(framer.push(b"\x1b]").is_empty());
            assert!(framer.idle_tick().is_empty());
            let mut events = framer.push(held);
            events.extend(framer.idle_tick());
            events.extend(framer.push(b"\x1b[200~hello world\x1b[201~"));

            let pasted: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    RawInputEvent::Paste(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(pasted, vec!["hello world"], "{held:?}: {events:?}");
            assert_eq!(events.len(), 1 + held.len(), "{held:?}: {events:?}");
            assert!(!framer.byte_framer.has_pending_input());
        }
    }

    #[test]
    fn terminator_typed_after_discarded_osc_bound_reaches_the_pane() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut framer = RawInputByteFramer::default();

            assert!(framer.push(b"\x1b]").is_empty());
            assert!(framer.flush_timeout().is_empty());
            framer.age_held_input(COLOR_REPLY_TAIL_LIMIT);
            let mut typed = framer.push(terminator);
            typed.extend(framer.flush_timeout());
            assert_eq!(typed.concat(), terminator.to_vec(), "{terminator:?}");
        }
    }

    #[test]
    fn discarded_osc_tail_is_dropped_only_through_its_terminator() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut framer = RawInputByteFramer::default();

            assert!(framer.push(b"\x1b]4;50;").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(b"rgb:afaf/").is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut tail = b"d7d7/8787".to_vec();
            tail.extend_from_slice(terminator);
            tail.push(b'x');
            assert_eq!(framer.push(&tail), vec![b"x".to_vec()], "{terminator:?}");
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn preserved_doubled_escape_is_alt_escape_after_a_short_wait() {
        // macOS keeps legacy ESC ESC whole: Alt+Escape (and Ctrl+Alt+3).
        let mut framer = RawInputByteFramer::with_host_input_policy(true);
        assert!(framer.push(b"\x1b\x1b").is_empty());
        assert_eq!(
            framer.idle_flush_timeout_ms(false),
            RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );
        let chunks = framer.flush_timeout();
        assert_eq!(chunks, vec![b"\x1b\x1b".to_vec()]);
        let events = events_from_framed_chunks(chunks);
        assert!(
            matches!(&events[..], [RawInputEvent::Key(key)]
                if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::ALT),
            "{events:?}"
        );
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        // A longer Alt sequence still joins up within the window.
        assert!(framer.push(b"\x1b\x1b").is_empty());
        assert_eq!(framer.push(b"[A"), vec![b"\x1b\x1b[A".to_vec()]);
    }

    #[test]
    fn csi_and_ss3_introducers_without_expected_reply_are_alt_keys() {
        for (bytes, code) in [
            (&b"\x1b["[..], KeyCode::Char('[')),
            (b"\x1bO", KeyCode::Char('O')),
        ] {
            let mut framer = RawInputFramer::default();
            assert!(framer.push(bytes).is_empty());
            let events = framer.flush_timeout();
            assert!(
                matches!(&events[..], [RawInputEvent::Key(key)]
                    if key.code == code && key.modifiers.contains(KeyModifiers::ALT)),
                "{bytes:?}: {events:?}"
            );
            // The next keystroke is not swallowed as a sequence tail.
            assert_eq!(framer.push(b"a").len(), 1, "{bytes:?}");
        }
    }

    #[test]
    fn parse_raw_input_bytes_sync_does_not_parse_incomplete_strings_as_alt_keys() {
        for bytes in [
            b"\x1b]".as_slice(),
            b"\x1bP".as_slice(),
            b"\x1b_".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1bX".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);

            assert!(events.is_empty(), "parsed {bytes:?} as {events:?}");
        }
    }

    #[test]
    fn non_osc_control_strings_ignore_bel_and_complete_at_st() {
        let bytes = b"\x1bPabc\x07def\x1b\\x";

        let (event, consumed) = extract_one_event(bytes).unwrap();

        assert!(matches!(event, RawInputEvent::Unsupported));
        assert_eq!(consumed, b"\x1bPabc\x07def\x1b\\".len());
    }

    #[test]
    fn non_osc_default_color_text_remains_key_input() {
        let events = parse_raw_input_bytes_sync(b"11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 22);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn byte_framer_does_not_hold_non_osc_default_color_text() {
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(b"11;rgb:2828");

        assert_eq!(chunks.len(), 11);
        assert!(framer.flush_timeout().is_empty());
    }

    #[cfg(any(windows, test))]
    #[test]
    fn windows_host_color_prefix_respects_query_deadline() {
        let mut framer = RawInputFramer::for_host_input();
        framer.host_default_color_query_sent(Duration::from_secs(1));
        assert!(framer.push(b"\x1b]10").is_empty());
        for _ in 0..3 {
            assert!(framer.idle_tick().is_empty());
        }
        assert!(framer.push(b";rgb:aaaa/").is_empty());
        for _ in 0..3 {
            assert!(framer.idle_tick().is_empty());
        }
        assert!(matches!(
            framer.push(b"bbbb/cccc\x1b\\").as_slice(),
            [RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                ..
            }]
        ));

        let mut expired = RawInputFramer::for_host_input();
        expired.host_default_color_query_sent(Duration::ZERO);
        assert!(expired.push(b"\x1b]11;").is_empty());
        assert!(expired.idle_tick().is_empty());
        assert!(matches!(
            expired.push(b"x").as_slice(),
            [RawInputEvent::Key(key)] if key.code == KeyCode::Char('x')
        ));
        assert!(matches!(
            expired.push(b"\x1b]11;rgb:1111/2222/3333\x1b\\").as_slice(),
            [RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                ..
            }]
        ));
    }

    #[cfg(any(windows, test))]
    #[test]
    fn windows_expired_host_color_reply_tail_does_not_reach_pane() {
        let mut framer = RawInputFramer::for_host_input();
        framer.host_default_color_query_sent(Duration::ZERO);
        assert!(framer.push(b"\x1b]11;rgb:1111/").is_empty());
        assert!(framer.idle_tick().is_empty());
        assert!(framer.push(b"2222/").is_empty());
        for _ in 0..3 {
            assert!(framer.idle_tick().is_empty());
        }
        assert!(matches!(
            framer.push(b"3333\x1b\\x").as_slice(),
            [RawInputEvent::Key(key)] if key.code == KeyCode::Char('x')
        ));

        let mut expired = RawInputFramer::for_host_input();
        expired.host_default_color_query_sent(Duration::ZERO);
        assert!(expired.push(b"\x1b]10;rgb:aaaa/").is_empty());
        assert!(expired.idle_tick().is_empty());
        expired.byte_framer.host_color_tail_deadline = Some(Instant::now());
        assert!(expired.idle_tick().is_empty());
        assert!(matches!(
            expired.push(b"y").as_slice(),
            [RawInputEvent::Key(key)] if key.code == KeyCode::Char('y')
        ));

        let mut raw_keyboard = RawInputFramer::for_host_input();
        raw_keyboard.host_default_color_query_sent(Duration::ZERO);
        assert!(raw_keyboard.push(b"\x1b]11;rgb:1111/").is_empty());
        assert!(raw_keyboard.idle_tick().is_empty());
        assert!(raw_keyboard.push(b"a").is_empty());
        raw_keyboard.byte_framer.host_color_tail_deadline = Some(Instant::now());
        assert!(matches!(
            raw_keyboard.idle_tick().as_slice(),
            [RawInputEvent::Key(key)] if key.code == KeyCode::Char('a')
        ));

        let mut split_st = RawInputFramer::for_host_input();
        split_st.host_default_color_query_sent(Duration::ZERO);
        assert!(split_st.push(b"\x1b]11;rgb:1111/2222/3333\x1b").is_empty());
        assert!(split_st.idle_tick().is_empty());
        assert!(matches!(
            split_st.push(b"\\x").as_slice(),
            [RawInputEvent::Key(key)] if key.code == KeyCode::Char('x')
        ));

        let mut abandoned_st = RawInputFramer::for_host_input();
        abandoned_st.host_default_color_query_sent(Duration::ZERO);
        assert!(abandoned_st
            .push(b"\x1b]11;rgb:1111/2222/3333\x1b")
            .is_empty());
        assert!(abandoned_st.idle_tick().is_empty());
        assert!(matches!(
            abandoned_st
                .push(b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\")
                .as_slice(),
            [RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                ..
            }]
        ));

        let mut next_reply = RawInputFramer::for_host_input();
        next_reply.host_default_color_query_sent(Duration::ZERO);
        assert!(next_reply.push(b"\x1b]11;rgb:1111/").is_empty());
        assert!(next_reply.idle_tick().is_empty());
        assert!(next_reply.push(b"\x1b").is_empty());
        assert!(matches!(
            next_reply.push(b"]10;rgb:aaaa/bbbb/cccc\x1b\\").as_slice(),
            [RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                ..
            }]
        ));
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_color_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        // The reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        // The idle flush must not release the ESC as an Escape key while a host
        // color reply is still outstanding.
        assert!(framer.flush_timeout().is_empty());

        // The rest of the OSC 11 reply arrives and stitches back together
        // instead of leaking its payload into the focused pane.
        let chunks = framer.push(b"]11;rgb:2424/2727/3a3a\x1b\\");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                ..
            }
        ));
    }

    fn decoded(chunks: &[Vec<u8>]) -> String {
        let events: Vec<_> = chunks
            .iter()
            .flat_map(|chunk| parse_raw_input_bytes_sync(chunk))
            .collect();
        format!("{events:?}")
    }

    fn flush_pending(framer: &mut RawInputByteFramer, chunks: &mut Vec<Vec<u8>>) {
        for _ in 0..3 {
            if !framer.has_pending_input() {
                break;
            }
            chunks.extend(framer.flush_timeout());
        }
    }

    #[test]
    fn truncated_host_color_reply_does_not_hold_later_input() {
        // #5052: the link cut an OSC 11 reply and nothing else arrived.
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b]11;rgb:1e1e/1e").is_empty());
        // Like the client loop: idle flushes, then a blocking read.
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        framer.age_partial_host_color_reply(HOST_COLOR_REPLY_IDLE_LIMIT);

        assert_eq!(framer.push(b"echo ok\r").concat(), b"echo ok\r");
        assert!(!framer.has_pending_input());
        assert_eq!(framer.push(b"ls").concat(), b"ls");
    }

    #[test]
    fn idle_flush_drops_host_color_reply_after_idle_limit() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b]11;rgb:1e1e/1e").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.has_pending_input());

        framer.age_partial_host_color_reply(HOST_COLOR_REPLY_IDLE_LIMIT);
        assert!(framer.flush_timeout().is_empty());
        assert!(!framer.has_pending_input());
        // The next read is input, not a reply tail to discard.
        assert_eq!(framer.push(b"echo").concat(), b"echo");
    }

    #[test]
    fn complete_unsupported_default_color_osc_is_consumed() {
        let reply = b"\x1b]11;rgbi:0.5/0.5/0.5\x07";
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut input = reply.to_vec();
        input.push(b'x');
        assert_eq!(framer.push(&input), vec![reply.to_vec(), b"x".to_vec()]);
        assert!(matches!(
            extract_one_event(reply),
            Some((RawInputEvent::Unsupported, _))
        ));
    }

    #[test]
    fn host_color_reply_split_within_idle_limit_assembles() {
        let reply = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\";
        let gap = HOST_COLOR_REPLY_IDLE_LIMIT - Duration::from_millis(100);
        for cut in 1..reply.len() {
            let mut framer = RawInputByteFramer::default();
            framer.host_color_query_sent();
            let mut chunks = framer.push(&reply[..cut]);
            // Once the OSC 10/11 prefix is in, idle flushes keep holding it.
            if cut >= 5 {
                chunks.extend(framer.flush_timeout());
                chunks.extend(framer.flush_timeout());
                framer.age_partial_host_color_reply(gap);
            }
            chunks.extend(framer.push(&reply[cut..]));
            assert_eq!(chunks, vec![reply.to_vec()], "cut at {cut}");
        }
    }

    #[test]
    fn unsolicited_host_color_reply_split_in_three_reads_assembles() {
        let pieces: [&[u8]; 3] = [b"\x1b]11;rgb:1e1e/", b"1e1e/", b"2e2e\x07"];
        let gap = HOST_COLOR_REPLY_IDLE_LIMIT - Duration::from_millis(100);
        let mut framer = RawInputByteFramer::default();
        let mut chunks = Vec::new();
        for piece in pieces {
            chunks.extend(framer.push(piece));
            chunks.extend(framer.flush_timeout());
            framer.age_partial_host_color_reply(gap);
        }
        assert_eq!(chunks, vec![pieces.concat()]);
    }

    #[test]
    fn palette_reply_fragments_after_discarded_header_do_not_leak() {
        // #5302: the OSC 4 header is dropped once it idles out, and the reply
        // fragments that follow must be discarded with it, not typed.
        let pieces: [&[u8]; 5] = [b"\x1b]4;50;", b"rgb:afaf/", b"d7d7/", b"8787", b"\x1b\\"];
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut chunks = Vec::new();
        for piece in pieces {
            chunks.extend(framer.push(piece));
            chunks.extend(framer.flush_timeout());
            chunks.extend(framer.flush_timeout());
        }
        chunks.extend(framer.push(b"x"));

        let events = events_from_framed_chunks(chunks);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn palette_payload_bytes_after_discarded_header_do_not_leak() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut chunks = framer.push(b"\x1b]4;50;");
        chunks.extend(framer.flush_timeout());

        for byte in b"rgb:afaf/d7d7/8787\x1b\\" {
            chunks.extend(framer.push(&[*byte]));
            chunks.extend(framer.flush_timeout());
        }
        chunks.extend(framer.push(b"x"));

        let events = events_from_framed_chunks(chunks);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn input_after_dropped_host_color_reply_is_intact_at_every_split() {
        let follow_ups: [&[u8]; 3] = [
            b"\x1b[200~hello\r\x1b[201~",
            b"\x1b[A",
            b"\x1b]4;1;rgb:cdcd/0000/0000\x1b\\",
        ];
        for head in [
            b"\x1b]11;rgb:1e1e/1e".as_slice(),
            b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b",
        ] {
            for follow in follow_ups {
                let expected = decoded(&[follow.to_vec()]);
                for cut in 0..follow.len() {
                    let mut framer = RawInputByteFramer::default();
                    framer.host_color_query_sent();
                    assert!(framer.push(head).is_empty());
                    assert!(framer.flush_timeout().is_empty());
                    framer.age_partial_host_color_reply(HOST_COLOR_REPLY_IDLE_LIMIT);
                    let mut chunks = framer.push(&follow[..cut]);
                    chunks.extend(framer.push(&follow[cut..]));
                    flush_pending(&mut framer, &mut chunks);
                    assert_eq!(
                        decoded(&chunks),
                        expected,
                        "head {head:?} follow {follow:?} cut {cut}"
                    );
                }
            }
        }
    }

    #[test]
    fn unfinished_host_color_reply_is_bounded_across_pushes() {
        let prefix = b"\x1b]11;rgb:";
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(prefix).is_empty());
        let mut chunks = Vec::new();
        for _ in 0..200 {
            chunks.extend(framer.push(b"1"));
            assert!(framer.buffer.len() <= MAX_DISCARDED_CONTROL_TAIL_BYTES);
        }
        // Once the buffer reaches the bound it is dropped; later bytes are input.
        let held = MAX_DISCARDED_CONTROL_TAIL_BYTES - prefix.len();
        assert_eq!(chunks.concat(), vec![b'1'; 200 - held]);
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_cell_size_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        // The XTWINOPS reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());

        let chunks = framer.push(b"[6;21;10t");
        assert_eq!(chunks, vec![b"\x1b[6;21;10t".to_vec()]);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn timed_out_host_cell_size_reply_fragments_do_not_leak() {
        for (prefix, tail) in [
            (b"\x1b[6".as_slice(), b";21;10t".as_slice()),
            (b"\x1b[6;".as_slice(), b"21;10t".as_slice()),
            (b"\x1b[6;21;".as_slice(), b"10t".as_slice()),
        ] {
            let mut framer = RawInputByteFramer::default();
            framer.host_cell_size_query_sent();

            assert!(framer.push(prefix).is_empty(), "prefix: {prefix:?}");
            assert!(framer.flush_timeout().is_empty(), "prefix: {prefix:?}");
            assert!(framer.push(tail).is_empty(), "tail: {tail:?}");
            assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        }
    }

    #[test]
    fn split_host_cell_size_reply_after_csi_intro_gets_one_more_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"6;21;10t"), vec![b"\x1b[6;21;10t".to_vec()]);

        let mut alt_bracket = RawInputByteFramer::default();
        alt_bracket.host_cell_size_query_sent();
        assert!(alt_bracket.push(b"\x1b[").is_empty());
        assert!(alt_bracket.flush_timeout().is_empty());
        assert_eq!(alt_bracket.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn unanswered_host_query_stops_holding_alt_bracket() {
        // Many hosts answer only part of the 256-entry palette query, or no
        // cell size query at all. Once nothing arrives for a while, ESC[ is a
        // key again: a short wait, released alone, next key not swallowed.
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.host_cell_size_query_sent();
        framer.age_host_reply_activity(HOST_REPLY_QUIET_WINDOW);

        assert!(framer.push(b"\x1b[").is_empty());
        assert_eq!(
            framer.idle_flush_timeout_ms(false),
            RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn host_reply_burst_keeps_split_protection_alive() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.age_host_reply_activity(HOST_REPLY_QUIET_WINDOW);
        // A reply arriving late still refreshes the window for the next one.
        assert_eq!(
            framer.push(b"\x1b]4;1;rgb:cd/00/00\x1b\\").len(),
            1,
            "palette reply"
        );
        assert!(framer.push(b"\x1b]").is_empty());
        assert_eq!(
            framer.idle_flush_timeout_ms(false),
            INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS
        );
    }

    #[test]
    fn malformed_host_reply_tail_preserves_following_input() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b";10xabc"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn host_reply_tail_discard_is_bounded_across_pushes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&[b'1'; 64]).is_empty());
        assert_eq!(
            framer.push(&[b'2'; 67]),
            vec![b"2".to_vec(), b"2".to_vec(), b"2".to_vec()]
        );
    }

    #[test]
    fn stops_holding_lone_escape_after_host_cell_size_reply_completes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert_eq!(
            framer.push(b"\x1b[6;21;10t"),
            vec![b"\x1b[6;21;10t".to_vec()]
        );

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn default_byte_framer_does_not_rearm_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opt_in_does_not_delay_plain_escape_without_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_outer_focus_gained() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"[?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_appearance_reply_split_after_csi() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_delayed_appearance_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn timed_out_appearance_reply_preserves_pending_color_reply_window() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"2n").is_empty());

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"]10;rgb:aaaa/bbbb/cccc\x1b\\"),
            vec![b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\".to_vec()]
        );
    }

    #[test]
    fn disabled_focus_query_does_not_rearm_byte_framer() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_policy_does_not_delay_plain_escape_without_focus() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_without_reply_holds_escape_for_only_one_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]10;#abcdef\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                color: RgbColor {
                    r: 0xab,
                    g: 0xcd,
                    b: 0xef
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]11;#123456\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn flushes_lone_escape_when_not_awaiting_host_color_reply() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn gives_up_holding_lone_escape_after_one_idle_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        assert!(framer.push(b"\x1b").is_empty());
        // First idle flush holds the escape.
        assert!(framer.flush_timeout().is_empty());
        // No continuation arrived; the second idle flush releases it as Escape.
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn stops_holding_lone_escape_after_host_color_reply_completes() {
        use std::fmt::Write as _;

        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut replies =
            String::from("\x1b]10;rgb:6565/7b7b/8383\x1b\\\x1b]11;rgb:2424/2727/3a3a\x1b\\");
        for index in 0..=u8::MAX {
            let _ = write!(replies, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\");
        }

        let chunks = framer.push(replies.as_bytes());
        assert_eq!(chunks.len(), 258);

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn overlapping_host_color_queries_keep_holding_split_reply_escape() {
        use std::fmt::Write as _;

        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.host_color_query_sent();
        let mut first_batch =
            String::from("\x1b]10;rgb:6565/7b7b/8383\x1b\\\x1b]11;rgb:2424/2727/3a3a\x1b\\");
        for index in 0..=u8::MAX {
            let _ = write!(first_batch, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\");
        }
        assert_eq!(framer.push(first_batch.as_bytes()).len(), 258);

        // The second batch is still outstanding, so its first reply split at ESC
        // must not leak into the pane as an Escape key.
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]10;rgb:eeee/eeee/eeee\x1b\\");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                ..
            }
        ));
    }

    fn non_reply_events(events: &[RawInputEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|event| !is_host_reply_event(event))
            .map(|event| format!("{event:?}"))
            .collect()
    }

    #[test]
    fn color_reply_after_released_escape_does_not_leak() {
        let cases: [(&[u8], &[u8]); 4] = [
            (b"]10;rgb:afaf/", b"d7d7/8787\x07x"),
            (b"]11;rgb:afaf/", b"d7d7/8787\x07x"),
            (b"]10;rgb:afaf/", b"d7d7/8787\x1b\\x"),
            (b"]11;rgb:afaf/", b"d7d7/8787\x1b\\x"),
        ];
        for (first, second) in cases {
            let mut framer = RawInputFramer::default();
            framer.byte_framer.host_color_query_sent();

            // The reply split after its ESC, and both idle flushes ran: the
            // first held the ESC, the second released it as a key.
            assert!(framer.push(b"\x1b").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.flush_timeout().len(), 1);

            let mut events = framer.push(first);
            events.extend(framer.flush_timeout());
            events.extend(framer.flush_timeout());
            events.extend(framer.push(second));

            let typed = non_reply_events(&events);
            assert_eq!(typed.len(), 1, "{first:?} {second:?}: {events:?}");
            let event = events
                .into_iter()
                .find(|event| !is_host_reply_event(event))
                .unwrap();
            assert_raw_key(event, KeyCode::Char('x'), KeyModifiers::empty());
            assert!(!framer.byte_framer.has_pending_input());
        }
    }

    #[test]
    fn color_reply_split_inside_its_header_after_released_escape_does_not_leak() {
        let reply = b"]11;rgb:afaf/d7d7/8787\x07";
        for split in 1..reply.len() {
            let mut framer = RawInputFramer::default();
            framer.byte_framer.host_color_query_sent();
            assert!(framer.push(b"\x1b").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.flush_timeout().len(), 1);

            let mut events = framer.push(&reply[..split]);
            events.extend(framer.flush_timeout());
            events.extend(framer.flush_timeout());
            events.extend(framer.push(&reply[split..]));
            events.extend(framer.push(b"x"));

            assert_eq!(
                non_reply_events(&events).len(),
                1,
                "split {split}: {events:?}"
            );
            assert!(!framer.byte_framer.has_pending_input(), "split {split}");
        }
    }

    #[test]
    fn bracket_typed_after_escape_during_color_query_is_bounded() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![vec![ESC]]);

        // `]` could start the reply's header, so it waits, but only while
        // the reply can still arrive.
        assert!(framer.push(b"]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.held_input_release_ms().is_some());
        framer.age_held_input(HOST_REPLY_QUIET_WINDOW);
        assert_eq!(framer.flush_timeout(), vec![b"]".to_vec()]);
        assert_eq!(framer.held_input_release_ms(), None);

        // Text that cannot be a reply header is never held.
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![vec![ESC]]);
        assert_eq!(framer.push(b"]x"), vec![b"]".to_vec(), b"x".to_vec()]);
    }
}
