use super::*;
use crate::domain::InputSource;

fn text_part(id: usize, name: &str, text: &str) -> InputPart {
    InputPart {
        id,
        source: InputSource::File(name.into()),
        name: name.into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(text.into()),
        unit: None,
    }
}

fn para(n: usize) -> String {
    (0..n)
        .map(|i| format!("paragraph {i} with some words"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn paragraphs_split_on_blank_lines_and_keep_interior_breaks() {
    let paras = paragraphs("a\nb\n\n\nc\n\nd");
    assert_eq!(paras, vec!["a\nb", "c", "d"]);
}

#[test]
fn sentences_cut_at_cjk_and_latin_terminals() {
    let s = split_sentences("第一句。第二句！Third one. Fourth? Last; done");
    assert_eq!(
        s,
        vec![
            "第一句。",
            "第二句！",
            "Third one.",
            "Fourth?",
            "Last;",
            "done"
        ]
    );
}

#[test]
fn decimal_points_do_not_cut_sentences() {
    let s = split_sentences("pi is 3.14159 exactly here");
    assert_eq!(s, vec!["pi is 3.14159 exactly here"]);
}

#[test]
fn ellipses_and_punctuation_bursts_stay_glued() {
    let s = split_sentences("wait... what?! really…… ok");
    assert_eq!(s, vec!["wait...", "what?!", "really……", "ok"]);
}

#[test]
fn oversized_paragraph_keeps_ellipses_intact() {
    let filler = "这是一个完整的句子。".repeat(420); // 4200 chars → oversized
    let chunks = split_text(&format!("{filler}wait... what"));
    let joined = chunks.concat();
    // The ellipsis must survive as one piece, not shatter into
    // blank-line-separated fragments around a lone ".".
    assert!(joined.contains("wait..."), "{joined}");
    assert!(!joined.contains(".\n\n."), "{joined}");
}

#[test]
fn hard_pieces_never_split_a_char() {
    let text = "汉".repeat(10);
    let pieces = hard_pieces(&text, 4);
    assert_eq!(pieces.join(""), text);
    assert!(pieces.iter().all(|p| char_len(p) <= 4));
}

#[test]
fn short_text_passes_through_unsplit() {
    let steps = plan_steps(&[text_part(0, "a.txt", "hello")], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].label, "all material");
    assert_eq!(steps[0].inputs.len(), 1);
}

#[test]
fn long_text_produces_ordered_labeled_chunks() {
    let text = para(600); // ~17k chars
    let steps = plan_steps(&[text_part(0, "book.txt", &text)], true).unwrap();
    assert!(
        steps.len() >= 3,
        "expected several chunks, got {}",
        steps.len()
    );
    assert_eq!(
        steps[0].label,
        format!("chunk 1/{} of book.txt", steps.len())
    );
    for (i, step) in steps.iter().enumerate() {
        // chunk part is last; its name carries the chunk index
        let last = step.inputs.last().unwrap();
        assert_eq!(
            last.name,
            format!("book.txt [chunk {}/{}]", i + 1, steps.len())
        );
        if i > 0 {
            assert_eq!(step.inputs.len(), 3, "note + context + chunk");
            assert!(step.inputs[1]
                .text()
                .unwrap()
                .starts_with("Context from the end"));
        } else {
            assert_eq!(step.inputs.len(), 1);
        }
    }
    // The chunks tile the text: joined, they cover every paragraph.
    let joined: String = steps
        .iter()
        .map(|s| s.inputs.last().unwrap().text().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\u{1}");
    for i in 0..600 {
        assert!(
            joined.contains(&format!("paragraph {i} ")),
            "missing paragraph {i}"
        );
    }
    assert!(!steps[0].hard_cut_end);
}

#[test]
fn context_excerpt_comes_from_the_previous_chunk_tail() {
    let text = para(600);
    let steps = plan_steps(&[text_part(0, "book.txt", &text)], true).unwrap();
    let first = steps[0].inputs.last().unwrap().text().unwrap();
    let context = steps[1].inputs[1].text().unwrap();
    let tail = context_tail(first);
    assert!(
        context.ends_with(&tail),
        "{context:?} should end with the excerpt"
    );
}

#[test]
fn unsliced_material_travels_with_every_chunk_in_order() {
    let inputs = vec![
        text_part(0, "small.txt", "hello"),
        text_part(1, "book.txt", &para(600)),
    ];
    let steps = plan_steps(&inputs, true).unwrap();
    assert!(steps.len() >= 3);
    for (i, step) in steps.iter().enumerate() {
        let names: Vec<&str> = step.inputs.iter().map(|p| p.name.as_str()).collect();
        let small = names.iter().position(|n| *n == "small.txt").unwrap();
        let chunk = names
            .iter()
            .position(|n| n.starts_with("book.txt [chunk "))
            .unwrap();
        assert!(small < chunk, "step {i}: {names:?}");
    }
    assert_eq!(steps[0].inputs[0].name, "small.txt");
    // Notes stay ahead of the real material on later steps.
    assert!(steps[1].inputs[0].text().unwrap().contains("chunk"));
}

#[test]
fn material_order_follows_the_command_line() {
    // The long document listed first: its chunk keeps the front spot in
    // every request, the glossary rides behind it.
    let inputs = vec![
        text_part(0, "book.txt", &para(600)),
        text_part(1, "gloss.txt", "terms"),
    ];
    let steps = plan_steps(&inputs, true).unwrap();
    for (i, step) in steps.iter().enumerate() {
        let names: Vec<&str> = step.inputs.iter().map(|p| p.name.as_str()).collect();
        let gloss = names.iter().position(|n| *n == "gloss.txt").unwrap();
        let chunk = names
            .iter()
            .position(|n| n.starts_with("book.txt [chunk "))
            .unwrap();
        assert!(chunk < gloss, "step {i}: {names:?}");
    }
}

#[test]
fn oversized_unsliced_material_rides_with_the_first_chunk_only() {
    // Over the carry budget, under the chunk target: too big to repeat
    // in every request, too small to be chunked itself.
    let glossary = "词".repeat(super::super::MAX_CARRY_CHARS + 1);
    assert!(char_len(&glossary) < TARGET_CHUNK_CHARS);
    let inputs = vec![
        text_part(0, "gloss.txt", &glossary),
        text_part(1, "book.txt", &para(600)),
    ];
    let steps = plan_steps(&inputs, true).unwrap();
    assert!(steps.len() >= 3);
    assert_eq!(steps[0].inputs[0].name, "gloss.txt");
    for (i, step) in steps.iter().enumerate().skip(1) {
        assert!(
            !step.inputs.iter().any(|p| p.name == "gloss.txt"),
            "step {i} must not repeat the oversized material"
        );
    }
}

#[test]
fn a_second_chunked_part_travels_in_its_own_steps_only() {
    let inputs = vec![
        text_part(0, "a.txt", &para(600)),
        text_part(1, "shared.txt", "hello"),
        text_part(2, "b.txt", &para(600)),
    ];
    let steps = plan_steps(&inputs, true).unwrap();
    let mut seen_b = false;
    for step in &steps {
        let names: Vec<&str> = step.inputs.iter().map(|p| p.name.as_str()).collect();
        let has_a = names.iter().any(|n| n.starts_with("a.txt [chunk "));
        let has_b = names.iter().any(|n| n.starts_with("b.txt [chunk "));
        assert!(
            has_a ^ has_b,
            "exactly one chunked part's chunk per request: {names:?}"
        );
        assert!(names.contains(&"shared.txt"), "{names:?}");
        if has_b {
            seen_b = true;
            // Command-line order: b's chunk stands at b's position,
            // behind the shared material listed before it.
            let shared = names.iter().position(|n| *n == "shared.txt").unwrap();
            let chunk = names
                .iter()
                .position(|n| n.starts_with("b.txt [chunk "))
                .unwrap();
            assert!(shared < chunk, "{names:?}");
        } else {
            assert!(!seen_b, "a's steps must all precede b's: {names:?}");
        }
    }
    assert!(seen_b);
}

#[test]
fn tiny_tail_folds_into_the_previous_chunk() {
    // One paragraph just over target + one tiny paragraph: without the
    // fold the tail would travel as a request for two words.
    let big = format!("{}\n\n\ntiny end", "x".repeat(TARGET_CHUNK_CHARS + 10));
    let chunks = split_text(&big);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].ends_with("tiny end"));
}

#[test]
fn oversized_paragraph_splits_at_sentences_not_mid_word() {
    let sentence = "这是一个完整的句子。";
    let para_text = sentence.repeat(600); // 6000 chars, one paragraph
    let chunks = split_text(&para_text);
    assert!(chunks.len() >= 2);
    for chunk in &chunks {
        assert!(char_len(chunk) <= TARGET_CHUNK_CHARS);
        // every cut lands after 。 — chunks end at sentence boundaries
        assert!(chunk.ends_with('。'));
    }
    // Only paragraph separators were added; the text itself is intact.
    assert_eq!(chunks.concat().replace("\n\n", ""), para_text);
}

#[test]
fn oversized_sentence_hard_cuts_on_char_boundaries() {
    let sentence = "汉".repeat(2 * TARGET_CHUNK_CHARS + 200); // no punctuation at all
    let chunks = split_text(&sentence);
    assert!(chunks.len() >= 2);
    // The small tail folded into its predecessor with a paragraph
    // separator; the characters themselves are all still there.
    assert_eq!(chunks.concat().replace("\n\n", ""), sentence);
    // ...so one chunk may sit a little over target, bounded by MIN_TAIL.
    assert!(chunks
        .iter()
        .all(|c| char_len(c) <= TARGET_CHUNK_CHARS + MIN_TAIL_CHARS));
}

#[test]
fn whitespace_only_long_text_passes_through_unsplit() {
    // No paragraphs at all: chunking would stage a run with zero
    // requests, so the text must fall back to the single-request path.
    let text = " \n ".repeat(2000);
    let steps = plan_steps(&[text_part(0, "pad.txt", &text)], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].label, "all material");
    assert_eq!(steps[0].inputs.len(), 1);
}

