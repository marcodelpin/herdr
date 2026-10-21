//! Property test for the raw-input hold (ADR-0013 and ADR-0012, herdr-nxa8;
//! ADR-0010, herdr-f2a1; upstream #5251).
//!
//! Generates input streams that mix typed text, Escape-led keys, Alt chords,
//! bracketed pastes, SGR and X10 mouse reports, OSC 10/11/4 colour replies
//! (some longer than any reply the framer accepts) and, while the Unix
//! reader awaits a direct-graphics response, kitty graphics APC commands
//! (complete responses, and streams that arrive in many small reads). It
//! cuts each token into up to six reads at random times, and on the Windows
//! pump interleaves semantic key records, also between the reads of one
//! token. It drives the byte framer the way the Unix stdin reader does
//! (through the production `ReaderSchedule`, behind a model of the
//! direct-graphics filter) or the way the Windows pump does (pump policy,
//! idle ticks, immediate flushes after key records), under a virtual clock.
//!
//! The generator stream is the same on every OS (ADR-0013 D5): each seed
//! runs a `Mixed` stream (Unix reader and pump cases) and a `PumpOnly`
//! stream. Windows skips only the cases that need the Unix-only escape
//! disambiguation.
//!
//! The oracle is independent of the framer:
//! - what reached the pane is decoded the way production decodes it (the
//!   Unix server decodes every event of a chunk, the Windows pump one event
//!   per chunk), so a suffix production does not consume is a loss;
//! - expectations are per event, so a key that arrives as literal text does
//!   not match the key;
//! - the harness records when every input byte was delivered, reached the
//!   framer and left it; rules that depend on time are resolved from that
//!   record after the run (ADR-0013 D6), and the age of every pending byte
//!   is measured from it against the ADRs' named bounds.
//!
//! Invariants (ADR-0013):
//! - I1 every byte that is not part of a report reaches the pane exactly
//!   once, in order, in the event production decodes it into;
//! - I2 a report the context says to drop does not reach the pane, and a key
//!   reaches it as that key or (split after a released prefix) not at all;
//! - I3 no pending byte except an unterminated paste is older than the
//!   pending-input bound, and the stream drains;
//! - I4' a semantic key never overtakes a byte that can reach the pane: after
//!   `flush_interrupted` only colour-reply bytes or an unterminated paste are
//!   pending.

use super::*;
use std::time::Instant;

/// Fixed seeds; every run generates the same cases.
const SEEDS: [u64; 4] = [0x0012_0001, 0x0012_0002, 0x0012_0003, 0x0012_0004];
const CASES_PER_SEED: usize = 3_000;
/// Stands for a semantic key (Windows key record) in the expected and the
/// observed stream. Never valid in UTF-8, so no generated input contains it.
const SEMANTIC: u8 = 0xff;

// The ADR bounds table, restated so the oracle does not read the framer's
// own deadlines (`oracle_bounds_match_the_adr` ties them together).
/// `REPORT_TAIL_LIMIT`: a split report or key tail is recognised within this.
const REPORT_SPAN: u64 = 500;
/// `COLOR_REPLY_TAIL_LIMIT`: the same for a colour reply while a query is
/// outstanding.
const COLOR_SPAN: u64 = 1_000;
/// `ORPHAN_TAIL_BURST`: a tail after a released prefix is dropped only when
/// all of it arrived within this of its first byte.
const BURST: u64 = 10;
/// `PENDING_INPUT_AGE_LIMIT`: no pending byte except an unterminated paste
/// is older than this after a framer call.
const AGE_LIMIT: u64 = 1_500;
/// `MAX_DISCARDED_CONTROL_TAIL_BYTES`.
const REPORT_BYTES: usize = 128;
/// Windows pump idle tick.
const WINDOWS_TICK_MS: u64 = 10;
/// The Windows default-colour reply window the harness opens at 0 ms.
const DEFAULT_COLOR_WINDOW: u64 = 1_000;

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }

    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// What the pane may receive for a stretch of the input.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rule {
    /// This byte reaches the pane once, in order; event boundaries are free.
    Keep,
    /// These bytes reach the pane once, in order, as exactly one event.
    KeepEvent,
    /// These bytes do not reach the pane.
    Drop,
    /// This byte may or may not reach the pane: outside what the framer
    /// promises (a report spread past its bound, a residual in the ADR).
    Either,
    /// The pane receives exactly one of these event sequences.
    OneOf(Vec<Vec<Vec<u8>>>),
    /// Decided after the run from when the bytes arrived (ADR-0013 D6).
    Deferred {
        when: Cond,
        then: Box<Rule>,
        otherwise: Box<Rule>,
    },
}

/// A fact about the run's timing, over input byte offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cond {
    /// Bytes `first..=last` reached the framer as one burst, starting while
    /// the context of the released prefix ending at `prefix` was still open.
    ReleasedTailBurst {
        prefix: usize,
        first: usize,
        last: usize,
        limit: u64,
    },
    /// Bytes `first..=last` were all delivered inside the graphics wait.
    InsideGraphicsWait { first: usize, last: usize },
    /// Bytes `first..=last` all reached the framer inside the default-colour
    /// reply window.
    InsideDefaultColorWindow { first: usize, last: usize },
    /// Byte `first` reached the framer after the default-colour window had
    /// closed and its expiry was processed.
    AfterDefaultColorWindow { first: usize },
}

#[derive(Clone, Debug)]
struct Unit {
    bytes: Vec<u8>,
    rule: Rule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Key,
    /// Escape, Alt+[ or Alt+O: a prefix of longer sequences.
    Ambiguous,
    AltChord,
    Paste,
    Sgr,
    X10,
    Color,
    /// A colour reply longer than the report budget.
    LongColor,
    /// A kitty graphics APC command while a response is awaited.
    Graphics,
    Semantic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Driver {
    UnixReader,
    WindowsPump,
}

/// Which generator stream a case comes from (ADR-0013 D5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    /// Unix reader and pump cases (the former Unix stream).
    Mixed,
    /// Pump cases only (the former Windows stream; CI cases 404 and 779).
    PumpOnly,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    driver: Driver,
    /// Selects the Unix reader's idle waits; the Windows pump ignores it.
    mouse_capture: bool,
    color_query: bool,
    /// The Windows default-colour query (1 s reply window) is outstanding
    /// at the start.
    default_color_query: bool,
    /// Unix escape disambiguation (#3480); needs a Unix framer.
    disambiguation: bool,
    /// The Unix reader awaits a direct-graphics response in this window
    /// (ms): the graphics filter holds what may be a response.
    graphics_wait: Option<(u64, u64)>,
}

#[derive(Debug)]
struct Read {
    /// Milliseconds after the previous read was delivered.
    gap: u64,
    bytes: Vec<u8>,
    /// Colour queries the client sent just before this read (one per colour
    /// reply that starts in it, when queries are on).
    color_queries: usize,
    /// The user types this only after everything before it reached the pane.
    wait_for_flush: bool,
    /// A semantic key record (Windows); `bytes` is empty.
    semantic: bool,
}

struct Case {
    config: Config,
    reads: Vec<Read>,
    expected: Vec<Unit>,
    /// For each input byte, the index one past the end of the bracketed
    /// paste it belongs to: the one pending input without an age bound.
    paste_end: Vec<Option<usize>>,
    /// For each input byte, the kind of token it belongs to.
    kinds: Vec<Kind>,
    /// The input bytes, in order (semantic records excluded).
    input: Vec<u8>,
    /// The input offset of each token's first byte, ascending.
    token_starts: Vec<usize>,
}

const TEMPLATES: [&[u8]; 4] = [b"<35;x", b"<1;2;3x", b"]10;x", b"]4;1;x"];
/// Typed characters, weighted toward the bytes reports are made of. No `M`,
/// `m` or `;`: typed text must not complete a mouse report by chance (`;`
/// comes only from templates that end in a byte no report carries).
const TYPED: &[u8] = b"[[]]<<0123456789abcdefABCDEFxyz /:#\\~?.-_+OPXr";
const KEYS: [&[u8]; 10] = [
    b"\x1b[A",
    b"\x1b[B",
    b"\x1b[C",
    b"\x1b[D",
    b"\x1b[H",
    b"\x1b[3~",
    b"\x1b[5~",
    b"\x1b[1;5C",
    b"\x1bOP",
    b"\x1bOQ",
];
/// Alt chords; `] P _ ^ X` start host control strings (#344) and `[`, `O`
/// are the ambiguous prefixes generated separately.
const ALT_CHORDS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789,./=";

fn typed_text(rng: &mut Rng) -> Vec<u8> {
    let mut text = Vec::new();
    for _ in 0..rng.range(1, 6) {
        match rng.below(20) {
            0 => text.extend_from_slice(rng.pick(&TEMPLATES)),
            1 => text.push(0x07),
            2 => text.push(rng.range(0x01, 0x1a) as u8),
            3 => text.extend_from_slice("\u{e9}".as_bytes()),
            4 => text.extend_from_slice("\u{4f60}".as_bytes()),
            _ => text.push(*rng.pick(TYPED)),
        }
    }
    text
}

