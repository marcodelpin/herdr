//! Regression tests for the five findings of codex review f5251-r2 on 166f9f8b.
//! They use only framer API that 166f9f8b already had (`age_held_input` moves
//! every time-bounded hold into the past), so each one can be run against
//! that revision to show it fails there.

use super::*;
use std::time::Duration;

/// Longer than every hold bound.
const PAST_EVERY_BOUND: Duration = Duration::from_secs(3);

fn typed(chunks: &[Vec<u8>]) -> Vec<u8> {
    chunks
        .iter()
        .filter(|chunk| {
            events_from_framed_chunks(vec![chunk.to_vec()])
                .iter()
                .any(|event| {
                    matches!(
                        event,
                        RawInputEvent::Key(_) | RawInputEvent::Text(_) | RawInputEvent::Paste(_)
                    )
                })
        })
        .flat_map(|chunk| chunk.iter().copied())
        .collect()
}

/// What the Unix reader does while nothing more arrives: flush whenever the
/// framer reports a release time, until it reports none.
fn drain_like_reader(framer: &mut RawInputByteFramer) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    for _ in 0..16 {
        if framer.held_input_release_ms().is_none() {
            break;
        }
        framer.age_held_input(PAST_EVERY_BOUND);
        chunks.extend(framer.flush_timeout());
    }
    chunks
}

/// r2-1: `ESC [` | flush | `<` | flush | `x`, and the same without `x`.
#[test]
fn review_r2_1_less_than_after_alt_bracket_is_never_lost() {
    let mut framer = RawInputByteFramer::default();
    let mut chunks = framer.push(b"\x1b[");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"<"));
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"x"));
    assert_eq!(typed(&chunks), b"\x1b[<x".to_vec(), "{chunks:?}");

    let mut framer = RawInputByteFramer::default();
    let mut chunks = framer.push(b"\x1b[");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"<"));
    chunks.extend(framer.flush_timeout());
    chunks.extend(drain_like_reader(&mut framer));
    assert_eq!(typed(&chunks), b"\x1b[<".to_vec(), "{chunks:?}");
    assert!(!framer.has_pending_input());
}

/// r2-2: a colour header after a released ESC must not swallow a paste or
/// typing, and must be released by a deadline the reader can see.
#[test]
fn review_r2_2_color_header_after_released_escape_keeps_paste_and_typing() {
    let paste = b"\x1b[200~abc\x1b[201~";
    for header in [b"]10;".as_slice(), b"]11;"] {
        for next in [paste.as_slice(), b"x"] {
            let mut framer = RawInputByteFramer::default();
            framer.host_color_query_sent();
            let mut chunks = framer.push(b"\x1b");
            chunks.extend(framer.flush_timeout());
            chunks.extend(framer.flush_timeout());
            chunks.extend(framer.push(header));
            assert!(framer.has_pending_input());
            assert!(
                framer.held_input_release_ms().is_some(),
                "{header:?}: a held header needs a deadline"
            );
            chunks.extend(framer.push(next));
            chunks.extend(framer.flush_timeout());
            chunks.extend(drain_like_reader(&mut framer));

            let typed = typed(&chunks);
            assert!(
                typed.ends_with(next),
                "{header:?} then {next:?} must reach the pane: {chunks:?}"
            );
            assert!(!framer.has_pending_input());
        }
    }
}

/// r2-3 (framer level): text held after a dropped OSC header is released
/// before a semantic key.
#[test]
fn review_r2_3_held_osc_candidate_text_is_released_on_interruption() {
    let mut framer = RawInputFramer::default();
    assert!(framer.push(b"\x1b]4;50;").is_empty());
    assert!(framer.idle_tick().is_empty());
    assert!(framer.push(b"a").is_empty());

    let events = framer.flush_interrupted();
    assert!(
        matches!(&events[..], [RawInputEvent::Key(key)]
            if key.code == crossterm::event::KeyCode::Char('a')),
        "{events:?}"
    );
    assert!(!framer.byte_framer.has_pending_input());
}

/// r2-4: an Escape left over when a held OSC candidate is released must get
/// its own deadline, not wait for another input byte.
#[cfg(unix)]
#[test]
fn review_r2_4_escape_after_released_osc_candidate_is_not_stranded() {
    let mut framer = RawInputByteFramer::default();
    framer.set_host_escape_disambiguation_active(true);
    let mut chunks = framer.push(b"\x1b]4;50;");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"a\x1b"));
    framer.age_held_input(PAST_EVERY_BOUND);
    chunks.extend(drain_like_reader(&mut framer));

    assert_eq!(typed(&chunks), b"a\x1b".to_vec(), "{chunks:?}");
    assert!(!framer.has_pending_input());
}

/// r2-5: a complete continuation that arrives after the hold expired, with no
/// flush in between, is classified after the expiry, so it is input.
#[test]
fn review_r2_5_expiry_precedes_completion_of_mouse_continuation() {
    let mut framer = RawInputByteFramer::default();
    let mut chunks = framer.push(b"\x1b");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"["));
    chunks.extend(framer.flush_timeout());
    framer.age_held_input(PAST_EVERY_BOUND);
    chunks.extend(framer.push(b"<35;100;30Mx"));

    assert_eq!(
        typed(&chunks),
        b"\x1b[<35;100;30Mx".to_vec(),
        "past its bound the bracket and everything after it are input: {chunks:?}"
    );
}
