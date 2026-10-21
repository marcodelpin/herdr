//! Property test for the raw-input hold (ADR-0010, herdr-f2a1, upstream #5251).
//!
//! Generates input streams that mix typed text, Escape-led keys, Alt chords,
//! bracketed pastes, SGR and X10 mouse reports and OSC 10/11/4 colour replies,
//! splits them into reads at random times, and drives the byte framer the way
//! the Unix stdin reader or the Windows pump does, under a virtual clock. The
//! oracle checks the ADR invariants on bytes:
//! - I1 every byte that is not part of a report reaches the pane exactly once,
//!   in order;
//! - I2 a report the context says to drop does not reach the pane;
//! - I3 no hold outlives its deadline, and the stream drains;
//! - I4 a semantic key never overtakes an earlier byte.

use super::*;
use std::time::Instant;

/// Fixed seeds; every run generates the same cases.
const SEEDS: [u64; 4] = [0x5251_0001, 0x5251_0002, 0x5251_0003, 0x5251_0004];
const CASES_PER_SEED: usize = 2_500;
/// Stands for a semantic key (Windows key record) in the expected and the
/// observed stream. Never valid in UTF-8, so no generated input contains it.
const SEMANTIC: u8 = 0xff;
/// A report spread over less than this is a split report the framer must
/// still recognise (it is the mouse tail bound).
const MOUSE_SPAN: u64 = MOUSE_REPORT_TAIL_LIMIT.as_millis() as u64;
/// The same for a colour reply while a colour query is outstanding.
const COLOR_SPAN: u64 = COLOR_REPLY_TAIL_LIMIT.as_millis() as u64;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Must reach the pane exactly once, in order.
    Keep,
    /// Must not reach the pane.
    Drop,
    /// Either: the bytes are outside what the framer promises (a report spread
    /// past its bound, a residual listed in the ADR).
    Either,
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
    #[cfg(unix)]
    disambiguation: bool,
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
    expected: Vec<(u8, Class)>,
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

fn color_reply(rng: &mut Rng) -> Vec<u8> {
    let header = match rng.below(3) {
        0 => "]10;".to_string(),
        1 => "]11;".to_string(),
        _ => format!("]4;{};", rng.below(256)),
    };
    let terminator: &[u8] = if rng.chance(1, 2) { b"\x07" } else { b"\x1b\\" };
    let mut reply = vec![ESC];
    reply.extend_from_slice(header.as_bytes());
    reply.extend_from_slice(format!("rgb:{}/{}/{}", hex4(rng), hex4(rng), hex4(rng)).as_bytes());
    reply.extend_from_slice(terminator);
    reply
}

