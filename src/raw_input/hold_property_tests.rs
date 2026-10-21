//! Property test for the raw-input hold (ADR-0012, herdr-nxa8; ADR-0010,
//! herdr-f2a1; upstream #5251).
//!
//! Generates input streams that mix typed text, Escape-led keys, Alt chords,
//! bracketed pastes, SGR and X10 mouse reports and OSC 10/11/4 colour replies
//! (some longer than any reply the framer accepts), cuts each token into up
//! to six reads at random times, and on Windows interleaves semantic key
//! records, also between the reads of one token. It drives the byte framer
//! the way the Unix stdin reader (with the direct-graphics filter's response
//! wait) or the Windows pump does, under a virtual clock.
//!
//! The oracle is independent of the framer:
//! - what reached the pane is decoded the way production decodes it (the
//!   Unix server decodes every event of a chunk, the Windows pump one event
//!   per chunk), so a suffix production does not consume is a loss;
//! - expectations are per event, so a key that arrives as literal text does
//!   not match the key;
//! - the age of every pending byte is measured from the harness's own record
//!   of when it reached the framer, against the ADR's named bounds.
//!
//! Invariants (ADR-0012):
//! - I1 every byte that is not part of a report reaches the pane exactly
//!   once, in order, in the event production decodes it into;
//! - I2 a report the context says to drop does not reach the pane, and a key
//!   reaches it as that key or (split after a released prefix) not at all;
//! - I3 no pending byte except an unterminated paste is older than the
//!   pending-input bound, and the stream drains;
//! - I4 a semantic key never overtakes an earlier byte.

use super::*;
use std::time::Instant;

/// Fixed seeds; every run generates the same cases.
const SEEDS: [u64; 4] = [0x0012_0001, 0x0012_0002, 0x0012_0003, 0x0012_0004];
const CASES_PER_SEED: usize = 3_000;
/// Stands for a semantic key (Windows key record) in the expected and the
/// observed stream. Never valid in UTF-8, so no generated input contains it.
const SEMANTIC: u8 = 0xff;