#[test]
fn text_that_folds_to_one_chunk_travels_whole() {
    // Just over target with no cut points: the tail fold leaves a
    // single chunk, which is the single-request path with extra steps.
    let text = "x".repeat(TARGET_CHUNK_CHARS + 2);
    let steps = plan_steps(&[text_part(0, "one.txt", &text)], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].label, "all material");
}

#[test]
fn reduce_plan_appends_one_reduce_step_after_the_maps() {
    let input = [text_part(0, "book.txt", &para(600))];
    let steps = plan_steps_reduce(&input, true).unwrap();
    let maps = plan_steps(&input, true).unwrap();
    assert_eq!(steps.len(), maps.len() + 1);
    let (reduce, map_steps) = steps.split_last().unwrap();
    assert_eq!(reduce.role, StepRole::Reduce);
    // Placeholders only: the map replies become the material at run
    // time, so the plan cannot carry real inputs here.
    assert!(reduce.inputs.is_empty());
    assert_eq!(reduce.index, map_steps.len());
    assert_eq!(
        reduce.label,
        format!("consolidate {} chunks", map_steps.len())
    );
    assert!(map_steps.iter().all(|s| s.role == StepRole::Map));
}

#[test]
fn reduce_plan_skips_the_reduce_step_for_a_single_chunk() {
    // One chunk is the whole document; consolidating it with itself
    // would be an extra request for nothing.
    let steps = plan_steps_reduce(&[text_part(0, "a.txt", "hello")], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].role, StepRole::Map);
    assert_eq!(steps[0].label, "all material");
}

