use super::*;
use crate::clipboard::ClipboardContent;
use crate::test_support::run_root;
use std::io::Cursor;

fn env(stdin_data: &'static [u8]) -> InputEnv<'static> {
    // The cursor and closure are leaked here on purpose: test-scoped,
    // tiny, and it keeps every call site a one-liner. The data probe
    // mirrors what a real pipe would report: bytes pending or not.
    let has_data = !stdin_data.is_empty();
    InputEnv {
        stdin: Box::leak(Box::new(Cursor::new(stdin_data))),
        stdin_data_probe: Box::leak(Box::new(move || has_data)),
        clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
    }
}

fn file(p: &str) -> SourceSpec {
    SourceSpec::File(p.into())
}

#[test]
fn loose_mpeg_sync_is_not_audio() {
    // Missing sync, reserved layer, and reserved version.
    for second in [0x0a, 0xe0, 0xea, 0xf8] {
        let bytes = [0xff, second, 0x00, 0x01];
        assert_eq!(audio_type(&bytes), None, "header: {bytes:02x?}");
        let err = classify("blob.bin", bytes.to_vec(), InputSource::Stdin, 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "'blob.bin' is neither valid UTF-8 text nor a supported image/audio file"
        );
    }
}

#[test]
fn mpeg_headers_and_id3_still_classify_as_audio() {
    for bytes in [
        b"ID3\x04\x00".as_slice(),
        &[0xff, 0xfb, 0x90, 0x00],
        &[0xff, 0xff, 0x40, 0x00],
    ] {
        assert_eq!(audio_type(bytes), Some(("audio/mpeg", "mp3")));
        let part = classify("song.mp3", bytes.to_vec(), InputSource::Stdin, 0).unwrap();
        assert_eq!(part.kind, MediaKind::Audio);
        assert_eq!(part.mime, "audio/mpeg");
        assert_eq!(part.content, InputContent::Media(bytes.to_vec()));
    }
}

#[test]
fn piped_stdin_alone_is_material() {
    let mut e = env(b"hello\n");
    let parts = gather(&[], true, None, false, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].text(), Some("hello\n"));
    assert_eq!(parts[0].source, InputSource::Stdin);
}

#[test]
fn empty_closed_stdin_falls_back_to_the_clipboard() {
    // No pending bytes (closed pipe, /dev/null) is the decision
    // table's no-data row: the clipboard takes over instead of the
    // old "stdin is empty" rejection.
    let mut e = env(b"");
    let parts = gather(&[], true, None, false, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].source, InputSource::Clipboard);
    assert_eq!(parts[0].text(), Some("clip"));
}

#[test]
fn instruction_only_run_proceeds_with_a_closed_empty_stdin() {
    // `aido ask -p "hi" < /dev/null`: no data, no clipboard need —
    // the instruction alone drives the run.
    let mut e = env(b"");
    let parts = gather(&[], false, None, false, &mut e).unwrap();
    assert!(parts.is_empty());
}

#[test]
fn probe_true_but_eof_read_is_still_an_error() {
    // The probe promised bytes, the read hit EOF: a writer closed the
    // pipe between the snapshot and the read. The strict error stays —
    // this really was meant to be stdin material.
    let mut e = InputEnv {
        stdin: Box::leak(Box::new(Cursor::new(b""))),
        stdin_data_probe: Box::leak(Box::new(|| true)),
        clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
    };
    let err = gather(&[], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("stdin is empty"), "{err}");
}

#[test]
fn unconsumed_pipe_with_explicit_material_is_an_error() {
    let mut e = env(b"pipe data\n");
    let err = gather(&[file("a.txt")], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("add `-`"));
}