fn token(rng: &mut Rng, driver: Driver) -> (Kind, Vec<u8>) {
    let roll = rng.below(100);
    match roll {
        0..=24 => (Kind::Text, typed_text(rng)),
        25..=34 => (Kind::Key, rng.pick(&KEYS).to_vec()),
        35..=46 => {
            let prefix: &[u8] = rng.pick(&[b"\x1b".as_slice(), b"\x1b[", b"\x1bO"]);
            (Kind::Ambiguous, prefix.to_vec())
        }
        47..=54 => (Kind::AltChord, vec![ESC, *rng.pick(ALT_CHORDS)]),
        55..=60 => {
            let mut paste = BRACKETED_PASTE_START.to_vec();
            paste.extend(typed_text(rng));
            paste.extend_from_slice(BRACKETED_PASTE_END);
            (Kind::Paste, paste)
        }
        61..=75 => {
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
        76..=80 => {
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
        81..=92 => (Kind::Color, color_reply(rng)),
        _ if driver == Driver::WindowsPump => (Kind::Semantic, Vec::new()),
        _ => (Kind::Text, typed_text(rng)),
    }
}

fn gap(rng: &mut Rng) -> u64 {
    match rng.below(20) {
        0..=5 => 0,
        6..=7 => rng.range(1, 9),
        8..=10 => rng.range(10, 60),
        11..=13 => rng.range(100, 450),
        14..=16 => rng.range(450, 560),
        17..=18 => rng.range(560, 999),
        _ => rng.range(1000, 1300),
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
    let config = Config {
        driver,
        #[cfg(unix)]
        mouse_capture: rng.chance(1, 2),
        color_query: rng.chance(1, 2),
        #[cfg(unix)]
        disambiguation: driver == Driver::UnixReader && rng.chance(1, 4),
    };
    // A split inside a keyboard CSI longer than this drops its prefix (ADR
    // residual): the Unix reader waits the split-read timeout, the Windows
    // pump flushes on its next tick.
    let key_split_limit = match driver {
        #[cfg(unix)]
        Driver::UnixReader => INCOMPLETE_SEQUENCE_FLUSH_TIMEOUT_MS as u64,
        Driver::WindowsPump => 1,
    };

    let mut reads: Vec<Read> = Vec::new();
    let mut expected = Vec::new();
    let mut previous_ambiguous = false;
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
            expected.push((SEMANTIC, Class::Keep));
            previous_ambiguous = false;
            continue;
        }

        // Cut into up to three pieces at distinct offsets.
        let mut cuts: Vec<usize> = (0..rng.below(3))
            .filter(|_| bytes.len() > 1)
            .map(|_| rng.range(1, bytes.len() as u64 - 1) as usize)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut gaps = Vec::new();
        let mut pieces = Vec::new();
        let mut start = 0;
        for cut in cuts.iter().copied().chain(std::iter::once(bytes.len())) {
            pieces.push(bytes[start..cut].to_vec());
            gaps.push(gap(rng));
            start = cut;
        }
        let inner: Vec<(usize, u64)> = cuts
            .iter()
            .copied()
            .zip(gaps[1..].iter().copied())
            .collect();
        let span: u64 = inner.iter().map(|(_, gap)| gap).sum();
        let atomic = span == 0;

        // After Escape, Alt+[ or Alt+O, the user types a byte that is not
        // ESC-led only once the key reached the pane: before that it would be
        // another key (`ESC a` is Alt+a), which is not what the oracle models.
        let wait_for_flush = previous_ambiguous && bytes.first() != Some(&ESC);
        previous_ambiguous = kind == Kind::Ambiguous;

        let class = match kind {
            Kind::Text | Kind::AltChord | Kind::Ambiguous => Class::Keep,
            Kind::Key => {
                let dropped_prefix = inner
                    .iter()
                    .any(|&(at, gap)| gap >= key_split_limit && at >= 3);
                if dropped_prefix {
                    Class::Either
                } else {
                    Class::Keep
                }
            }
            Kind::Paste => Class::Keep,
            Kind::Sgr if span < MOUSE_SPAN => Class::Drop,
            Kind::X10 if atomic => Class::Drop,
            Kind::Color if atomic || (config.color_query && span < COLOR_SPAN) => Class::Drop,
            Kind::Sgr | Kind::X10 | Kind::Color => Class::Either,
            Kind::Semantic => unreachable!(),
        };
        // A report split right after its ESC or `ESC [` can be no other way:
        // the framer releases that prefix as Escape / Alt+[ at its idle
        // timeout and drops only the tail (ADR-0010 Consequences).
        let released_prefix = match kind {
            Kind::Sgr | Kind::X10 | Kind::Color => {
                inner.iter().map(|&(at, _)| at).filter(|at| *at <= 2).max()
            }
            _ => None,
        };
        for (index, byte) in bytes.iter().enumerate() {
            let marker = index < BRACKETED_PASTE_START.len()
                || index >= bytes.len() - BRACKETED_PASTE_END.len();
            let byte_class = if (kind == Kind::Paste && marker)
                || released_prefix.is_some_and(|len| index < len)
            {
                Class::Either
            } else {
                class
            };
            expected.push((*byte, byte_class));
        }

        let queries = usize::from(kind == Kind::Color && config.color_query);
        for (index, (piece, gap)) in pieces.into_iter().zip(gaps).enumerate() {
            let first = index == 0;
            let gap = if first && wait_for_flush {
                gap.max(1)
            } else {
                gap
            };
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
    }
    Case {
        config,
        reads,
        expected,
    }
}

/// Bytes of the chunks that reach the pane as input (keys, text, pastes).
fn typed_into(out: &mut Vec<u8>, chunks: Vec<Vec<u8>>) {
    for chunk in chunks {
        let typed = events_from_framed_chunks(vec![chunk.clone()])
            .iter()
            .any(|event| {
                matches!(
                    event,
                    RawInputEvent::Key(_) | RawInputEvent::Text(_) | RawInputEvent::Paste(_)
                )
            });
        if typed {
            out.extend(chunk);
        }
    }
}

/// A driver: the framer, the virtual clock, and what reached the pane.
struct Harness<'a> {
    #[cfg(unix)]
    config: Config,
    reads: &'a [Read],
    next: usize,
    base: Instant,
    now: u64,
    last_delivery: u64,
    framer: RawInputByteFramer,
    out: Vec<u8>,
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
        if case.config.color_query {
            framer.host_color_query_sent();
        }
        Self {
            #[cfg(unix)]
            config: case.config,
            reads: &case.reads,
            next: 0,
            base,
            now: 0,
            last_delivery: 0,
            framer,
            out: Vec::new(),
            rng: Rng(seed),
        }
    }

    fn advance_to(&mut self, at: u64) {
        self.now = self.now.max(at);
        self.framer
            .set_test_clock(self.base + Duration::from_millis(self.now));
    }

    /// I3: after any framer call at time t, no hold has a deadline <= t.
    fn check_deadline(&self, what: &str) -> Result<(), String> {
        let now = self.base + Duration::from_millis(self.now);
        match self.framer.tail_hold_deadline() {
            Some(deadline) if deadline <= now => Err(format!(
                "I3: hold {:?} outlived its deadline after {what} at {} ms",
                self.framer.tail_hold, self.now
            )),
            _ => Ok(()),
        }
    }

    fn flush(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let chunks = self.framer.flush_timeout();
        self.check_deadline("flush_timeout")?;
        Ok(chunks)
    }

    /// When the next read arrives, or None if there is none or the user is
    /// still waiting for an earlier key to reach the pane.
    fn arrival(&self) -> Option<u64> {
        let read = self.reads.get(self.next)?;
        if read.wait_for_flush && self.framer.has_pending_input() {
            return None;
        }
        Some((self.last_delivery + read.gap).max(self.now))
    }

    fn arrives_by(&self, due: u64) -> bool {
        self.arrival().is_some_and(|at| at <= due)
    }

    fn deliver(&mut self) -> Result<&'a Read, String> {
        let read = &self.reads[self.next];
        let at = self.arrival().ok_or("delivering a blocked read")?;
        self.advance_to(at);
        self.next += 1;
        self.last_delivery = self.now;
        if read.semantic {
            let chunks = self.framer.flush_interrupted();
            self.check_deadline("flush_interrupted")?;
            typed_into(&mut self.out, chunks);
            self.out.push(SEMANTIC);
            return Ok(read);
        }
        for _ in 0..read.color_queries {
            self.framer.host_color_query_sent();
        }
        let chunks = self.framer.push(&read.bytes);
        self.check_deadline("push")?;
        typed_into(&mut self.out, chunks);
        Ok(read)
    }

    fn stranded(&self) -> String {
        format!(
            "I3: {} bytes pending with no deadline at {} ms: {:?}",
            self.framer.buffer.len(),
            self.now,
            self.framer.buffer.escape_ascii().to_string()
        )
    }

    /// The Unix stdin reader loop of `client/input.rs`, with `read()` and
    /// `stdin_read_ready` answered from the schedule.
    #[cfg(unix)]
    fn run_unix_reader(&mut self) -> Result<(), String> {
        for _ in 0..10_000 {
            // A bounded hold must end even if no further input arrives.
            if let Some(wait) = self.framer.held_input_release_ms() {
                let due = self.now + wait as u64;
                if !self.arrives_by(due) {
                    self.advance_to(due);
                    let chunks = self.flush()?;
                    typed_into(&mut self.out, chunks);
                    continue;
                }
            }
            if self.next == self.reads.len() {
                return if self.framer.has_pending_input() {
                    Err(self.stranded())
                } else {
                    Ok(())
                };
            }
            if self.arrival().is_none() {
                return Err(self.stranded());
            }
            self.deliver()?;

            let timeout = self.framer.idle_flush_timeout_ms(self.config.mouse_capture);
            let due = self.now + timeout as u64;
            if !self.arrives_by(due) {
                self.advance_to(due);
                let had_pending = self.framer.has_pending_input();
                let chunks = self.flush()?;
                let held_escape = had_pending && chunks.is_empty();
                typed_into(&mut self.out, chunks);
                if held_escape {
                    let due = self.now + self.framer.held_input_flush_timeout_ms() as u64;
                    if !self.arrives_by(due) {
                        self.advance_to(due);
                        let chunks = self.flush()?;
                        typed_into(&mut self.out, chunks);
                    }
                }
            }
        }
        Err("reader loop did not terminate".to_string())
    }

    /// The Windows pump: `push` per read (sometimes followed by an immediate
    /// `flush_timeout`, as a non-key record does), `flush_timeout` on every
    /// idle tick, `flush_interrupted` before a semantic key. Idle ticks that
    /// cannot change anything are skipped: after three quiet ticks the next
    /// one runs at the hold deadline or when the host reply windows end.
    fn run_windows_pump(&mut self) -> Result<(), String> {
        for _ in 0..10_000 {
            let mut quiet = 0;
            loop {
                let tick = if quiet < 3 {
                    Some(self.now + WINDOWS_TICK_MS)
                } else {
                    let base = self.base;
                    let hold = self
                        .framer
                        .tail_hold_deadline()
                        .map(|deadline| deadline.duration_since(base).as_millis() as u64);
                    let reply_windows = [self.last_delivery + 1_010, self.last_delivery + 1_020];
                    hold.into_iter()
                        .chain(reply_windows)
                        .filter(|at| *at > self.now)
                        .min()
                };
                let Some(tick) = tick else { break };
                if self.arrives_by(tick) {
                    break;
                }
                self.advance_to(tick);
                let chunks = self.flush()?;
                typed_into(&mut self.out, chunks);
                quiet += 1;
            }
            if self.next == self.reads.len() {
                return if self.framer.has_pending_input() {
                    Err(self.stranded())
                } else {
                    Ok(())
                };
            }
            if self.arrival().is_none() {
                return Err(self.stranded());
            }
            let read = self.deliver()?;
            if !read.semantic && self.rng.chance(1, 4) {
                let chunks = self.flush()?;
                typed_into(&mut self.out, chunks);
            }
        }
        Err("pump loop did not terminate".to_string())
    }
}