#[test]
fn reduce_material_labels_every_chunk_result() {
    let parts = reduce_inputs(&[(0, " 一\n\n".to_string()), (1, "二".to_string())]);
    // Consolidation note + one section per map reply.
    assert_eq!(parts.len(), 3);
    assert!(parts[0].text().unwrap().contains("one longer document"));
    let one = parts[1].text().unwrap();
    assert!(one.starts_with("--- result 1 of 2 ---"), "{one}");
    assert!(one.ends_with("一"), "reply whitespace trimmed: {one}");
    assert!(parts[2].text().unwrap().contains("--- result 2 of 2 ---"));
    assert!(parts[2].text().unwrap().contains("二"));
}

#[test]
fn reduce_material_depends_on_position_not_step_index() {
    // The step index rides with each reply for provenance only: the
    // material a request sees is byte-identical however the replies
    // map onto requests.
    let sparse = reduce_inputs(&[(2, "一".to_string()), (5, "二".to_string())]);
    let dense = reduce_inputs(&[(0, "一".to_string()), (1, "二".to_string())]);
    assert_eq!(sparse.len(), dense.len());
    for (a, b) in sparse.iter().zip(dense.iter()) {
        assert_eq!(a.text().unwrap(), b.text().unwrap());
    }
}

// --- ChunkGate ---------------------------------------------------------