fn hex4(rng: &mut Rng) -> String {
    format!("{:04x}", rng.below(0x1_0000))
}

fn color_reply(rng: &mut Rng, long: bool) -> Vec<u8> {
    let header = match rng.below(3) {
        0 => "]10;".to_string(),
        1 => "]11;".to_string(),
        _ => format!("]4;{};", rng.below(256)),
    };
    let terminator: &[u8] = if rng.chance(1, 2) { b"\x07" } else { b"\x1b\\" };
    let mut reply = vec![ESC];
    reply.extend_from_slice(header.as_bytes());
    reply.extend_from_slice(format!("rgb:{}/{}/{}", hex4(rng), hex4(rng), hex4(rng)).as_bytes());
    if long {
        // Past the report budget: no reply the framer accepts is this long.
        while reply.len() <= REPORT_BYTES + 8 {
            reply.extend_from_slice(format!("/{}", hex4(rng)).as_bytes());
        }
    }
    reply.extend_from_slice(terminator);
    reply
}

/// A kitty graphics APC command: a complete response, or (`stream`) a long
/// command whose payload arrives in many small reads.
fn graphics_command(rng: &mut Rng, stream: bool) -> Vec<u8> {
    let mut command = b"\x1b_G".to_vec();
    if stream {
        command.extend(std::iter::repeat_n(b'0', rng.range(200, 2_000) as usize));
    } else {
        command.extend_from_slice(format!("i={};OK", rng.range(1, 99)).as_bytes());
    }
    command.extend_from_slice(b"\x1b\\");
    command
}

fn token(rng: &mut Rng, config: &Config) -> (Kind, Vec<u8>) {
    let roll = rng.below(100);
    match roll {
        0..=23 => (Kind::Text, typed_text(rng)),
        24..=34 => (Kind::Key, rng.pick(&KEYS).to_vec()),
        35..=45 => {
            let prefix: &[u8] = rng.pick(&[b"\x1b".as_slice(), b"\x1b[", b"\x1bO"]);
            (Kind::Ambiguous, prefix.to_vec())
        }
        46..=52 => (Kind::AltChord, vec![ESC, *rng.pick(ALT_CHORDS)]),
        53..=58 => {
            let mut paste = BRACKETED_PASTE_START.to_vec();
            paste.extend(typed_text(rng));
            paste.extend_from_slice(BRACKETED_PASTE_END);
            (Kind::Paste, paste)
        }
        59..=72 => {
            let button = rng.pick(&[0u16, 1, 2, 32, 35, 64, 65]);
            let report = format!(
                "\x1b[<{};{};{}{}",
                button,
                rng.range(1, 300),
                rng.range(1, 300),
                if rng.chance(1, 2) { 'M' } else { 'm' }
            );
            (Kind::Sgr, report.into_bytes())
        }
        73..=79 => {
            let report = vec![
                ESC,
                b'[',
                b'M',
                32 + rng.below(4) as u8,
                33 + rng.below(90) as u8,
                33 + rng.below(90) as u8,
            ];
            (Kind::X10, report)
        }
        80..=89 => (Kind::Color, color_reply(rng, false)),
        90..=91 => (Kind::LongColor, color_reply(rng, true)),
        _ if config.driver == Driver::WindowsPump => (Kind::Semantic, Vec::new()),
        92..=95 if config.graphics_wait.is_some() => {
            let stream = rng.chance(1, 2);
            (Kind::Graphics, graphics_command(rng, stream))
        }
        _ => (Kind::Text, typed_text(rng)),
    }
}

fn gap(rng: &mut Rng) -> u64 {
    match rng.below(40) {
        0..=11 => 0,
        12..=15 => rng.range(1, 9),
        16..=21 => rng.range(10, 60),
        22..=27 => rng.range(100, 450),
        28..=32 => rng.range(450, 560),
        33..=36 => rng.range(560, 999),
        37..=38 => rng.range(1_000, 1_300),
        // Past every hold, the pending-input bound included.
        _ => rng.range(1_300, 4_000),
    }
}

/// A cut inside a token: the bytes before `at` were delivered `gap` ms
/// before the rest.
#[derive(Clone, Copy, Debug)]
struct Cut {
    at: usize,
    gap: u64,
}

fn per_byte(bytes: &[u8], rule: Rule) -> Vec<Unit> {
    bytes
        .iter()
        .map(|byte| Unit {
            bytes: vec![*byte],
            rule: rule.clone(),
        })
        .collect()
}

fn deferred(when: Cond, then: Rule, otherwise: Rule) -> Rule {
    Rule::Deferred {
        when,
        then: Box::new(then),
        otherwise: Box::new(otherwise),
    }
}

/// What a token that was cut but not interrupted may become (ADR-0012 D3,
/// ADR-0013 D4, D6). `start` is the input offset of its first byte.
fn cut_token_rule(
    kind: Kind,
    bytes: &[u8],
    cuts: &[Cut],
    config: &Config,
    start: usize,
) -> Vec<Unit> {
    let span: u64 = cuts.iter().map(|cut| cut.gap).sum();
    let first = cuts.first().copied();
    let later: u64 = cuts.iter().skip(1).map(|cut| cut.gap).sum();
    let every = |rule: Rule| per_byte(bytes, rule);
    let one = |rule: Rule| {
        vec![Unit {
            bytes: bytes.to_vec(),
            rule,
        }]
    };
    let released_prefixes = || vec![vec![vec![ESC]], vec![b"\x1b[".to_vec()]];
    // Spread past its bound: what arrived before the last late cut is outside
    // the promise, what arrived after it is typed (r3-3: a late tail that is
    // dropped is a loss, not an `Either`).
    let spread = || {
        let mut units = every(Rule::Either);
        if let Some(late) = late_cut(cuts, config) {
            for unit in &mut units[late..] {
                unit.rule = Rule::Keep;
            }
        }
        units
    };
    match kind {
        Kind::Text | Kind::AltChord | Kind::Ambiguous => {
            let mut units = every(Rule::Keep);
            mark_late_utf8(&mut units, bytes, cuts, None);
            units
        }
        Kind::Key => {
            let Some(first) = first else {
                return one(Rule::KeepEvent);
            };
            if bytes.get(1) == Some(&b'O') {
                // SS3 tails after a released prefix are not recovered.
                return spread();
            }
            if config.disambiguation && first.at == 2 && !matches!(bytes[2], b'M' | 0x20..=0x3f) {
                // With escape disambiguation, `ESC [` held past keyboard
                // timing and followed by a final byte is a key binding
                // (#4759): released as Alt+[, the byte typed. Unchanged.
                return spread();
            }
            if first.at <= 2 && first.gap < REPORT_SPAN && later <= BURST {
                // Whole, or the released ESC / Alt+[ and the tail dropped.
                let mut alternatives = released_prefixes();
                alternatives.push(vec![bytes.to_vec()]);
                return one(Rule::OneOf(alternatives));
            }
            if first.at >= 3 && first.gap < REPORT_SPAN && later < REPORT_SPAN {
                // Whole, or the prefix dropped at its timeout and the tail
                // after it.
                return one(Rule::OneOf(vec![vec![bytes.to_vec()], Vec::new()]));
            }
            spread()
        }
        Kind::Paste => {
            let marker_cut = cuts.iter().any(|cut| cut.at < BRACKETED_PASTE_START.len());
            if marker_cut {
                every(Rule::Either)
            } else {
                one(Rule::KeepEvent)
            }
        }
        Kind::Sgr => {
            if first.is_none() {
                return one(Rule::Drop);
            }
            if span < REPORT_SPAN {
                // Dropped however it was fragmented; a prefix split at its
                // ESC or `ESC [` may have gone out as a key first.
                let mut alternatives = vec![Vec::new()];
                if first.is_some_and(|cut| cut.at <= 2) {
                    alternatives.extend(released_prefixes());
                }
                return one(Rule::OneOf(alternatives));
            }
            spread()
        }
        Kind::X10 => {
            let Some(first) = first else {
                return one(Rule::Drop);
            };
            // X10 prefixes dropped at their timeout are not recovered.
            let inside = cuts.iter().any(|cut| cut.at >= 3);
            // On the pump an X10 tail after a released prefix is the report
            // at any spread within its bound (ADR-0013 D4).
            let tail_bound = if config.driver == Driver::WindowsPump {
                REPORT_SPAN - 1
            } else {
                BURST
            };
            if !inside && first.gap < REPORT_SPAN && later <= tail_bound {
                let mut alternatives = vec![Vec::new()];
                alternatives.extend(released_prefixes());
                return one(Rule::OneOf(alternatives));
            }
            spread()
        }
        Kind::Color => {
            let generic = || -> Vec<Unit> {
                if first.is_none() || (config.color_query && span < COLOR_SPAN) {
                    // A reply split right after its ESC or `ESC ]` can be no
                    // other way: that prefix may go out at its idle timeout
                    // and only the tail is dropped (ADR-0010 Consequences).
                    let released = cuts
                        .iter()
                        .map(|cut| cut.at)
                        .filter(|at| *at <= 2)
                        .max()
                        .unwrap_or(0);
                    return bytes
                        .iter()
                        .enumerate()
                        .map(|(index, byte)| Unit {
                            bytes: vec![*byte],
                            rule: if index < released {
                                Rule::Either
                            } else {
                                Rule::Drop
                            },
                        })
                        .collect();
                }
                every(Rule::Either)
            };
            if first.is_some() && config.default_color_query {
                // Inside the Windows default-colour window the reply
                // completes, an interruption included (ADR-0013 D1). Begun
                // after the window, it takes the generic rule. Straddling
                // the window end and its recovery grace is the declared
                // residual (ADR-0012 Residuals).
                let (first_byte, last_byte) = (start, start + bytes.len() - 1);
                return generic()
                    .into_iter()
                    .map(|unit| Unit {
                        bytes: unit.bytes,
                        rule: deferred(
                            Cond::InsideDefaultColorWindow {
                                first: first_byte,
                                last: last_byte,
                            },
                            Rule::Drop,
                            deferred(
                                Cond::AfterDefaultColorWindow { first: first_byte },
                                unit.rule,
                                Rule::Either,
                            ),
                        ),
                    })
                    .collect();
            }
            generic()
        }
        Kind::LongColor => every(Rule::Either),
        Kind::Graphics => every(deferred(
            Cond::InsideGraphicsWait {
                first: start,
                last: start + bytes.len() - 1,
            },
            Rule::Drop,
            Rule::Either,
        )),
        Kind::Semantic => unreachable!(),
    }
}

