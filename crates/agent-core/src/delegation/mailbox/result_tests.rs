//! A run's result split into its parent's mailbox messages, and the one way a refused steer
//! goes back to wait for the next turn.
use super::*;

/// The text a part carries, without its "[part i/n]" header and the omission note.
fn body(part: &str) -> &str {
    let text = part
        .strip_prefix("[part ")
        .and_then(|rest| rest.split_once("]\n"))
        .map_or(part, |(_, text)| text);
    text.split("\n[… ").next().unwrap_or(text)
}

fn joined(parts: &[String]) -> String {
    parts.iter().map(|p| body(p)).collect()
}

#[test]
fn a_result_that_fits_one_message_is_that_message_unchanged() {
    assert_eq!(result_parts("", 64, 0).parts, vec![String::new()]);
    let one = result_parts("short answer", 64, 0);
    assert_eq!(one.parts, vec!["short answer".to_owned()]);
    assert_eq!(one.omitted, 0);
    // Exactly the cap still fits; one byte more is two parts.
    let exact = "x".repeat(64);
    assert_eq!(result_parts(&exact, 64, 0).parts, vec![exact.clone()]);
    let over = "x".repeat(65);
    let two = result_parts(&over, 64, 0);
    assert_eq!(two.parts.len(), 2);
    assert!(two.parts[0].starts_with("[part 1/2]\n"));
    assert!(two.parts[1].starts_with("[part 2/2]\n"));
    assert_eq!(joined(&two.parts), over);
    assert_eq!(two.omitted, 0);
}

#[test]
fn a_long_result_is_split_in_order_every_part_within_the_cap() {
    let cap = 1024;
    let text: String = (0..3500)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let split = result_parts(&text, cap, 0);
    let n = split.parts.len();
    assert!(n > 1 && n <= MAX_RESULT_PARTS, "{n} parts");
    for (i, part) in split.parts.iter().enumerate() {
        assert!(part.len() <= cap, "part {} is {} bytes", i + 1, part.len());
        assert!(part.starts_with(&format!("[part {}/{n}]\n", i + 1)));
    }
    assert_eq!(joined(&split.parts), text, "read back in full and in order");
    assert_eq!(split.omitted, 0);
    assert!(!split.parts[n - 1].contains("were cut"));
}

#[test]
fn past_eight_parts_the_last_says_how_much_was_cut() {
    let cap = 512;
    let text = "y".repeat(cap * 12);
    let split = result_parts(&text, cap, 0);
    assert_eq!(split.parts.len(), MAX_RESULT_PARTS);
    for part in &split.parts {
        assert!(part.len() <= cap, "{} bytes", part.len());
    }
    let carried = joined(&split.parts);
    assert!(text.starts_with(&carried));
    assert_eq!(split.omitted, text.len() - carried.len());
    let last = &split.parts[MAX_RESULT_PARTS - 1];
    assert!(
        last.ends_with(&omission_note(split.omitted)),
        "the last part names the cut: {last}"
    );
    // What was cut before the split counts too, even for text that would fit.
    let earlier = result_parts("tail", cap, 300);
    assert_eq!(earlier.omitted, 300);
    assert_eq!(earlier.parts.len(), 1);
    assert!(earlier.parts[0].contains("tail"));
    assert!(earlier.parts[0].ends_with(&omission_note(300)));
}

#[test]
fn a_cut_never_splits_a_character() {
    // Three- and four-byte characters against caps that fall inside one, at every alignment,
    // split whole and cut past eight parts.
    for cap in 141..=148 {
        let text = "日本語🦀".repeat(100);
        let split = result_parts(&text, cap, 0);
        for part in &split.parts {
            assert!(part.len() <= cap, "cap {cap}: {} bytes", part.len());
        }
        let carried = joined(&split.parts);
        assert!(text.starts_with(&carried), "cap {cap}");
        assert_eq!(split.omitted, text.len() - carried.len(), "cap {cap}");
        assert!(split.omitted > 0, "cap {cap}: the cut is exercised");
        let whole = "日本語🦀".repeat(10);
        assert_eq!(
            joined(&result_parts(&whole, cap, 0).parts),
            whole,
            "cap {cap}"
        );
    }
}

#[test]
fn only_a_refused_auto_steer_goes_back_to_wait_for_the_next_turn() {
    use DeliveryState as D;
    for from in D::ALL {
        for policy in [Delivery::Auto, Delivery::NextTurn, Delivery::ThisRun] {
            let ok = from == D::Submitted && policy == Delivery::Auto;
            assert_eq!(
                requeue_refused_steer(from, policy).ok(),
                ok.then_some(D::QueuedNextTurn),
                "{from:?} {policy:?}"
            );
        }
        // No ordinary step (an echo, a confirmation, an unknown outcome) takes that way back.
        if from == D::Submitted {
            assert!(delivery_step(from, D::QueuedNextTurn).is_err());
        }
    }
}

#[test]
fn a_cap_too_small_for_a_header_and_note_still_bounds_every_part() {
    for cap in [1, 8, 12, 40] {
        let split = result_parts(&"z".repeat(400), cap, 0);
        assert!(!split.parts.is_empty());
        for part in &split.parts {
            assert!(part.len() <= cap, "cap {cap}: {} bytes", part.len());
        }
    }
}