fn gate_log() -> (ChunkGate, std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let gate = ChunkGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    (gate, log)
}

fn joined(log: &std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> String {
    log.borrow().concat()
}

#[test]
fn slices_join_with_one_paragraph_break() {
    let (mut gate, log) = gate_log();
    gate.push_delta("first chunk end");
    gate.slice_end();
    gate.push_delta("second chunk start");
    gate.finish();
    assert_eq!(joined(&log), "first chunk end\n\nsecond chunk start");
}

#[test]
fn leading_newlines_of_the_next_slice_collapse() {
    let (mut gate, log) = gate_log();
    gate.push_delta("first\n");
    gate.slice_end();
    gate.push_delta("\n\n\nsecond\n");
    gate.finish();
    assert_eq!(joined(&log), "first\n\nsecond\n");
}

#[test]
fn empty_slice_does_not_double_the_separator() {
    let (mut gate, log) = gate_log();
    gate.push_delta("first");
    gate.slice_end();
    gate.slice_end(); // empty reply in between
    gate.push_delta("third");
    gate.finish();
    assert_eq!(joined(&log), "first\n\nthird");
}

#[test]
fn single_slice_passes_through_untouched() {
    let (mut gate, log) = gate_log();
    gate.push_delta("\nleading newlines are the model's own\n");
    gate.slice_end();
    gate.finish();
    assert_eq!(joined(&log), "\nleading newlines are the model's own\n");
}

#[test]
fn head_buffering_spans_deltas() {
    let (mut gate, log) = gate_log();
    gate.push_delta("first");
    gate.slice_end();
    // The next slice's head arrives in pieces: newlines first, then
    // content split mid-line.
    gate.push_delta("\n");
    gate.push_delta("\nse");
    gate.push_delta("cond\n");
    gate.finish();
    assert_eq!(joined(&log), "first\n\nsecond\n");
}

#[test]
fn pure_newline_deltas_cannot_overflow_the_tracker() {
    let (mut gate, log) = gate_log();
    gate.push_delta("a\n\n");
    // 2 held newlines + 255 incoming ones must not overflow the u8
    // tracker (it caps at 2); mid-slice newlines still pass through
    // untouched, and the boundary adds no separator on top.
    gate.push_delta(&"\n".repeat(255));
    gate.slice_end();
    gate.push_delta("b");
    gate.finish();
    // 2 from "a\n\n" + 255 passed through; the boundary adds nothing.
    assert_eq!(joined(&log), format!("a{}b", "\n".repeat(257)));
}

#[test]
fn deltas_after_finish_are_ignored() {
    let (mut gate, log) = gate_log();
    gate.push_delta("done");
    gate.finish();
    gate.push_delta(" stray");
    gate.slice_end();
    assert_eq!(joined(&log), "done");
}

#[test]
fn finish_is_idempotent() {
    let (mut gate, log) = gate_log();
    gate.push_delta("only");
    gate.slice_end();
    gate.finish();
    gate.finish();
    assert_eq!(joined(&log), "only");
}
