//! Regression tests for the four findings of codex review f5251-r1 on 3f74f771.
//! They use only framer API that 3f74f771 already had, so each one can be run
//! against that revision to show it fails there.

use super::*;

/// Bytes of the chunks that reach the pane as input (keys, text, pastes).
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

/// r1-1: `ESC` | flush | `[` | interruption. The held bracket is input.
#[test]
fn review_r1_1_bracket_after_timed_out_escape_survives_interruption() {
    let mut framer = RawInputFramer::default();
    assert!(framer.push(b"\x1b").is_empty());
    assert_eq!(framer.flush_timeout().len(), 1);
    assert!(framer.push(b"[").is_empty());

    let events = framer.flush_interrupted();
    assert!(
        matches!(&events[..], [RawInputEvent::Key(key)]
            if key.code == crossterm::event::KeyCode::Char('[')),
        "the held bracket must be released before the semantic key: {events:?}"
    );
}

/// r1-2: typing and a paste after a dropped incomplete OSC header.
#[test]
fn review_r1_2_typing_and_paste_after_dropped_osc_header_reach_the_pane() {
    let paste = b"\x1b[200~hi\x1b[201~";
    let mut framer = RawInputByteFramer::default();
    let mut chunks = framer.push(b"\x1b]");
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"a"));
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(b"b"));
    chunks.extend(framer.flush_timeout());
    chunks.extend(framer.push(paste));
    chunks.extend(framer.flush_timeout());

    let mut expected = b"ab".to_vec();
    expected.extend_from_slice(paste);
    assert_eq!(typed(&chunks), expected, "{chunks:?}");
}

/// r1-3: `ESC [` | timeout | `<35;100;30Mx`, no host reply outstanding.
#[test]
fn review_r1_3_sgr_report_split_after_csi_introducer_is_not_typed() {
    let mut framer = RawInputByteFramer::default();
    assert!(framer.push(b"\x1b[").is_empty());
    assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);

    assert_eq!(framer.push(b"<35;100;30Mx"), vec![b"x".to_vec()]);
}

/// r1-4: an OSC 10/11 reply split after its ESC while a colour query is
/// outstanding, terminated by BEL or ST.
#[test]
fn review_r1_4_color_reply_split_after_its_escape_is_not_typed() {
    for (first, second) in [
        (b"]10;rgb:afaf/".as_slice(), b"d7d7/8787\x07x".as_slice()),
        (b"]11;rgb:afaf/", b"d7d7/8787\x07x"),
        (b"]10;rgb:afaf/", b"d7d7/8787\x1b\\x"),
        (b"]11;rgb:afaf/", b"d7d7/8787\x1b\\x"),
    ] {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut chunks = framer.push(b"\x1b");
        chunks.extend(framer.flush_timeout());
        chunks.extend(framer.flush_timeout());
        chunks.extend(framer.push(first));
        chunks.extend(framer.push(second));
        chunks.extend(framer.flush_timeout());

        assert_eq!(
            typed(&chunks),
            b"\x1bx".to_vec(),
            "{first:?} {second:?}: only Escape and x are input: {chunks:?}"
        );
    }
}