/// What a Key, SGR or X10 token cut by a semantic record at `at` may become
/// (ADR-0013 D6). The record ends every report and key context: the part
/// after it arrives with no context and is typed. Before it, an idle release
/// at some cut `b` may have sent out the prefix (ESC, `ESC [`, `ESC O`) or
/// dropped it (three bytes or more); what was held after `b` is emitted as
/// input when the record comes (#4048), except an unfinished X10 tail on the
/// pump, which is dropped (ADR-0013 D4): at the record, or at its deadline
/// at a later cut `j`, the bytes after `j` then arriving with no context.
fn interrupted_report_rule(
    kind: Kind,
    bytes: &[u8],
    cuts: &[Cut],
    at: usize,
    config: &Config,
) -> Vec<Unit> {
    let mut boundaries: Vec<usize> = cuts
        .iter()
        .map(|cut| cut.at)
        .filter(|cut| *cut < at)
        .chain(std::iter::once(at))
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut alternatives = Vec::new();
    for &b in &boundaries {
        let prefix: Vec<Vec<u8>> = if b <= 2 {
            vec![bytes[..b].to_vec()]
        } else {
            Vec::new()
        };
        let mut typed = prefix.clone();
        typed.extend(bytes[b..at].iter().map(|byte| vec![*byte]));
        alternatives.push(typed);
        if kind == Kind::X10 && config.driver == Driver::WindowsPump && b <= 2 {
            for &j in boundaries.iter().filter(|j| **j > b && **j < bytes.len()) {
                let mut dropped = prefix.clone();
                dropped.extend(bytes[j..at].iter().map(|byte| vec![*byte]));
                alternatives.push(dropped);
            }
        }
    }
    alternatives.dedup();
    let mut units = vec![Unit {
        bytes: bytes[..at].to_vec(),
        rule: Rule::OneOf(alternatives),
    }];
    units.extend(per_byte(&bytes[at..], Rule::Keep));
    units
}

/// The offset of the last cut after which every context of the token has
/// closed: its prefix was released within the pending-input bound (plus a
/// Windows tick) and the longest tail limit has passed since. The graphics
/// filter can delay the prefix itself past that, so it has no late cut.
fn late_cut(cuts: &[Cut], config: &Config) -> Option<usize> {
    if config.graphics_wait.is_some() {
        return None;
    }
    let late = AGE_LIMIT + WINDOWS_TICK_MS + COLOR_SPAN + 90;
    cuts.iter()
        .filter(|cut| cut.gap >= late)
        .map(|cut| cut.at)
        .max()
}

/// A UTF-8 character cut past the pending-input bound, or cut by a
/// semantic record, is dropped: its bytes are `Either`.
fn mark_late_utf8(units: &mut [Unit], bytes: &[u8], cuts: &[Cut], interrupt_at: Option<usize>) {
    let mut start = 0;
    while start < bytes.len() {
        let end = (start + utf8_len(bytes[start])).min(bytes.len());
        let inside: u64 = cuts
            .iter()
            .filter(|cut| cut.at > start && cut.at < end)
            .map(|cut| cut.gap)
            .sum();
        let interrupted = interrupt_at.is_some_and(|at| at > start && at < end);
        if interrupted || inside + 1 >= AGE_LIMIT {
            for unit in &mut units[start..end] {
                unit.rule = Rule::Either;
            }
        }
        start = end;
    }
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// After a released Escape or Alt+[ the framer drops the tail of a split
/// key that arrives as one burst inside the released prefix's context
/// (ADR-0012 D3). Typed bytes that, after that prefix, form a CSI key may be
/// dropped only then: whether they did is decided from their arrival times
/// after the run (ADR-0013 D6, codex f5251-r4 finding 4).
fn mark_released_prefix_tails(case: &mut Case) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Scan {
        None,
        AfterPrefix,
        InCsi,
    }
    let limit = if case.config.color_query {
        COLOR_SPAN
    } else {
        REPORT_SPAN
    };
    let mut scan = Scan::None;
    let mut prefix = 0;
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0;
    for index in 0..case.expected.len() {
        let unit = &case.expected[index];
        if unit.bytes == [SEMANTIC] && unit.rule == Rule::KeepEvent {
            scan = Scan::None;
            candidates.clear();
            continue;
        }
        let len = unit.bytes.len();
        let kind = case.kinds[offset];
        let token_ends_here =
            offset + len == case.kinds.len() || case.token_start(offset + len) == offset + len;
        if kind == Kind::Graphics {
            // The graphics filter may take it out before the framer sees it.
            offset += len;
            continue;
        }
        if kind != Kind::Text {
            candidates.clear();
            scan = Scan::None;
            if kind == Kind::Ambiguous && token_ends_here {
                let token = case.token_start(offset)..offset + len;
                let bytes: Vec<u8> = case.expected_bytes(token.clone());
                match bytes.as_slice() {
                    [ESC] => {
                        scan = Scan::AfterPrefix;
                        prefix = offset + len - 1;
                    }
                    [ESC, b'['] => {
                        scan = Scan::InCsi;
                        prefix = offset + len - 1;
                    }
                    _ => {}
                }
            }
            offset += len;
            continue;
        }
        let byte = unit.bytes[0];
        match scan {
            Scan::AfterPrefix if byte == b'[' => {
                scan = Scan::InCsi;
                candidates.push((index, offset));
            }
            Scan::InCsi if (0x20..=0x3f).contains(&byte) => candidates.push((index, offset)),
            Scan::InCsi if (0x40..=0x7e).contains(&byte) => {
                candidates.push((index, offset));
                let first = candidates[0].1;
                for (unit_index, _) in candidates.drain(..) {
                    let unit = &mut case.expected[unit_index];
                    if unit.rule == Rule::Keep {
                        unit.rule = deferred(
                            Cond::ReleasedTailBurst {
                                prefix,
                                first,
                                last: offset,
                                limit,
                            },
                            Rule::Either,
                            Rule::Keep,
                        );
                    }
                }
                scan = Scan::None;
            }
            _ => {
                scan = Scan::None;
                candidates.clear();
            }
        }
        offset += len;
    }
}

impl Case {
    /// The input offset where the token holding byte `offset` starts.
    fn token_start(&self, offset: usize) -> usize {
        self.token_starts
            .partition_point(|start| *start <= offset)
            .checked_sub(1)
            .map_or(0, |index| self.token_starts[index])
    }

    /// The input bytes in `range`.
    fn expected_bytes(&self, range: std::ops::Range<usize>) -> Vec<u8> {
        self.input[range].to_vec()
    }
}

