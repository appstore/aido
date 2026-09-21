use super::*;

#[test]
fn speech_checks_mime_and_signature_independently() {
    let wav = b"RIFF\x04\0\0\0WAVE".to_vec();
    assert!(speech(wav.clone(), "mp3", "audio/wav").is_err());
    assert!(speech(wav.clone(), "mp3", "audio/mpeg").is_err());
    assert!(speech(wav.clone(), "mp3", "application/octet-stream").is_err());
    assert!(speech(wav, "wav", "Audio/Wav; charset=binary").is_ok());
    assert!(speech(b"garbage".to_vec(), "mp3", "audio/mpeg").is_err());
}

#[test]
fn speech_accepts_supported_headers_and_headerless_pcm() {
    for (format, bytes) in [
        ("mp3", b"ID3\x04\0\0\0\0\0\0".as_slice()),
        ("mp3", b"\xff\xfb\x90\0"),
        ("aac", b"\xff\xf1\x50\x80"),
        ("flac", b"fLaC\0\0\0\0"),
        ("opus", b"OggS\0\0OpusHead"),
        ("pcm", b"\0\0\x01\0"),
    ] {
        assert!(
            speech(bytes.to_vec(), format, "application/octet-stream").is_ok(),
            "{format}"
        );
    }
}