// The ADR-0012 bounds table, restated so the oracle does not read the
// framer's own deadlines (`oracle_bounds_match_the_adr` ties them together).
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
    Semantic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Driver {
    #[cfg(unix)]
    UnixReader,
    WindowsPump,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    driver: Driver,
    /// Selects the Unix reader's idle waits; the Windows pump ignores it.
    #[cfg(unix)]
    mouse_capture: bool,
    color_query: bool,
    /// The Windows default-colour query (1 s reply window) is outstanding
    /// at the start.
    default_color_query: bool,
    #[cfg(unix)]
    disambiguation: bool,
    /// The Unix reader awaits a direct-graphics response in this window
    /// (ms): the graphics filter keeps a trailing `ESC` / `ESC _`.
    #[cfg(unix)]
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

fn token(rng: &mut Rng, driver: Driver) -> (Kind, Vec<u8>) {
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
        _ if driver == Driver::WindowsPump => (Kind::Semantic, Vec::new()),
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

/// What a token that was cut but not interrupted may become (ADR-0012 D3).
fn cut_token_rule(kind: Kind, bytes: &[u8], cuts: &[Cut], config: &Config) -> Vec<Unit> {
    let span: u64 = cuts.iter().map(|cut| cut.gap).sum();
    let first = cuts.first().copied();
    let later: u64 = cuts.iter().skip(1).map(|cut| cut.gap).sum();
    let every = |rule: Rule| -> Vec<Unit> {
        bytes
            .iter()
            .map(|byte| Unit {
                bytes: vec![*byte],
                rule: rule.clone(),
            })
            .collect()
    };
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
            #[cfg(unix)]
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
            if !inside && first.gap < REPORT_SPAN && later <= BURST {
                let mut alternatives = vec![Vec::new()];
                alternatives.extend(released_prefixes());
                return one(Rule::OneOf(alternatives));
            }
            spread()
        }
        Kind::Color => {
            if first.is_some() && config.default_color_query {
                // A reply split across the end of the Windows default-colour
                // window and its 100 ms recovery types its tail (ADR-0012
                // Residuals; unchanged by this ADR).
                return every(Rule::Either);
            }
            if first.is_none() || (config.color_query && span < COLOR_SPAN) {
                // A reply split right after its ESC or `ESC ]` can be no
                // other way: that prefix may go out at its idle timeout and
                // only the tail is dropped (ADR-0010 Consequences).
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
        }
        Kind::LongColor => every(Rule::Either),
        Kind::Semantic => unreachable!(),
    }
}

/// The offset of the last cut after which every context of the token has
/// closed: its prefix was released within the pending-input bound (plus a
/// Windows tick) and the longest tail limit has passed since. The graphics
/// filter can delay the prefix itself past that, so it has no late cut.
fn late_cut(cuts: &[Cut], config: &Config) -> Option<usize> {
    #[cfg(unix)]
    if config.graphics_wait.is_some() {
        return None;
    }
    let _ = config;
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
/// key that arrives in one read (ADR-0012 Residuals): typed bytes that,
/// after that prefix, form a CSI key may be dropped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleasedPrefixScan {
    None,
    AfterEscape,
    InCsi,
}

impl ReleasedPrefixScan {
    fn apply(&mut self, unit: &mut Unit) {
        let byte = unit.bytes[0];
        let either = match *self {
            Self::None => false,
            Self::AfterEscape if byte == b'[' => {
                *self = Self::InCsi;
                true
            }
            Self::InCsi if (0x20..=0x3f).contains(&byte) => true,
            Self::InCsi if (0x40..=0x7e).contains(&byte) => {
                *self = Self::None;
                true
            }
            _ => {
                *self = Self::None;
                false
            }
        };
        if either && unit.rule == Rule::Keep {
            unit.rule = Rule::Either;
        }
    }
}

fn generate(rng: &mut Rng) -> Case {
    #[cfg(unix)]
    let driver = if rng.chance(1, 2) {
        Driver::UnixReader
    } else {
        Driver::WindowsPump
    };
    #[cfg(not(unix))]
    let driver = Driver::WindowsPump;
    let default_color_query = driver == Driver::WindowsPump && rng.chance(1, 4);
    #[cfg(unix)]
    let graphics_wait = (driver == Driver::UnixReader && rng.chance(1, 4)).then(|| {
        let start = rng.range(0, 3_000);
        (start, start + rng.range(100, 4_000))
    });
    let config = Config {
        driver,
        #[cfg(unix)]
        mouse_capture: rng.chance(1, 2),
        color_query: default_color_query || rng.chance(1, 2),
        default_color_query,
        #[cfg(unix)]
        disambiguation: driver == Driver::UnixReader && rng.chance(1, 4),
        #[cfg(unix)]
        graphics_wait,
    };

    let mut reads: Vec<Read> = Vec::new();
    let mut expected: Vec<Unit> = Vec::new();
    let mut paste_end = Vec::new();
    let mut previous_ambiguous = false;
    let mut scan = ReleasedPrefixScan::None;
    for _ in 0..rng.range(3, 14) {
        let (kind, bytes) = token(rng, driver);
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
            scan = ReleasedPrefixScan::None;
            continue;
        }

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
        // A cut delivered in the same read as the bytes before it is no cut.
        let cuts: Vec<Cut> = offsets
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
        let wait_for_flush = previous_ambiguous && bytes.first() != Some(&ESC);
        previous_ambiguous = kind == Kind::Ambiguous;

        let mut units = match interrupt_at {
            None => cut_token_rule(kind, &bytes, &cuts, &config),
            Some(at) => {
                // An interruption ends every hold: what was held is input
                // (typed bytes) or a fragment that cannot be input (ADR-0012
                // D1). A report cut by a record is outside the promise.
                let mut units: Vec<Unit> = bytes
                    .iter()
                    .map(|byte| Unit {
                        bytes: vec![*byte],
                        rule: match kind {
                            Kind::Text | Kind::AltChord | Kind::Ambiguous => Rule::Keep,
                            _ => Rule::Either,
                        },
                    })
                    .collect();
                mark_late_utf8(&mut units, &bytes, &cuts, Some(at));
                units
            }
        };
        if kind == Kind::Text {
            for unit in &mut units {
                scan.apply(unit);
            }
        } else {
            scan = match bytes.as_slice() {
                [ESC] if kind == Kind::Ambiguous => ReleasedPrefixScan::AfterEscape,
                [ESC, b'['] if kind == Kind::Ambiguous => ReleasedPrefixScan::InCsi,
                _ => ReleasedPrefixScan::None,
            };
        }
        let token_start = paste_end.len();
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
    Case {
        config,
        reads,
        expected,
        paste_end,
    }
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
        #[cfg(unix)]
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
/// trailing prefix of `ESC _ G` is kept, and released once none is awaited.
#[cfg(unix)]
struct GraphicsFilter {
    window: Option<(u64, u64)>,
    pending: Vec<u8>,
}

#[cfg(unix)]
impl GraphicsFilter {
    fn interested(&self, now: u64) -> bool {
        self.window
            .is_some_and(|(start, end)| (start..end).contains(&now))
    }

    fn push(&mut self, bytes: &[u8], now: u64) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        if !self.interested(now) {
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
    /// Typed events, in the order they reached the pane.
    out: Vec<Vec<u8>>,
    /// When each run of bytes reached the framer: (end offset, ms).
    pushed: Vec<(usize, u64)>,
    pushed_len: usize,
    #[cfg(unix)]
    filter: GraphicsFilter,
    rng: Rng,
}

impl<'a> Harness<'a> {
    fn new(case: &'a Case, seed: u64) -> Self {
        let base = Instant::now();
        let mut framer = RawInputByteFramer::with_host_input_policy(false);
        framer.set_test_clock(base);
        #[cfg(unix)]
        if case.config.disambiguation {
            framer.set_host_escape_disambiguation_active(true);
        }
        if case.config.default_color_query {
            framer.host_default_color_query_sent(Duration::from_secs(1));
        } else if case.config.color_query {
            framer.host_color_query_sent();
        }
        Self {
            case,
            next: 0,
            base,
            now: 0,
            last_delivery: 0,
            framer,
            out: Vec::new(),
            pushed: Vec::new(),
            pushed_len: 0,
            #[cfg(unix)]
            filter: GraphicsFilter {
                window: case.config.graphics_wait,
                pending: Vec::new(),
            },
            rng: Rng(seed),
        }
    }

    /// I3, measured from the harness's own record: the age of the oldest
    /// byte the framer still holds, unless it is part of a bracketed paste
    /// whose end has not arrived. `slack` allows the reader's round-up.
    fn check_age(&self, what: &str, slack: u64) -> Result<(), String> {
        let pending = self.framer.buffer.len().min(self.pushed_len);
        if pending == 0 {
            return Ok(());
        }
        let oldest = self.pushed_len - pending;
        if self.case.paste_end[oldest].is_some_and(|end| end > self.pushed_len) {
            return Ok(());
        }
        let arrived = self
            .pushed
            .iter()
            .find(|(end, _)| *end > oldest)
            .map_or(self.now, |(_, at)| *at);
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
            #[cfg(unix)]
            Driver::UnixReader => 2,
            Driver::WindowsPump => WINDOWS_TICK_MS,
        };
        self.check_age("waking", slack)
    }

    fn emit(&mut self, chunks: Vec<Vec<u8>>) {
        let events = decode(self.case.config.driver, chunks);
        self.out.extend(events);
    }

    fn flush(&mut self) -> Result<(), String> {
        let chunks = self.framer.flush_timeout();
        self.check_age("after flush_timeout", 0)?;
        self.emit(chunks);
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !bytes.is_empty() {
            self.pushed_len += bytes.len();
            self.pushed.push((self.pushed_len, self.now));
        }
        let chunks = self.framer.push(bytes);
        self.check_age("after push", 0)?;
        self.emit(chunks);
        Ok(())
    }

    #[cfg(unix)]
    fn filter_pending(&self) -> bool {
        !self.filter.pending.is_empty()
    }

    #[cfg(not(unix))]
    fn filter_pending(&self) -> bool {
        false
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

    /// Delivers the next read. Returns its bytes after the graphics filter,
    /// or None for a semantic record.
    fn deliver(&mut self) -> Result<Option<Vec<u8>>, String> {
        let case = self.case;
        let read = &case.reads[self.next];
        let at = self.arrival().ok_or("delivering a blocked read")?;
        self.advance_to(at)?;
        self.next += 1;
        self.last_delivery = self.now;
        if read.semantic {
            let chunks = self.framer.flush_interrupted();
            // I4: nothing is left to follow the record.
            let pending = self.framer.buffer.len();
            if pending > 0 && !self.framer.has_pending_bracketed_paste() {
                return Err(format!(
                    "I4: {pending} bytes pending behind a semantic key at {} ms",
                    self.now
                ));
            }
            self.check_age("after flush_interrupted", 0)?;
            self.emit(chunks);
            self.out.push(vec![SEMANTIC]);
            return Ok(None);
        }
        for _ in 0..read.color_queries {
            self.framer.host_color_query_sent();
        }
        #[cfg(unix)]
        let bytes = if self.case.config.driver == Driver::UnixReader {
            let now = self.now;
            self.filter.push(&read.bytes, now)
        } else {
            read.bytes.clone()
        };
        #[cfg(not(unix))]
        let bytes = read.bytes.clone();
        self.push(&bytes)?;
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

    /// The Unix stdin reader loop of `client/input.rs`, with `read()` and
    /// `stdin_read_ready` answered from the schedule: one wait per turn
    /// (`reader_idle_wait`) over the idle flush, the framer's release time
    /// and the graphics filter's poll.
    #[cfg(unix)]
    fn run_unix_reader(&mut self) -> Result<(), String> {
        let mut idle_flush_at: Option<u64> = None;
        for _ in 0..100_000 {
            let idle = idle_flush_at
                .filter(|_| self.framer.has_pending_input())
                .map(|at| at.saturating_sub(self.now) as i32);
            let framer_wait = match (idle, self.framer.held_input_release_ms()) {
                (Some(idle), Some(release)) => Some(idle.min(release)),
                (idle, release) => idle.or(release),
            };
            if let Some((wait, framer_due)) = reader_idle_wait(framer_wait, self.filter_pending()) {
                let due = self.now + wait.max(0) as u64;
                if !self.arrives_by(due) {
                    self.advance_to(due)?;
                    let now = self.now;
                    if let Some(data) = self.filter.flush_if_inactive(now) {
                        self.push(&data)?;
                        idle_flush_at = Some(
                            self.now
                                + self
                                    .framer
                                    .idle_flush_timeout_ms(self.case.config.mouse_capture)
                                    as u64,
                        );
                    } else if framer_due {
                        idle_flush_at = None;
                        self.flush()?;
                    }
                    continue;
                }
            }
            if self.next == self.case.reads.len() {
                return self.finish();
            }
            if self.arrival().is_none() {
                return Err(self.stranded());
            }
            self.deliver()?;
            idle_flush_at = Some(
                self.now
                    + self
                        .framer
                        .idle_flush_timeout_ms(self.case.config.mouse_capture)
                        as u64,
            );
        }
        Err("reader loop did not terminate".to_string())
    }

    /// The Windows pump: `push` per read (sometimes followed by an immediate
    /// `flush_timeout`, as a non-key record does), `flush_timeout` on every
    /// idle tick, `flush_interrupted` before a semantic key. Idle ticks that
    /// cannot change anything are skipped: after three quiet ticks the next
    /// one runs when the framer's state can next change with time.
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
            if self.deliver()?.is_some() && self.rng.chance(1, 4) {
                self.flush()?;
            }
        }
        Err("pump loop did not terminate".to_string())
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

fn describe(case: &Case) -> String {
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
    let expected: String = case
        .expected
        .iter()
        .map(|unit| {
            let bytes = unit.bytes.escape_ascii().to_string();
            match &unit.rule {
                Rule::Keep => bytes,
                Rule::KeepEvent => format!("[{bytes}]"),
                Rule::Drop => format!("-({bytes})"),
                Rule::Either => format!("?{bytes}"),
                Rule::OneOf(_) => format!("{{{bytes}}}"),
            }
        })
        .collect();
    format!(
        "{:?}\n  reads: {reads:?}\n  expected: {expected}",
        case.config
    )
}

fn run_case(case: &Case, seed: u64) -> Result<(), String> {
    let mut harness = Harness::new(case, seed);
    match case.config.driver {
        #[cfg(unix)]
        Driver::UnixReader => harness.run_unix_reader(),
        Driver::WindowsPump => harness.run_windows_pump(),
    }
    .and_then(|()| check_output(&case.expected, &harness.out))
    .map_err(|err| {
        let out: Vec<String> = harness
            .out
            .iter()
            .map(|event| event.escape_ascii().to_string())
            .collect();
        format!("{err}\n  out: {out:?}")
    })
}

#[test]
fn held_input_property_every_byte_once_in_order_and_bounded() {
    let mut failures = Vec::new();
    let mut cases = 0;
    for seed in SEEDS {
        let mut rng = Rng(seed);
        for index in 0..CASES_PER_SEED {
            let case = generate(&mut rng);
            cases += 1;
            if let Err(err) = run_case(&case, seed ^ index as u64) {
                failures.push(format!(
                    "seed {seed:#x} case {index}: {err}\n  {}",
                    describe(&case)
                ));
            }
        }
    }
    assert_eq!(cases, SEEDS.len() * CASES_PER_SEED);
    assert!(cases >= 10_000);
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