#[test]
fn closed_empty_stdin_with_explicit_material_is_not_an_error() {
    // CI runners, cron and `docker run` without `-t` attach a closed
    // pipe or /dev/null: not a terminal, and no bytes either. The
    // explicit material must run exactly as it would on a terminal.
    let dir = run_root().join(format!("aido-input-null-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.txt");
    std::fs::write(&real, b"real\n").unwrap();
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// A data probe that says yes regardless of the cursor: stdin state
/// and probe answer are independent injections.
#[test]
fn probe_drives_the_pipe_guard_not_the_cursor() {
    let dir = run_root().join(format!("aido-input-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.txt");
    std::fs::write(&real, b"real\n").unwrap();
    let mut e = InputEnv {
        stdin: Box::leak(Box::new(Cursor::new(b""))),
        stdin_data_probe: Box::leak(Box::new(|| true)),
        clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
    };
    let err = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("add `-`"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dash_reads_stdin_at_its_position() {
    let dir = run_root().join(format!("aido-input-dash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.txt");
    std::fs::write(&a, b"from file\n").unwrap();
    let mut e = env(b"pipe\n");
    let parts = gather(
        &[SourceSpec::File(a.clone()), SourceSpec::Stdin],
        true,
        None,
        false,
        &mut e,
    )
    .unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].text(), Some("from file\n"));
    assert_eq!(parts[1].text(), Some("pipe\n"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn terminal_stdin_falls_back_to_clipboard_only_without_specs() {
    let mut e = env(b"");
    let parts = gather(&[], true, None, false, &mut e).unwrap();
    assert_eq!(parts[0].text(), Some("clip"));
    assert_eq!(parts[0].source, InputSource::Clipboard);
}

#[test]
fn no_material_task_runs_on_instruction_alone() {
    let mut e = env(b"");
    let parts = gather(&[], false, None, false, &mut e).unwrap();
    assert!(parts.is_empty());
}

#[test]
fn empty_file_is_an_error_not_a_skip() {
    let dir = run_root().join(format!("aido-input-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty.txt");
    std::fs::write(&empty, b"").unwrap();
    let mut e = env(b"");
    let err = gather(
        &[SourceSpec::File(empty.clone())],
        true,
        None,
        false,
        &mut e,
    )
    .unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
    let real = dir.join("real.txt");
    std::fs::write(&real, b"real\n").unwrap();
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn original_image_bytes_are_kept() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([1, 2, 3, 255]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let dir = run_root().join(format!("aido-input-jpg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shot.jpg");
    std::fs::write(&path, &jpg).unwrap();
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::File(path)], true, None, false, &mut e).unwrap();
    assert_eq!(parts[0].kind, MediaKind::Image);
    assert_eq!(parts[0].mime, "image/jpeg");
    assert_eq!(parts[0].content, InputContent::Media(jpg));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_stdin_and_paste_rejected() {
    let mut e = env(b"x");
    let err = gather(
        &[SourceSpec::Stdin, SourceSpec::Stdin],
        true,
        None,
        false,
        &mut e,
    );
    assert!(err.is_err());
    let mut e = env(b"");
    let err = gather(
        &[SourceSpec::Paste, SourceSpec::Paste],
        true,
        None,
        false,
        &mut e,
    );
    assert!(err.is_err());
}

#[test]
fn paste_is_material_and_consumes_the_pipe_check() {
    // paste + piped stdin without `-` is still "unconsumed pipe"
    let mut e = env(b"pipe\n");
    let err = gather(&[SourceSpec::Paste], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("add `-`"));
}

#[test]
fn dry_run_never_touches_the_clipboard() {
    // The closure would panic if called; the placeholder keeps the
    // plan checkable without desktop clipboard state.
    let mut e = InputEnv {
        stdin: Box::leak(Box::new(Cursor::new(b""))),
        stdin_data_probe: Box::leak(Box::new(|| false)),
        clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
    };
    let parts = gather(&[SourceSpec::Paste], true, None, true, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].source, InputSource::Clipboard);
    assert!(parts[0].name.contains("dry-run"), "{}", parts[0].name);

    let mut e = InputEnv {
        stdin: Box::leak(Box::new(Cursor::new(b""))),
        stdin_data_probe: Box::leak(Box::new(|| false)),
        clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
    };
    let parts = gather(&[], true, None, true, &mut e).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].source, InputSource::Clipboard);
}

/// A fresh temp dir with the given relative entries (parents created
/// as needed). The same-pid pre-clean guards against a reused pid
/// picking up a pre-sweep (<24h) run root's leftovers.
fn write_dir(name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
    let dir = run_root().join(format!("aido-input-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (file, bytes) in entries {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    dir
}

#[test]
fn glob_expands_sorted_and_in_place() {
    let dir = write_dir("glob-sorted", &[("b.txt", b"b\n"), ("a.txt", b"a\n")]);
    std::fs::write(dir.join("c.md"), b"md\n").unwrap();
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let parts = gather(
        &[
            SourceSpec::Glob(pattern),
            SourceSpec::File(dir.join("c.md")),
        ],
        true,
        None,
        false,
        &mut e,
    )
    .unwrap();
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt", "c.md"]);
    assert_eq!(parts[0].source, InputSource::File(dir.join("a.txt")));
    assert_eq!(parts[2].id, 2);
}

#[test]
fn glob_without_matches_is_an_error() {
    let dir = write_dir("glob-none", &[("a.txt", b"a\n")]);
    let pattern = format!("{}/*.png", dir.display());
    let mut e = env(b"");
    let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("no files match"), "{err}");
}

#[test]
fn glob_skips_dotfiles_and_stays_in_one_level() {
    let dir = write_dir(
        "glob-dots",
        &[
            (".hidden.txt", b"h\n"),
            ("top.txt", b"t\n"),
            ("sub/nested.txt", b"n\n"),
        ],
    );
    // `*` sees the subdirectory but must not descend into it.
    let pattern = format!("{}/*", dir.display());
    let mut e = env(b"");
    let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("is a directory"), "{err}");
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap();
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["top.txt"]);
}

#[test]
fn literal_file_with_metachars_wins_over_pattern() {
    let dir = write_dir("glob-literal", &[("note[1].txt", b"literal\n")]);
    let pattern = dir.join("note[1].txt").display().to_string();
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap();
    assert_eq!(parts[0].text(), Some("literal\n"));
    // Even a name that is not a valid glob at all (unclosed `[`) takes
    // the literal exit before parsing ever runs.
    let dir = write_dir("glob-literal-raw", &[("shot[1.png", b"literal\n")]);
    let pattern = dir.join("shot[1.png").display().to_string();
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap();
    assert_eq!(parts[0].text(), Some("literal\n"));
}

#[test]
fn directory_expands_one_level_sorted() {
    let dir = write_dir(
        "dir-sorted",
        &[
            ("b.txt", b"b\n"),
            ("a.txt", b"a\n"),
            (".hidden.txt", b"h\n"),
        ],
    );
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::File(dir.clone())], true, None, false, &mut e).unwrap();
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    assert_eq!(parts[0].source, InputSource::File(dir.join("a.txt")));
}

#[test]
fn directory_with_subdirectory_is_an_error_with_a_glob_hint() {
    let dir = write_dir("dir-sub", &[("a.txt", b"a\n")]);
    std::fs::create_dir(dir.join("raw")).unwrap();
    let mut e = env(b"");
    let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("expands one level"), "{msg}");
    assert!(msg.contains("glob"), "{msg}");
}

#[test]
fn directory_without_usable_files_is_an_error() {
    let dir = write_dir("dir-empty", &[]);
    let mut e = env(b"");
    let err = gather(&[SourceSpec::File(dir.clone())], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("no input files"), "{err}");
    std::fs::write(dir.join(".dot.txt"), b"d\n").unwrap();
    let mut e = env(b"");
    let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("no input files"), "{err}");
}

#[test]
fn directory_and_dash_keep_argv_order() {
    let dir = write_dir("dir-dash", &[("a.txt", b"a\n"), ("b.txt", b"b\n")]);
    let mut e = env(b"pipe\n");
    let parts = gather(
        &[SourceSpec::File(dir), SourceSpec::Stdin],
        true,
        None,
        false,
        &mut e,
    )
    .unwrap();
    let texts: Vec<Option<&str>> = parts.iter().map(|p| p.text()).collect();
    assert_eq!(texts, vec![Some("a\n"), Some("b\n"), Some("pipe\n")]);
}

#[test]
fn expansion_caps_fail_loudly() {
    let dir = write_dir(
        "cap",
        &[("a.txt", b"a\n"), ("b.txt", b"b\n"), ("c.txt", b"c\n")],
    );
    let pattern = format!("{}/*.txt", dir.display());
    let err = expand_glob(&pattern, 2).unwrap_err();
    assert!(err.to_string().contains("narrow the pattern"), "{err}");
    let err = expand_file_spec(&dir, 2).unwrap_err();
    assert!(err.to_string().contains("narrow the input"), "{err}");
    // Exactly at the cap everything still goes through.
    let paths = expand_glob(&pattern, 3).unwrap();
    assert_eq!(paths.len(), 3);
}

#[test]
fn dry_run_still_expands_files_and_globs() {
    let dir = write_dir("dry-glob", &[("a.txt", b"a\n"), ("b.txt", b"b\n")]);
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let parts = gather(&[SourceSpec::Glob(pattern)], true, None, true, &mut e).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].text(), Some("a\n"));
}

