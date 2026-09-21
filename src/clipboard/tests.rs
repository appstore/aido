use super::*;

fn tiny_jpeg() -> Vec<u8> {
    let img = image::GrayImage::from_pixel(2, 2, image::Luma([128]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    jpg
}

/// A real 2×2 JPEG whose SOF0 segment is patched to declare `w`×`h`:
/// the decompression-bomb shape — a few hundred bytes claiming a huge
/// canvas.
fn jpeg_declaring(w: u32, h: u32) -> Vec<u8> {
    let mut jpg = tiny_jpeg();
    // Layout after the FF C0 marker: length(2), precision(1), height
    // (2 BE), width (2 BE).
    let sof = jpg
        .windows(2)
        .position(|p| p == [0xFF, 0xC0])
        .expect("encoder wrote a SOF0 marker");
    jpg[sof + 5..sof + 7].copy_from_slice(&(h as u16).to_be_bytes());
    jpg[sof + 7..sof + 9].copy_from_slice(&(w as u16).to_be_bytes());
    jpg
}

#[test]
fn clipboard_decode_refuses_bomb_shaped_bytes() {
    let err = decode_for_clipboard(&jpeg_declaring(20_000, 20_000)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to decode"), "{msg}");
    assert!(msg.contains("400 MP"), "{msg}");
}

#[test]
fn clipboard_decode_returns_rgba_for_small_images() {
    let rgba = decode_for_clipboard(&tiny_jpeg()).unwrap();
    assert_eq!(rgba.dimensions(), (2, 2));
}

/// A path that can never be spawned — the same class of failure
/// `spawn_holder` hits in production when the aido binary was deleted
/// or swapped mid-run, or the environment forbids forking.
#[cfg(target_os = "linux")]
fn unspawnable_exe() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent/aido-holder")
}

#[cfg(target_os = "linux")]
#[test]
fn holder_spawn_failure_is_a_warning_not_an_error() {
    // The failure path must come back as an error the caller turns into
    // the stderr warning — never a panic, never a propagation that
    // would fail the (already successful) clipboard delivery.
    let err = hold_via(&unspawnable_exe(), b"payload", 5, false).unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("cannot start the holder child"), "{chain}");
    let warning = hold_warning(&err);
    assert!(warning.starts_with("warning: "), "{warning}");
    assert!(
        warning.contains("clipboard contents may not outlive this process"),
        "{warning}"
    );
    assert!(warning.contains("hold failed"), "{warning}");
}

#[cfg(target_os = "linux")]
#[test]
fn image_holder_takes_the_same_warning_path() {
    let err = hold_via(&unspawnable_exe(), b"png-bytes", 5, true).unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot start the holder child"),
        "{err:#}"
    );
    let warning = hold_warning(&err);
    assert!(
        warning.contains("clipboard contents may not outlive this process"),
        "{warning}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn zero_hold_secs_never_spawns_the_holder() {
    // hold_secs == 0 short-circuits before any spawn machinery runs,
    // so there is no failure mode to report and no child to leave.
    spawn_holder(b"payload", 0, false);
    spawn_holder(b"payload", 0, true);
}