fn generate(rng: &mut Rng, stream: Stream) -> Case {
    // The draws match the former per-OS streams, so `PumpOnly` reproduces
    // the native Windows cases (ADR-0013 D5).
    let driver = match stream {
        Stream::Mixed if rng.chance(1, 2) => Driver::UnixReader,
        Stream::Mixed | Stream::PumpOnly => Driver::WindowsPump,
    };
    let default_color_query = driver == Driver::WindowsPump && rng.chance(1, 4);
    let graphics_wait = (driver == Driver::UnixReader && rng.chance(1, 4)).then(|| {
        let start = rng.range(0, 3_000);
        (start, start + rng.range(100, 4_000))
    });
    let mouse_capture = stream == Stream::Mixed && rng.chance(1, 2);
    let config = Config {
        driver,
        mouse_capture,
        color_query: default_color_query || rng.chance(1, 2),
        default_color_query,
        disambiguation: driver == Driver::UnixReader && rng.chance(1, 4),
        graphics_wait,
    };

    let mut reads: Vec<Read> = Vec::new();
    let mut expected: Vec<Unit> = Vec::new();
    let mut paste_end = Vec::new();
    let mut kinds = Vec::new();
    let mut input = Vec::new();
    let mut token_starts = Vec::new();
    let mut previous_ambiguous = false;
    for _ in 0..rng.range(3, 14) {
        let (kind, bytes) = token(rng, &config);
        if kind == Kind::Semantic {
            reads.push(Read {
                gap: gap(rng),
                bytes: Vec::new(),
                color_queries: 0,
                wait_for_flush: false,
                semantic: true,
            });
            expected.push(Unit {
                bytes: vec![SEMANTIC],
                rule: Rule::KeepEvent,
            });
            previous_ambiguous = false;
            continue;
        }

        let (pieces, gaps) = if kind == Kind::Graphics && bytes.len() > 64 {
            // A stream: many small reads, each sooner than any idle wait.
            let mut pieces = Vec::new();
            let mut gaps = Vec::new();
            let mut start = 0;
            while start < bytes.len() {
                let end = (start + rng.range(1, 24) as usize).min(bytes.len());
                pieces.push(bytes[start..end].to_vec());
                gaps.push(if start == 0 {
                    gap(rng)
                } else {
                    rng.range(1, 9)
                });
                start = end;
            }
            (pieces, gaps)
        } else {
            // Cut into up to six pieces at distinct offsets.
            let mut offsets: Vec<usize> = (0..rng.below(6))
                .filter(|_| bytes.len() > 1)
                .map(|_| rng.range(1, bytes.len() as u64 - 1) as usize)
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            let mut gaps = Vec::new();
            let mut pieces = Vec::new();
            let mut start = 0;
            for cut in offsets.iter().copied().chain(std::iter::once(bytes.len())) {
                pieces.push(bytes[start..cut].to_vec());
                gaps.push(gap(rng));
                start = cut;
            }
            (pieces, gaps)
        };
        let piece_ends: Vec<usize> = pieces
            .iter()
            .scan(0, |end, piece| {
                *end += piece.len();
                Some(*end)
            })
            .collect();
        // A cut delivered in the same read as the bytes before it is no cut.
        let cuts: Vec<Cut> = piece_ends[..piece_ends.len() - 1]
            .iter()
            .copied()
            .zip(gaps[1..].iter().copied())
            .filter(|(_, gap)| *gap > 0)
            .map(|(at, gap)| Cut { at, gap })
            .collect();
        // On Windows a semantic record can come between two reads of a token
        // (not inside a paste, which the pump never splits around a record).
        let interrupt_at: Option<usize> = (driver == Driver::WindowsPump
            && kind != Kind::Paste
            && !cuts.is_empty()
            && rng.chance(1, 4))
        .then(|| cuts[rng.below(cuts.len() as u64) as usize].at);

        // After Escape, Alt+[ or Alt+O, the user types a byte that is not
        // ESC-led only once the key reached the pane: before that it would be
        // another key (`ESC a` is Alt+a), which is not what the oracle models.
        // A graphics command the filter takes out is invisible to the framer.
        let wait_for_flush = previous_ambiguous && bytes.first() != Some(&ESC);
        previous_ambiguous =
            kind == Kind::Ambiguous || (kind == Kind::Graphics && previous_ambiguous);

        let token_start = paste_end.len();
        let mut units = match interrupt_at {
            None => cut_token_rule(kind, &bytes, &cuts, &config, token_start),
            Some(at) => match kind {
                Kind::Text | Kind::AltChord | Kind::Ambiguous => {
                    let mut units = per_byte(&bytes, Rule::Keep);
                    mark_late_utf8(&mut units, &bytes, &cuts, Some(at));
                    units
                }
                Kind::Key | Kind::Sgr | Kind::X10 => {
                    interrupted_report_rule(kind, &bytes, &cuts, at, &config)
                }
                // A reply prefix longer than ESC stays pending through the
                // record (ADR-0013 D1) and takes its uninterrupted rule. A
                // lone ESC goes out before the record here: the pump's
                // host-reply exception for it is not modelled.
                Kind::Color if at >= 2 => cut_token_rule(kind, &bytes, &cuts, &config, token_start),
                _ => per_byte(&bytes, Rule::Either),
            },
        };
        token_starts.push(token_start);
        input.extend_from_slice(&bytes);
        kinds.extend(std::iter::repeat_n(kind, bytes.len()));
        paste_end.extend(
            std::iter::repeat_n(token_start + bytes.len(), bytes.len())
                .map(|end| (kind == Kind::Paste).then_some(end)),
        );

        let queries = usize::from(kind == Kind::Color && config.color_query);
        let mut offset = 0;
        for (index, (piece, gap)) in pieces.into_iter().zip(gaps).enumerate() {
            let first = index == 0;
            let gap = if first && wait_for_flush {
                gap.max(1)
            } else {
                gap
            };
            if interrupt_at == Some(offset) {
                // The record arrives first, then this piece right after it.
                let split = units
                    .iter()
                    .scan(0, |len, unit| {
                        *len += unit.bytes.len();
                        Some(*len)
                    })
                    .position(|len| len == offset)
                    .map_or(0, |index| index + 1);
                let rest = units.split_off(split);
                expected.append(&mut units);
                expected.push(Unit {
                    bytes: vec![SEMANTIC],
                    rule: Rule::KeepEvent,
                });
                units = rest;
                reads.push(Read {
                    gap,
                    bytes: Vec::new(),
                    color_queries: 0,
                    wait_for_flush: false,
                    semantic: true,
                });
                reads.push(Read {
                    gap: 0,
                    bytes: piece.clone(),
                    color_queries: 0,
                    wait_for_flush: false,
                    semantic: false,
                });
                offset += piece.len();
                continue;
            }
            offset += piece.len();
            let merge = gap == 0 && !reads.is_empty() && !reads.last().is_some_and(|r| r.semantic);
            if merge {
                let last = reads.last_mut().expect("checked");
                last.bytes.extend(piece);
                if first {
                    last.color_queries += queries;
                }
            } else {
                reads.push(Read {
                    gap,
                    bytes: piece,
                    color_queries: if first { queries } else { 0 },
                    wait_for_flush: first && wait_for_flush,
                    semantic: false,
                });
            }
        }
        expected.append(&mut units);
    }
    let mut case = Case {
        config,
        reads,
        expected,
        paste_end,
        kinds,
        input,
        token_starts,
    };
    mark_released_prefix_tails(&mut case);
    case
}

/// The bytes of a typed event (key, text, paste); None for anything else.
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

/// The typed events production decodes from released chunks: the Unix
/// server decodes every event of a chunk, the Windows pump one per chunk.
fn decode(driver: Driver, chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    match driver {
        Driver::UnixReader => chunks
            .iter()
            .flat_map(|chunk| parse_framed_input(chunk))
            .filter_map(|event| typed_event_bytes(&event))
            .collect(),
        Driver::WindowsPump => events_from_framed_chunks(chunks)
            .iter()
            .filter_map(typed_event_bytes)
            .collect(),
    }
}

/// The direct-graphics filter of the Unix reader
/// (`client::direct_graphics::InputFilter`) while a response is awaited: a
/// complete kitty APC command is taken out of the input (a response it
/// matches is consumed; one it does not is a complete control string the
/// framer drops), an incomplete one is kept (up to 4096 bytes), and so is a
/// trailing prefix of `ESC _ G`. Once none is awaited, what it kept is
/// released. Each byte carries its input offset.
struct GraphicsFilter {
    window: Option<(u64, u64)>,
    pending: Vec<(u8, usize)>,
}

const GRAPHICS_PREFIX: &[u8] = b"\x1b_G";
const GRAPHICS_LIMIT: usize = 4_096;

impl GraphicsFilter {
    fn interested(&self, now: u64) -> bool {
        self.window
            .is_some_and(|(start, end)| (start..end).contains(&now))
    }

    fn bytes(&self) -> Vec<u8> {
        self.pending.iter().map(|(byte, _)| *byte).collect()
    }