#[test]
fn missing_file_spec_still_errors_normally() {
    let mut e = env(b"");
    let err = gather(
        &[file("definitely-missing-input.txt")],
        true,
        None,
        false,
        &mut e,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cannot read"), "{err}");
}

#[test]
fn double_star_descends_explicitly() {
    let dir = write_dir(
        "glob-recurse",
        &[
            ("top.txt", b"t\n"),
            ("sub/nested.txt", b"n\n"),
            ("sub/.hid.txt", b"h\n"),
        ],
    );
    let pattern = format!("{}/**/*.txt", dir.display());
    let paths = expand_glob(&pattern, MAX_EXPANSION).unwrap();
    // `**` matches zero directories too, and skips dotfiles on the way.
    // Compare as paths, not display strings: matched paths carry the
    // platform separator, which on Windows is not the pattern's `/`.
    assert_eq!(
        paths,
        vec![dir.join("sub").join("nested.txt"), dir.join("top.txt")],
    );
}

#[test]
fn invalid_patterns_error_cleanly() {
    // The literal file had its chance above, so a pattern that cannot
    // even parse reads as "no such file" first, syntax second — what
    // bash would say.
    let err = expand_glob("a**b", MAX_EXPANSION).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no files match 'a**b'"), "{msg}");
    assert!(msg.contains("not a valid glob pattern"), "{msg}");
    let err = expand_glob("[b", MAX_EXPANSION).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no files match '[b'"), "{msg}");
    assert!(msg.contains("not a valid glob pattern"), "{msg}");
}

#[test]
fn empty_file_inside_expansion_is_an_error_at_its_position() {
    let dir = write_dir("expansion-empty", &[("a.txt", b"a\n"), ("empty.txt", b"")]);
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
}

#[test]
fn total_budget_covers_expanded_files() {
    let dir = write_dir(
        "expansion-budget",
        &[("a.txt", b"aaaa\n"), ("b.txt", b"bbbb\n")],
    );
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let err = gather(&[SourceSpec::Glob(pattern)], true, Some(8), false, &mut e).unwrap_err();
    assert!(err.to_string().contains("exceed the total"), "{err}");
}

#[cfg(unix)]
#[test]
fn non_utf8_filename_is_an_error_not_a_crash() {
    use std::os::unix::ffi::OsStrExt;
    let dir = write_dir("glob-non-utf8", &[("ok.txt", b"ok\n")]);
    // glob 0.3 panics internally on this entry; the expectable panic
    // print on stderr is the price of keeping the run's exit a clean
    // usage error.
    let bad = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    // APFS rejects non-UTF-8 names outright ("Illegal byte sequence"),
    // so on macOS such a file cannot exist and glob can never hit it;
    // the catch_unwind guard is exercised on byte-preserving
    // filesystems (ext4 &c.) only.
    if std::fs::write(dir.join(bad), b"x\n").is_err() {
        return;
    }
    let pattern = format!("{}/*.txt", dir.display());
    let mut e = env(b"");
    let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not valid UTF-8"), "{msg}");
}

#[cfg(unix)]
#[test]
fn symlinked_directory_inside_a_directory_arg_is_refused() {
    let dir = write_dir("dir-symlink", &[("a.txt", b"a\n")]);
    std::os::unix::fs::symlink(dir.join(".."), dir.join("up")).unwrap();
    let mut e = env(b"");
    let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
    assert!(err.to_string().contains("expands one level"), "{err}");
}

#[test]
fn file_over_single_file_cap_keeps_its_message() {
    let dir = write_dir("cap-file", &[("big.bin", b"0123456789")]);
    let path = dir.join("big.bin");
    // Both budgets are exceeded: the single-file message takes priority.
    let err = read_file(&path, 4, 4).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "'{}' is 0 MB; refusing input files over 0 MB (they are fully loaded into memory)",
            path.display()
        )
    );
}