/// I1, I2, I4: `out` is `expected` with every Drop byte removed, every Keep
/// byte present once and in order, and Either bytes present or not.
fn check_output(expected: &[(u8, Class)], out: &[u8]) -> Result<(), String> {
    let mut reach = vec![false; out.len() + 1];
    reach[0] = true;
    for (index, (byte, class)) in expected.iter().enumerate() {
        let mut next = vec![false; out.len() + 1];
        for matched in 0..=out.len() {
            if !reach[matched] {
                continue;
            }
            if *class != Class::Keep {
                next[matched] = true;
            }
            if *class != Class::Drop && out.get(matched) == Some(byte) {
                next[matched + 1] = true;
            }
        }
        reach = next;
        if !reach.contains(&true) {
            return Err(format!(
                "no alignment past expected byte {index} ({:?} {class:?})",
                [*byte].escape_ascii().to_string()
            ));
        }
    }
    if reach[out.len()] {
        Ok(())
    } else {
        Err("the pane received bytes that are not in the input".to_string())
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
        .map(|(byte, class)| {
            let mark = match class {
                Class::Keep => "",
                Class::Drop => "-",
                Class::Either => "?",
            };
            format!("{mark}{}", [*byte].escape_ascii())
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
    .map_err(|err| format!("{err}\n  out: {:?}", harness.out.escape_ascii().to_string()))
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

#[test]
fn held_input_property_oracle_rejects_loss_reorder_and_leaks() {
    use Class::{Drop, Either, Keep};
    let expected = [(b'a', Keep), (b'x', Drop), (b'b', Either), (b'c', Keep)];
    assert!(check_output(&expected, b"abc").is_ok());
    assert!(check_output(&expected, b"ac").is_ok());
    assert!(check_output(&expected, b"a").is_err(), "loss");
    assert!(check_output(&expected, b"ca").is_err(), "reorder");
    assert!(check_output(&expected, b"axc").is_err(), "leak");
    assert!(check_output(&expected, b"aac").is_err(), "duplicate");
}

fn typed(chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    typed_into(&mut out, chunks);
    out
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