    /// The runs of bytes it lets through, possibly none.
    fn push(&mut self, bytes: &[(u8, usize)], now: u64) -> Vec<Vec<(u8, usize)>> {
        self.pending.extend_from_slice(bytes);
        if !self.interested(now) {
            return vec![std::mem::take(&mut self.pending)];
        }
        let mut output = Vec::new();
        loop {
            let pending = self.bytes();
            let Some(start) = pending
                .windows(GRAPHICS_PREFIX.len())
                .position(|window| window == GRAPHICS_PREFIX)
            else {
                let keep = (1..GRAPHICS_PREFIX.len())
                    .rev()
                    .find(|len| pending.ends_with(&GRAPHICS_PREFIX[..*len]))
                    .unwrap_or(0);
                let emit = pending.len() - keep;
                if emit > 0 {
                    output.push(self.pending.drain(..emit).collect());
                }
                break;
            };
            if start > 0 {
                output.push(self.pending.drain(..start).collect());
                continue;
            }
            let Some(end) = pending.windows(2).position(|window| window == b"\x1b\\") else {
                if pending.len() > GRAPHICS_LIMIT {
                    output.push(std::mem::take(&mut self.pending));
                }
                break;
            };
            self.pending.drain(..end + 2);
        }
        output
    }

    fn flush_if_inactive(&mut self, now: u64) -> Option<Vec<(u8, usize)>> {
        (!self.pending.is_empty() && !self.interested(now))
            .then(|| std::mem::take(&mut self.pending))
    }
}

/// A driver: the framer, the virtual clock, and what reached the pane.
struct Harness<'a> {
    case: &'a Case,
    next: usize,
    base: Instant,
    now: u64,
    last_delivery: u64,
    framer: RawInputByteFramer,
    schedule: ReaderSchedule,
    /// Typed events, in the order they reached the pane.
    out: Vec<Vec<u8>>,
    /// The input offset of each read's first byte.
    read_starts: Vec<usize>,
    /// For each byte the framer received, in order: its input offset and
    /// when it arrived.
    framed: Vec<(usize, u64)>,
    /// How many of the framed bytes have left the framer.
    left: usize,
    /// Per input byte: delivered by the reader, reached the framer, left it.
    delivered_at: Vec<Option<u64>>,
    framed_at: Vec<Option<u64>>,
    left_at: Vec<Option<u64>>,
    filter: GraphicsFilter,
    rng: Rng,
    /// When the Windows default-colour reply window closed: at its end, or
    /// earlier when a semantic key came while a lone ESC was pending (the
    /// pump's host-reply Escape rule).
    default_window_end: u64,
}

impl<'a> Harness<'a> {
    fn new(case: &'a Case, seed: u64) -> Self {
        let base = Instant::now();
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        framer.set_test_clock(base);
        if case.config.driver == Driver::WindowsPump {
            framer.set_pump_policy();
        }
        #[cfg(unix)]
        if case.config.disambiguation {
            framer.set_host_escape_disambiguation_active(true);
        }
        if case.config.default_color_query {
            framer.host_default_color_query_sent(Duration::from_secs(1));
        } else if case.config.color_query {
            framer.host_color_query_sent();
        }
        let read_starts = case
            .reads
            .iter()
            .scan(0, |offset, read| {
                let start = *offset;
                *offset += read.bytes.len();
                Some(start)
            })
            .collect();
        let len = case.input.len();
        Self {
            case,
            next: 0,
            base,
            now: 0,
            last_delivery: 0,
            framer,
            schedule: ReaderSchedule::default(),
            out: Vec::new(),
            read_starts,
            framed: Vec::new(),
            left: 0,
            delivered_at: vec![None; len],
            framed_at: vec![None; len],
            left_at: vec![None; len],
            filter: GraphicsFilter {
                window: case.config.graphics_wait,
                pending: Vec::new(),
            },
            rng: Rng(seed),
            default_window_end: DEFAULT_COLOR_WINDOW,
        }
    }

