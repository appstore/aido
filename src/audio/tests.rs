use super::decode_mono;

// Committed fixtures are generated with ffmpeg and checked in for
// determinism (libopus output moves with the encoder version):
//
//   tone.mp3:  ffmpeg -f lavfi -i sine=frequency=440:duration=0.5:sample_rate=44100 \
//                -af volume=18dB -ac 1 -b:a 64k tone.mp3
//   tone.webm: ffmpeg -f lavfi -i sine=frequency=440:duration=0.5:sample_rate=48000 \
//                -ac 1 -c:a libopus -b:a 24k tone.webm
//   av.mkv:    ffmpeg -f lavfi -i sine=frequency=440:duration=0.5:sample_rate=44100 \
//                -f lavfi -i testsrc=size=64x48:rate=10:duration=0.5 \
//                -af volume=18dB -ac 1 -c:a flac -c:v mpeg4 -shortest av.mkv
//
// The sine filter defaults to roughly -18 dB; the volume filter brings the
// fixtures to normal loudness so peak assertions have something to bite on.
// These tests exercise the wrapper contract and the asr-core integration;
// the decode logic itself is covered by asr-core's own suite.

/// One hour at 48 kHz, stereo: a generous stand-in for the caller's budget.
const BUDGET: usize = 48_000 * 3_600 * 2;

/// A minimal PCM16 WAV writer: RIFF header + fmt + data. Test WAVs are
/// built here rather than committed so channel layout, rate and samples
/// stay visible next to the assertions.
fn wav_bytes(interleaved: &[i16], channels: usize, sample_rate: u32) -> Vec<u8> {
    let data_len = (interleaved.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + interleaved.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&(channels as u16).to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in interleaved {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn f32_bytes(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[test]
fn wav_mono_survives_as_itself() {
    let rates: Vec<f32> = (0..100).map(|i| i as f32 * 300.0).collect();
    let interleaved: Vec<i16> = rates.iter().map(|s| f32_bytes(s / 32768.0)).collect();
    let buffer = decode_mono(&wav_bytes(&interleaved, 1, 8000), BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 8000);
    assert_eq!(buffer.samples.len(), 100);
    // 2 units: i16 truncation plus symphonia's 32768-vs-32767 scaling.
    for (got, want) in buffer.samples.iter().zip(rates.iter()) {
        assert!((got * 32768.0 - want).abs() < 2.0);
    }
}

#[test]
fn wav_stereo_downmixes_to_mono() {
    // Left +0.5, right -0.5: the mean of every frame is silence.
    let interleaved: Vec<i16> = (0..10)
        .flat_map(|_| [f32_bytes(0.5), f32_bytes(-0.5)])
        .collect();
    let buffer = decode_mono(&wav_bytes(&interleaved, 2, 44100), BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 44100);
    assert_eq!(buffer.samples.len(), 10);
    assert!(buffer.samples.iter().all(|s| s.abs() < 1e-4));
}

#[test]
fn wav_with_no_data_is_an_error() {
    let err = decode_mono(&wav_bytes(&[], 1, 8000), BUDGET).unwrap_err();
    assert!(format!("{err:#}").contains("no audio could be decoded"));
}

#[test]
fn mp3_fixture_decodes_to_mono() {
    // ffmpeg sine 440 Hz, 0.5 s mono at 44100 Hz. The frame count moves a
    // little with codec padding, hence the window.
    let buffer = decode_mono(include_bytes!("testdata/tone.mp3"), BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 44100);
    let seconds = buffer.samples.len() as f64 / f64::from(buffer.spec.sample_rate);
    assert!(
        (0.35..=0.65).contains(&seconds),
        "unexpected duration: {seconds:.3}s"
    );
    let peak = buffer.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        (0.5..=1.1).contains(&peak),
        "sine peak out of range: {peak}"
    );
}

#[test]
fn corrupt_mp3_packets_are_skipped() {
    // Sixty-four inverted bytes a third of the way in destroy at most a
    // couple of packets — well under the upstream consecutive-failure
    // limit; the rest of the file must still decode.
    let mut bytes = include_bytes!("testdata/tone.mp3").to_vec();
    let at = bytes.len() / 3;
    for b in &mut bytes[at..at + 64] {
        *b ^= 0xFF;
    }
    let buffer = decode_mono(&bytes, BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 44100);
    assert!(!buffer.samples.is_empty());
}

#[test]
fn truncated_mp3_keeps_the_prefix() {
    let bytes = include_bytes!("testdata/tone.mp3");
    let cut = bytes.len() * 2 / 3;
    let buffer = decode_mono(&bytes[..cut], BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 44100);
    assert!(!buffer.samples.is_empty());
}

#[test]
fn mkv_with_a_video_track_decodes_the_audio() {
    // flac audio + mpeg4 video: the video packets must be filtered out by
    // track id and the default-track selection must land on the audio.
    let buffer = decode_mono(include_bytes!("testdata/av.mkv"), BUDGET).unwrap();
    assert_eq!(buffer.spec.sample_rate, 44100);
    let seconds = buffer.samples.len() as f64 / f64::from(buffer.spec.sample_rate);
    assert!(
        (0.35..=0.65).contains(&seconds),
        "unexpected duration: {seconds:.3}s"
    );
}

#[test]
fn sample_budget_refuses_the_result() {
    // The budget is checked after the decode: it is a detection of an
    // oversized result, not a guard around the allocation.
    let err = decode_mono(include_bytes!("testdata/tone.mp3"), 100).unwrap_err();
    assert!(format!("{err:#}").contains("sample budget"));
}

#[test]
fn matroska_doctype_is_not_webm() {
    // Regression: MKV and WebM share the EBML magic, so classification
    // must ride on the DocType element. The committed fixture is a
    // matroska file; the synthesized header carries "webm" as its DocType.
    assert!(!super::is_webm(include_bytes!("testdata/av.mkv")));
    let mut webm_header = vec![0x1A, 0x45, 0xDF, 0xA3];
    webm_header.extend_from_slice(&[0x42, 0x82, 0x84]);
    webm_header.extend_from_slice(b"webm");
    assert!(super::is_webm(&webm_header));
}

#[test]
fn webm_opus_routes_to_the_cloud() {
    let err = decode_mono(include_bytes!("testdata/tone.webm"), BUDGET).unwrap_err();
    assert!(format!("{err:#}").contains("cloud"));
}

#[test]
fn garbage_reports_unrecognized_format() {
    let err = decode_mono(b"definitely not audio", BUDGET).unwrap_err();
    assert!(format!("{err:#}").contains("offline decoding failed"));
}

#[test]
fn empty_input_is_an_error() {
    assert!(decode_mono(&[], BUDGET).is_err());
}
