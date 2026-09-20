use super::*;
#[test]
fn large_event_in_small_chunks_and_many_events_in_one_chunk() {
    let payload = "a".repeat(8 * 1024 * 1024);
    let wire = format!("data: {payload}\n\ndata: tail\n\n");
    let mut decoder = SseDecoder::new();
    let mut events = Vec::new();
    for chunk in wire.as_bytes().chunks(1024) {
        events.extend(decoder.feed(chunk));
    }
    assert_eq!(events, vec![payload, "tail".to_string()]);
    assert_eq!(
        decoder.feed("data: x\n\n".repeat(10000).as_bytes()),
        vec!["x"; 10000]
    );
    assert!(decoder.finish().is_empty());
}
#[test]
fn reassembles_utf8_at_every_byte_boundary() {
    let data = "data: {\"text\":\"你好\"}\r\n\r\n";
    for split in 0..data.len() {
        let mut decoder = SseDecoder::new();
        let mut frames = decoder.feed(&data.as_bytes()[..split]);
        frames.extend(decoder.feed(&data.as_bytes()[split..]));
        frames.extend(decoder.finish());
        assert_eq!(frames, vec!["{\"text\":\"你好\"}"]);
    }
}
#[test]
fn framing_leaves_protocol_sentinels_to_adapters() {
    let mut decoder = SseDecoder::new();
    assert_eq!(
        decoder.feed(b": ping\nevent: ignored\ndata: first\ndata: second\n\ndata: [DONE]\n\n"),
        vec!["first\nsecond", "[DONE]"]
    );
    assert!(decoder.feed(b"data: last").is_empty());
    assert_eq!(decoder.finish(), vec!["last"]);
}