    /// The input offsets of the bytes the framer still holds.
    fn pending_offsets(&self) -> impl Iterator<Item = usize> + '_ {
        let pending = self.framer.buffer.len().min(self.framed.len());
        self.framed[self.framed.len() - pending..]
            .iter()
            .map(|(offset, _)| *offset)
    }

    /// I3, measured from the harness's own record: the age of the oldest
    /// byte the framer still holds, unless it is part of a bracketed paste
    /// whose end has not arrived. `slack` allows the reader's round-up.
    fn check_age(&self, what: &str, slack: u64) -> Result<(), String> {
        let pending = self.framer.buffer.len().min(self.framed.len());
        if pending == 0 {
            return Ok(());
        }
        let (oldest, arrived) = self.framed[self.framed.len() - pending];
        let received = self.framed.last().map_or(0, |(offset, _)| offset + 1);
        if self.case.paste_end[oldest].is_some_and(|end| end > received) {
            return Ok(());
        }
        let age = self.now - arrived;
        if age + 1 > AGE_LIMIT + slack {
            return Err(format!(
                "I3: byte {oldest} pending {age} ms after it reached the framer, {what} at {} ms: {:?}",
                self.now,
                self.framer.buffer.escape_ascii().to_string()
            ));
        }
        Ok(())
    }

    fn advance_to(&mut self, at: u64) -> Result<(), String> {
        self.now = self.now.max(at);
        self.framer
            .set_test_clock(self.base + Duration::from_millis(self.now));
        // The Unix reader rounds its waits up by 1 ms; the Windows pump
        // looks only on its idle tick.
        let slack = match self.case.config.driver {
            Driver::UnixReader => 2,
            Driver::WindowsPump => WINDOWS_TICK_MS,
        };
        self.check_age("waking", slack)
    }

    /// Records bytes about to reach the framer.
    fn frame(&mut self, bytes: &[(u8, usize)]) {
        for (_, offset) in bytes {
            self.framed.push((*offset, self.now));
            self.framed_at[*offset] = Some(self.now);
        }
    }

    /// After a framer call: records what left it, checks I3 and emits.
    fn settle(&mut self, what: &str, chunks: Vec<Vec<u8>>) -> Result<(), String> {
        let pending = self.framer.buffer.len().min(self.framed.len());
        let gone = self.framed.len() - pending;
        for index in self.left..gone {
            let offset = self.framed[index].0;
            self.left_at[offset] = Some(self.now);
        }
        self.left = self.left.max(gone);
        self.check_age(what, 0)?;
        let events = decode(self.case.config.driver, chunks);
        self.out.extend(events);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        let chunks = self.framer.flush_timeout();
        self.settle("after flush_timeout", chunks)
    }

    fn filter_pending(&self) -> bool {
        !self.filter.pending.is_empty()
    }

    /// When the next read arrives, or None if there is none or the user is
    /// still waiting for an earlier key to reach the pane.
    fn arrival(&self) -> Option<u64> {
        let read = self.case.reads.get(self.next)?;
        if read.wait_for_flush && (self.framer.has_pending_input() || self.filter_pending()) {
            return None;
        }
        Some((self.last_delivery + read.gap).max(self.now))
    }

    fn arrives_by(&self, due: u64) -> bool {
        self.arrival().is_some_and(|at| at <= due)
    }

    /// Delivers the next read: the semantic record is handled here, a read
    /// of bytes is returned with each byte's input offset.
    fn deliver(&mut self) -> Result<Option<Vec<(u8, usize)>>, String> {
        let case = self.case;
        let read = &case.reads[self.next];
        let at = self.arrival().ok_or("delivering a blocked read")?;
        self.advance_to(at)?;
        let start = self.read_starts[self.next];
        self.next += 1;
        self.last_delivery = self.now;
        if read.semantic {
            let lone_escape = self.framer.buffer.last() == Some(&ESC);
            let window_open = self.framer.host_default_color_query_deadline.is_some();
            let chunks = self.framer.flush_interrupted();
            if window_open && self.framer.host_default_color_query_deadline.is_none() {
                if !lone_escape && self.now < DEFAULT_COLOR_WINDOW {
                    return Err(format!(
                        "the default-colour window closed early at {} ms with no ESC pending",
                        self.now
                    ));
                }
                self.default_window_end = self.default_window_end.min(self.now);
            }
            // I4': only bytes that cannot reach the pane as input are left
            // to follow the record, judged from the harness's own record.
            let stranded: Vec<usize> = self
                .pending_offsets()
                .filter(|offset| !matches!(case.kinds[*offset], Kind::Color | Kind::LongColor))
                .collect();
            if !stranded.is_empty() && !self.framer.has_pending_bracketed_paste() {
                return Err(format!(
                    "I4': input bytes {stranded:?} pending behind a semantic key at {} ms",
                    self.now
                ));
            }
            self.settle("after flush_interrupted", chunks)?;
            self.out.push(vec![SEMANTIC]);
            return Ok(None);
        }
        for _ in 0..read.color_queries {
            self.framer.host_color_query_sent();
        }
        let bytes: Vec<(u8, usize)> = read
            .bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| (*byte, start + index))
            .collect();
        for (_, offset) in &bytes {
            self.delivered_at[*offset] = Some(self.now);
        }
        Ok(Some(bytes))
    }

    fn stranded(&self) -> String {
        format!(
            "I3: {} bytes pending with no release at {} ms: {:?}",
            self.framer.buffer.len(),
            self.now,
            self.framer.buffer.escape_ascii().to_string()
        )
    }

    fn finish(&self) -> Result<(), String> {
        if self.framer.has_pending_input() || self.filter_pending() {
            Err(self.stranded())
        } else {
            Ok(())
        }
    }

    /// The Unix stdin reader loop of `client/input.rs`: the same
    /// `ReaderSchedule` calls, with `read()` and `stdin_read_ready` answered
    /// from the schedule and the graphics filter modelled.
    fn run_unix_reader(&mut self) -> Result<(), String> {
        let mouse_capture = self.case.config.mouse_capture;
        for _ in 0..1_000_000 {
            if let Some((wait, framer_due)) =
                self.schedule.wait(&self.framer, self.filter_pending())
            {
                let due = self.now + wait.max(0) as u64;
                if !self.arrives_by(due) {
                    self.advance_to(due)?;
                    let now = self.now;
                    let released = self.filter.flush_if_inactive(now);
                    if let Some(released) = &released {
                        self.frame(released);
                    }
                    let released = released.map(|run| run.iter().map(|(byte, _)| *byte).collect());
                    let chunks = self.schedule.on_idle(
                        &mut self.framer,
                        released,
                        framer_due,
                        mouse_capture,
                    );
                    self.settle("after an idle wake", chunks)?;
                    continue;
                }
            }
            if self.next == self.case.reads.len() {
                return self.finish();
            }
            if self.arrival().is_none() {
                return Err(self.stranded());
            }
            let Some(bytes) = self.deliver()? else {
                return Err("a semantic record on the Unix reader".to_string());
            };
            let now = self.now;
            let runs = self.filter.push(&bytes, now);
            let mut filtered = Vec::new();
            for run in runs {
                self.frame(&run);
                filtered.push(run.iter().map(|(byte, _)| *byte).collect());
            }
            let chunks = self
                .schedule
                .on_read(&mut self.framer, filtered, mouse_capture);
            self.settle("after a read", chunks)?;
        }
        Err("reader loop did not terminate".to_string())
    }

    /// The Windows pump: `push` per read (sometimes followed by an immediate
    /// `flush_timeout`, as a key record with no event does), `flush_timeout`
    /// on every idle tick, `flush_interrupted` before a semantic key. Idle
    /// ticks that cannot change anything are skipped: after three quiet ticks
    /// the next one runs when the framer's state can next change with time.
    fn run_windows_pump(&mut self) -> Result<(), String> {
        for _ in 0..100_000 {
            let mut quiet = 0;
            loop {
                let tick = if quiet < 3 {
                    Some(self.now + WINDOWS_TICK_MS)
                } else {
                    let since_base =
                        |at: Instant| at.saturating_duration_since(self.base).as_millis() as u64;
                    let release = self
                        .framer
                        .held_input_release_ms()
                        .map(|ms| self.now + ms.max(1) as u64);
                    let deadlines = [
                        self.framer.host_color_tail_deadline.map(since_base),
                        self.framer
                            .host_default_color_query_deadline
                            .map(since_base),
                    ];
                    let reply_windows = [self.last_delivery + 1_010, self.last_delivery + 1_020];
                    release
                        .into_iter()
                        .chain(deadlines.into_iter().flatten())
                        .chain(reply_windows)
                        .filter(|at| *at > self.now)
                        .min()
                };
                let Some(tick) = tick else { break };
                if self.arrives_by(tick) {
                    break;
                }
                self.advance_to(tick)?;
                self.flush()?;
                quiet += 1;
            }
            if self.next == self.case.reads.len() {
                return self.finish();
            }
            if self.arrival().is_none() {
                return Err(self.stranded());
            }
            if let Some(bytes) = self.deliver()? {
                self.frame(&bytes);
                let raw: Vec<u8> = bytes.iter().map(|(byte, _)| *byte).collect();
                let chunks = self.framer.push(&raw);
                self.settle("after push", chunks)?;
                if self.rng.chance(1, 4) {
                    self.flush()?;
                }
            }
        }
        Err("pump loop did not terminate".to_string())
    }

    /// Whether `when` held in this run.
    fn holds(&self, when: &Cond) -> bool {
        let framed = |offset: usize| self.framed_at[offset];
        match *when {
            Cond::ReleasedTailBurst {
                prefix,
                first,
                last,
                limit,
            } => match (self.left_at[prefix], framed(first), framed(last)) {
                (Some(released), Some(first), Some(last)) => {
                    last - first <= BURST && first < released + limit
                }
                _ => false,
            },
            Cond::InsideGraphicsWait { first, last } => {
                let Some((start, end)) = self.case.config.graphics_wait else {
                    return false;
                };
                match (self.delivered_at[first], self.delivered_at[last]) {
                    (Some(first), Some(last)) => start <= first && last < end,
                    _ => false,
                }
            }
            Cond::InsideDefaultColorWindow { first, last } => {
                framed(first).is_some()
                    && framed(last).is_some_and(|at| at < self.default_window_end)
            }
            Cond::AfterDefaultColorWindow { first } => {
                framed(first).is_some_and(|at| at >= self.default_window_end + 2 * WINDOWS_TICK_MS)
            }
        }
    }

    /// The expectations with every deferred rule decided from this run.
    fn resolve(&self) -> Vec<Unit> {
        self.case
            .expected
            .iter()
            .map(|unit| {
                let mut rule = unit.rule.clone();
                while let Rule::Deferred {
                    when,
                    then,
                    otherwise,
                } = rule
                {
                    rule = if self.holds(&when) { *then } else { *otherwise };
                }
                Unit {
                    bytes: unit.bytes.clone(),
                    rule,
                }
            })
            .collect()
    }
}

/// Whether `out` holds exactly `events`, event for event, from byte `at`.
fn match_events(out: &[u8], boundary: &[bool], at: usize, events: &[Vec<u8>]) -> Option<usize> {
    let mut at = at;
    for event in events {
        let end = at + event.len();
        if !boundary[at]
            || end > out.len()
            || out[at..end] != event[..]
            || boundary[at + 1..end].contains(&true)
            || !boundary[end]
        {
            return None;
        }
        at = end;
    }
    Some(at)
}

/// I1, I2, I4: the typed events `out` are what `expected` allows, in order.
fn check_output(expected: &[Unit], out: &[Vec<u8>]) -> Result<(), String> {
    let bytes: Vec<u8> = out.concat();
    let mut boundary = vec![false; bytes.len() + 1];
    let mut at = 0;
    for event in out {
        boundary[at] = true;
        at += event.len();
    }
    boundary[at] = true;

    let mut reach = vec![false; bytes.len() + 1];
    reach[0] = true;
    for (index, unit) in expected.iter().enumerate() {
        let mut next = vec![false; bytes.len() + 1];
        for matched in 0..=bytes.len() {
            if !reach[matched] {
                continue;
            }
            let here = bytes[matched..].starts_with(&unit.bytes);
            match &unit.rule {
                Rule::Keep => {
                    if here {
                        next[matched + unit.bytes.len()] = true;
                    }
                }
                Rule::Either => {
                    next[matched] = true;
                    if here {
                        next[matched + unit.bytes.len()] = true;
                    }
                }
                Rule::Drop => next[matched] = true,
                Rule::KeepEvent => {
                    if let Some(end) = match_events(
                        &bytes,
                        &boundary,
                        matched,
                        std::slice::from_ref(&unit.bytes),
                    ) {
                        next[end] = true;
                    }
                }
                Rule::OneOf(alternatives) => {
                    for events in alternatives {
                        if let Some(end) = match_events(&bytes, &boundary, matched, events) {
                            next[end] = true;
                        }
                    }
                }
                Rule::Deferred { .. } => {
                    return Err(format!("expected unit {index} was never resolved"));
                }
            }
        }
        reach = next;
        if !reach.contains(&true) {
            return Err(format!(
                "no alignment past expected unit {index} ({:?} {:?})",
                unit.bytes.escape_ascii().to_string(),
                unit.rule
            ));
        }
    }
    if reach[bytes.len()] {
        Ok(())
    } else {
        Err("the pane received events that are not in the input".to_string())
    }
}

fn describe_rule(rule: &Rule, bytes: &str) -> String {
    match rule {
        Rule::Keep => bytes.to_string(),
        Rule::KeepEvent => format!("[{bytes}]"),
        Rule::Drop => format!("-({bytes})"),
        Rule::Either => format!("?{bytes}"),
        Rule::OneOf(_) => format!("{{{bytes}}}"),
        Rule::Deferred { .. } => format!("<{bytes}>"),
    }
}