#[test]
fn file_over_remaining_budget_keeps_its_message() {
    let dir = write_dir("cap-remaining", &[("small.bin", b"0123456789")]);
    let err = read_file(&dir.join("small.bin"), MAX_PART_BYTES, 4).unwrap_err();
    assert_eq!(err.to_string(), "inputs exceed the total input limit");
    assert_eq!(
        read_file(&dir.join("small.bin"), 10, 10).unwrap(),
        b"0123456789"
    );
}

#[test]
fn stdin_over_cap_keeps_its_message() {
    let err = read_limited(&mut Cursor::new(vec![0u8; 16]), 8, "stdin").unwrap_err();
    assert_eq!(err.to_string(), "stdin exceeds the 32 MB input limit");
}

#[cfg(target_os = "linux")]
#[test]
fn size_zero_file_with_real_content_reads_and_stays_capped() {
    // /proc/self/status reports st_size 0 yet carries real content: the
    // metadata checks pass on len 0, so only the bounded read can keep
    // the per-file budget honest.
    let path = Path::new("/proc/self/status");
    assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    let bytes = read_file(path, MAX_PART_BYTES, u64::MAX).unwrap();
    assert!(bytes.len() > 8);
    let err = read_file(path, 8, u64::MAX).unwrap_err();
    assert_eq!(
        err.to_string(),
        "'/proc/self/status' is 0 MB; refusing input files over 0 MB (they are fully loaded into memory)"
    );
    let err = read_file(path, MAX_PART_BYTES, 8).unwrap_err();
    assert_eq!(err.to_string(), "inputs exceed the total input limit");
}

#[test]
fn bounded_reader_consumes_only_one_byte_past_the_cap() {
    let mut reader = Cursor::new(b"0123456789");
    assert_eq!(read_take(&mut reader, 4).unwrap(), b"01234");
    assert_eq!(reader.position(), 5);
    assert_eq!(read_limited(&mut reader, 5, "stdin").unwrap(), b"56789");
}