fn describe(case: &Case, expected: &[Unit]) -> String {
    let reads: Vec<String> = case
        .reads
        .iter()
        .map(|read| {
            if read.semantic {
                format!("+{} SEMANTIC", read.gap)
            } else {
                format!(
                    "+{}{}{} {:?}",
                    read.gap,
                    if read.wait_for_flush { " wait" } else { "" },
                    if read.color_queries > 0 { " query" } else { "" },
                    read.bytes.escape_ascii().to_string()
                )
            }
        })
        .collect();
    let expected: String = expected
        .iter()
        .map(|unit| describe_rule(&unit.rule, &unit.bytes.escape_ascii().to_string()))
        .collect();
    format!(
        "{:?}\n  reads: {reads:?}\n  expected: {expected}",
        case.config
    )
}

fn run_case(case: &Case, seed: u64) -> Result<(), String> {
    let mut harness = Harness::new(case, seed);
    let run = match case.config.driver {
        Driver::UnixReader => harness.run_unix_reader(),
        Driver::WindowsPump => harness.run_windows_pump(),
    };
    let expected = harness.resolve();
    run.and_then(|()| check_output(&expected, &harness.out))
        .map_err(|err| {
            let out: Vec<String> = harness
                .out
                .iter()
                .map(|event| event.escape_ascii().to_string())
                .collect();
            format!("{err}\n  out: {out:?}\n  {}", describe(case, &expected))
        })
}

/// Whether this OS can run `case`: escape disambiguation needs a Unix framer.
fn runs_here(case: &Case) -> bool {
    cfg!(unix) || !case.config.disambiguation
}

#[test]
fn held_input_property_every_byte_once_in_order_and_bounded() {
    let only = std::env::var("HERDR_HOLD_PROPERTY_ONLY").ok();
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut pump_cases = 0;
    for seed in SEEDS {
        for stream in [Stream::Mixed, Stream::PumpOnly] {
            let mut rng = Rng(seed);
            for index in 0..CASES_PER_SEED {
                let case = generate(&mut rng, stream);
                if !runs_here(&case) {
                    continue;
                }
                let name = format!("{seed:#x}:{stream:?}:{index}");
                if only.as_ref().is_some_and(|only| *only != name) {
                    continue;
                }
                cases += 1;
                pump_cases += usize::from(case.config.driver == Driver::WindowsPump);
                if let Err(err) = run_case(&case, seed ^ index as u64) {
                    failures.push(format!("case {name}: {err}"));
                }
            }
        }
    }
    if let Ok(path) = std::env::var("HERDR_HOLD_PROPERTY_DUMP") {
        let _ = std::fs::write(path, failures.join("\n"));
    }
    if only.is_none() {
        assert!(cases >= 20_000, "{cases} cases");
        assert!(pump_cases >= 12_000, "{pump_cases} pump cases");
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} cases failed; first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The oracle restates the ADR-0012 bounds; they must be the framer's.
#[test]
fn oracle_bounds_match_the_adr() {
    assert_eq!(REPORT_SPAN, REPORT_TAIL_LIMIT.as_millis() as u64);
    assert_eq!(COLOR_SPAN, COLOR_REPLY_TAIL_LIMIT.as_millis() as u64);
    assert_eq!(BURST, ORPHAN_TAIL_BURST.as_millis() as u64);
    assert_eq!(AGE_LIMIT, PENDING_INPUT_AGE_LIMIT.as_millis() as u64);
    assert_eq!(REPORT_BYTES, MAX_DISCARDED_CONTROL_TAIL_BYTES);
}

fn unit(bytes: &[u8], rule: Rule) -> Unit {
    Unit {
        bytes: bytes.to_vec(),
        rule,
    }
}

fn events(list: &[&[u8]]) -> Vec<Vec<u8>> {
    list.iter().map(|event| event.to_vec()).collect()
}

#[test]
fn held_input_property_oracle_rejects_loss_reorder_leaks_and_literal_keys() {
    let expected = [
        unit(b"a", Rule::Keep),
        unit(b"x", Rule::Drop),
        unit(b"b", Rule::Either),
        unit(b"c", Rule::Keep),
    ];
    assert!(check_output(&expected, &events(&[b"abc"])).is_ok());
    assert!(check_output(&expected, &events(&[b"a", b"c"])).is_ok());
    assert!(check_output(&expected, &events(&[b"a"])).is_err(), "loss");
    assert!(
        check_output(&expected, &events(&[b"c", b"a"])).is_err(),
        "reorder"
    );
    assert!(check_output(&expected, &events(&[b"axc"])).is_err(), "leak");
    assert!(
        check_output(&expected, &events(&[b"aac"])).is_err(),
        "duplicate"
    );

    // A key reaches the pane as that key, never as literal text.
    let delete = [
        unit(b"a", Rule::Keep),
        unit(
            b"\x1b[3~",
            Rule::OneOf(vec![events(&[b"\x1b[3~"]), events(&[b"\x1b"])]),
        ),
    ];
    assert!(check_output(&delete, &events(&[b"a", b"\x1b[3~"])).is_ok());
    assert!(check_output(&delete, &events(&[b"a", b"\x1b"])).is_ok());
    assert!(
        check_output(&delete, &events(&[b"a", b"\x1b", b"[3~"])).is_err(),
        "literal tail"
    );
    assert!(
        check_output(&delete, &events(&[b"a\x1b[3~"])).is_err(),
        "key merged into text"
    );
    let semantic = [
        unit(b"a", Rule::Keep),
        unit(&[SEMANTIC], Rule::KeepEvent),
        unit(b"b", Rule::Keep),
    ];
    assert!(check_output(&semantic, &events(&[b"a", &[SEMANTIC], b"b"])).is_ok());
    assert!(
        check_output(&semantic, &events(&[&[SEMANTIC], b"a", b"b"])).is_err(),
        "overtaken"
    );
}

/// The oracle decodes the way production does: the Windows pump decodes one
/// event per chunk, so a chunk carrying two events loses the second.
#[test]
fn held_input_property_decode_reports_an_unconsumed_suffix_as_loss() {
    let chunks = vec![b"a\x1b[A".to_vec()];
    let expected = [unit(b"a", Rule::Keep), unit(b"\x1b[A", Rule::KeepEvent)];
    #[cfg(unix)]
    assert!(check_output(&expected, &decode(Driver::UnixReader, chunks.clone())).is_ok());
    assert!(
        check_output(&expected, &decode(Driver::WindowsPump, chunks)).is_err(),
        "one event per chunk"
    );
}

fn typed(chunks: Vec<Vec<u8>>) -> Vec<u8> {
    decode(Driver::WindowsPump, chunks).concat()
}

/// Found by the property test: `ESC [` / `ESC O` followed by ESC in one read
/// was parsed as one sequence, losing Alt+[ / Alt+O and the key after it.
#[test]
fn property_found_alt_introducer_before_escape_led_key_is_kept() {
    for input in [
        b"\x1b[\x1bl".as_slice(),
        b"\x1b[\x1b[A",
        b"\x1bO\x1b[A",
        b"\x1bO\x1b",
    ] {
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        let mut chunks = framer.push(input);
        chunks.extend(framer.flush_timeout());
        assert_eq!(typed(chunks), input.to_vec(), "{:?}", input.escape_ascii());
    }
}

/// Found by the property test: while a colour reply is awaited, `ESC [` was
/// released through the key parser without a tail hold, so the rest of a
/// split mouse report was typed.
#[test]
fn property_found_alt_bracket_released_while_reply_awaited_holds_mouse_tail() {
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.host_color_query_sent();
    let mut chunks = framer.push(b"\x1b[");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"<32;209;103me"));
    assert_eq!(typed(chunks), b"\x1b[e".to_vec());
}

/// Found by the property test: an OSC 4 reply whose header was dropped at
/// its idle timeout, split between the ESC and `\` of its ST, typed `\`.
#[test]
fn property_found_split_st_after_dropped_osc_header_is_dropped() {
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.host_color_query_sent();
    let mut chunks = framer.push(b"\x1b]4;81;rgb:0cd4/54fa/b7bc\x1b");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"\\x"));
    assert_eq!(typed(chunks), b"x".to_vec());
}

fn at(framer: &mut RawInputByteFramer, base: Instant, ms: u64) {
    framer.set_test_clock(base + Duration::from_millis(ms));
}

/// Found by the property test (ADR-0012): with escape disambiguation, an
/// ESC held for a delayed mouse tail (#3480) and then followed by the rest
/// of a split key in one read went out as Escape and typed `[H`.
#[cfg(unix)]
#[test]
fn property_found_disambiguated_escape_released_by_split_key_tail_drops_tail() {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    framer.set_host_escape_disambiguation_active(true);
    let mut chunks = framer.push(b"\x1b");
    at(&mut framer, base, 150);
    chunks.extend(framer.flush_timeout());
    at(&mut framer, base, 399);
    chunks.extend(framer.push(b"[H"));
    assert_eq!(decode(Driver::UnixReader, chunks), vec![b"\x1b".to_vec()]);
    assert!(!framer.has_pending_input());
}

/// Found by the property test (ADR-0012): once the Windows default-colour
/// window had ended, a split OSC reply prefix was dropped without holding
/// its tail, so the tail was typed (#5251).
#[test]
fn property_found_reply_prefix_after_default_color_window_holds_its_tail() {
    for (prefix, tail) in [
        (
            b"\x1b]".as_slice(),
            b"4;8;rgb:effd/ee09/a92f\x1b\\a".as_slice(),
        ),
        (b"\x1b]10;rgb:23b8/85e0", b"/cec5\x07a"),
    ] {
        let base = Instant::now();
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        framer.set_test_clock(base);
        framer.host_default_color_query_sent(Duration::from_secs(1));
        at(&mut framer, base, 1_010);
        let mut chunks = framer.flush_timeout();
        at(&mut framer, base, 1_500);
        framer.host_color_query_sent();
        chunks.extend(framer.push(prefix));
        at(&mut framer, base, 1_510);
        chunks.extend(framer.flush_timeout());
        at(&mut framer, base, 1_550);
        chunks.extend(framer.push(tail));
        at(&mut framer, base, 1_560);
        chunks.extend(framer.flush_timeout());
        assert_eq!(typed(chunks), b"a".to_vec(), "{:?}", prefix.escape_ascii());
        assert!(!framer.has_pending_input());
    }
}

/// Found by the property test (ADR-0012): a UTF-8 continuation byte whose
/// lead byte was released by the pending-input bound took the input after
/// it in the same read with it.
#[test]
fn property_found_orphan_utf8_continuation_drops_only_itself() {
    let base = Instant::now();
    let mut framer = RawInputByteFramer::with_host_input_policy(false);
    framer.set_test_clock(base);
    let mut chunks = framer.push(b"\xe4\xbd");
    at(&mut framer, base, 10);
    chunks.extend(framer.flush_timeout());
    at(&mut framer, base, 1_600);
    chunks.extend(framer.flush_timeout());
    assert!(!framer.has_pending_input(), "released by the bound");
    at(&mut framer, base, 1_700);
    chunks.extend(framer.push(b"\xa0/"));
    at(&mut framer, base, 1_710);
    chunks.extend(framer.flush_timeout());
    assert_eq!(typed(chunks), b"/".to_vec());
}

/// The native Windows CI red on e0e6d7e6: seed 0x0012_0002 case 404 and seed
/// 0x0012_0004 case 779 of the Windows stream, which `Stream::PumpOnly`
/// reproduces on every OS (ADR-0013 D5).
#[test]
fn held_input_property_native_windows_cases_404_and_779() {
    for (seed, index) in [(0x0012_0002_u64, 404_usize), (0x0012_0004, 779)] {
        let mut rng = Rng(seed);
        let mut case = generate(&mut rng, Stream::PumpOnly);
        for _ in 0..index {
            case = generate(&mut rng, Stream::PumpOnly);
        }
        assert_eq!(case.config.driver, Driver::WindowsPump);
        if let Err(err) = run_case(&case, seed ^ index as u64) {
            panic!("seed {seed:#x} case {index}: {err}");
        }
    }
}

/// A case of typed reads after a released Escape, through the Unix reader.
fn escape_then_typed(reads: &[(u64, &[u8])]) -> Case {
    let mut input = vec![ESC];
    let mut case_reads = vec![Read {
        gap: 0,
        bytes: vec![ESC],
        color_queries: 0,
        wait_for_flush: false,
        semantic: false,
    }];
    for (index, (gap, bytes)) in reads.iter().enumerate() {
        input.extend_from_slice(bytes);
        case_reads.push(Read {
            gap: *gap,
            bytes: bytes.to_vec(),
            color_queries: 0,
            wait_for_flush: index == 0,
            semantic: false,
        });
    }
    let mut kinds = vec![Kind::Ambiguous];
    kinds.extend(std::iter::repeat_n(Kind::Text, input.len() - 1));
    let mut case = Case {
        config: Config {
            driver: Driver::UnixReader,
            mouse_capture: false,
            color_query: false,
            default_color_query: false,
            disambiguation: false,
            graphics_wait: None,
        },
        reads: case_reads,
        expected: per_byte(&input, Rule::Keep),
        paste_end: vec![None; input.len()],
        kinds,
        token_starts: vec![0, 1],
        input,
    };
    mark_released_prefix_tails(&mut case);
    case
}

/// codex f5251-r4 finding 4 (ADR-0013 D6): typing ambiguity is judged on
/// arrival times. Escape released at 10 ms, `[` typed at 200 ms and `D` at
/// 280 ms: 80 ms apart, not one burst, so both must reach the pane, and an
/// implementation that drops them fails. As one read, they may be dropped.
#[test]
fn oracle_released_prefix_tail_is_judged_on_arrival_times() {
    let case = escape_then_typed(&[(200, b"["), (80, b"D")]);
    assert!(case.expected[1..]
        .iter()
        .all(|unit| matches!(unit.rule, Rule::Deferred { .. })));
    let mut harness = Harness::new(&case, 0);
    harness.run_unix_reader().unwrap();
    let expected = harness.resolve();
    assert_eq!(expected[1].rule, Rule::Keep);
    assert_eq!(expected[2].rule, Rule::Keep);
    assert!(check_output(&expected, &harness.out).is_ok());
    assert!(check_output(&expected, &events(&[b"\x1b"])).is_err());

    let burst = escape_then_typed(&[(200, b"[D")]);
    let mut harness = Harness::new(&burst, 0);
    harness.run_unix_reader().unwrap();
    let expected = harness.resolve();
    assert_eq!(expected[1].rule, Rule::Either);
    assert!(check_output(&expected, &events(&[b"\x1b"])).is_ok());
}

/// codex f5251-r4 finding 4: a report cut by a semantic record is not
/// blanket `Either`. An SGR report whose prefix stayed held through the
/// record may be typed (#4048) but not lost, and the bytes after the record
/// are typed.
#[test]
fn oracle_interrupted_report_is_not_blanket_either() {
    let bytes = b"\x1b[<0;1;1M";
    let cuts = [Cut { at: 4, gap: 20 }];
    let config = Config {
        driver: Driver::WindowsPump,
        mouse_capture: false,
        color_query: false,
        default_color_query: false,
        disambiguation: false,
        graphics_wait: None,
    };
    let units = interrupted_report_rule(Kind::Sgr, bytes, &cuts, 4, &config);
    assert!(units.iter().all(|unit| unit.rule != Rule::Either));
    let mut expected = units[..1].to_vec();
    expected.push(unit(&[SEMANTIC], Rule::KeepEvent));
    expected.extend_from_slice(&units[1..]);
    // Dropped before the record, then the rest typed: allowed.
    let after: Vec<&[u8]> = bytes[4..].iter().map(std::slice::from_ref).collect();
    let mut out = vec![vec![SEMANTIC]];
    out.extend(after.iter().map(|byte| byte.to_vec()));
    assert!(check_output(&expected, &out).is_ok());
    // The bytes after the record lost: rejected.
    assert!(check_output(&expected, &events(&[&[SEMANTIC]])).is_err());
}

/// codex f5251-r4 findings 3 and 4: the generators reach the paths the
/// harness used to miss, a complete kitty APC introducer, a graphics stream
/// of many small reads (reads that bring the framer nothing), and both
/// generator streams.
#[test]
fn property_generators_cover_graphics_streams_and_both_streams() {
    let mut graphics = 0;
    let mut streams = 0;
    let mut pump_only = 0;
    for seed in SEEDS {
        for stream in [Stream::Mixed, Stream::PumpOnly] {
            let mut rng = Rng(seed);
            for _ in 0..CASES_PER_SEED {
                let case = generate(&mut rng, stream);
                let starts = case
                    .input
                    .windows(GRAPHICS_PREFIX.len())
                    .filter(|window| *window == GRAPHICS_PREFIX)
                    .count();
                graphics += starts;
                streams += usize::from(
                    starts > 0
                        && case
                            .reads
                            .iter()
                            .filter(|read| read.bytes.len() < 25)
                            .count()
                            > 20,
                );
                pump_only += usize::from(
                    stream == Stream::PumpOnly && case.config.driver == Driver::WindowsPump,
                );
            }
        }
    }
    assert!(graphics > 100, "{graphics} graphics commands");
    assert!(streams > 50, "{streams} graphics streams");
    assert_eq!(pump_only, SEEDS.len() * CASES_PER_SEED);
}
