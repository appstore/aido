use super::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The single-run view the historical tests are written against:
/// unwraps [`Normalized::Single`]. A chain invocation panics here —
/// chain tests use [`stages_of`].
fn normalize(args: Vec<OsString>) -> Result<StageArgv> {
    match super::normalize(args) {
        Ok(Normalized::Single { task, specs, argv }) => Ok(StageArgv { task, specs, argv }),
        Ok(Normalized::Chain { .. }) => panic!("expected a single-run normalization"),
        Ok(Normalized::Watch(_)) => panic!("expected a single-run normalization"),
        Err(e) => Err(e),
    }
}

/// The chain view: unwraps [`Normalized::Chain`] into its stages.
fn stages_of(args: &[&str]) -> Vec<StageArgv> {
    match super::normalize(os(args)).unwrap() {
        Normalized::Chain { stages } => stages,
        Normalized::Single { .. } | Normalized::Watch(_) => {
            panic!("expected a chain normalization")
        }
    }
}

/// The watch view: unwraps [`Normalized::Watch`].
fn watch_of(args: Vec<OsString>) -> WatchArgs {
    match super::normalize(args).unwrap() {
        Normalized::Watch(args) => args,
        _ => panic!("expected a watch normalization"),
    }
}

fn task_of(args: &[&str]) -> String {
    match super::normalize(os(args)).unwrap() {
        Normalized::Single { task, .. } => task.unwrap(),
        _ => panic!("expected a single-run normalization"),
    }
}

#[test]
fn task_can_come_before_or_after_flags() {
    assert_eq!(task_of(&["--profile", "local", "ocr", "scan.png"]), "ocr");
    assert_eq!(task_of(&["--copy", "ocr", "scan.png"]), "ocr");
    assert_eq!(task_of(&["ocr", "--profile", "local", "scan.png"]), "ocr");
}

#[test]
fn run_takes_the_next_free_token() {
    assert_eq!(task_of(&["run", "last", "input.txt"]), "last");
    assert_eq!(task_of(&["run", "--profile", "x", "ocr"]), "ocr");
    assert!(normalize(os(&["run"])).is_err());
}

#[test]
fn ask_via_flag_or_word() {
    assert_eq!(task_of(&["-p", "hi", "notes.md"]), "ask");
    assert_eq!(task_of(&["ask", "notes.md", "-p", "hi"]), "ask");
    assert_eq!(task_of(&["--prompt=hi"]), "ask");
    assert_eq!(task_of(&["-phi"]), "ask");
}

#[test]
fn specs_keep_cross_type_order() {
    let n = normalize(os(&[
        "ask",
        "--text",
        "图一",
        "a.png",
        "--text",
        "图二",
        "b.png",
        "-p",
        "比较两图",
    ]))
    .unwrap();
    assert_eq!(
        n.specs,
        vec![
            SourceSpec::Text("图一".into()),
            SourceSpec::File(PathBuf::from("a.png")),
            SourceSpec::Text("图二".into()),
            SourceSpec::File(PathBuf::from("b.png")),
        ]
    );
}

#[test]
fn dash_and_paste_are_slots() {
    let n = normalize(os(&["code-review", "-", "CONTRIBUTING.md"])).unwrap();
    assert_eq!(
        n.specs,
        vec![
            SourceSpec::Stdin,
            SourceSpec::File(PathBuf::from("CONTRIBUTING.md"))
        ]
    );
    let n = normalize(os(&["ocr", "--paste"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Paste]);
}

#[test]
fn separator_makes_everything_a_file() {
    let n = normalize(os(&["ocr", "--", "./-strange-name.png"])).unwrap();
    assert_eq!(
        n.specs,
        vec![SourceSpec::File(PathBuf::from("./-strange-name.png"))]
    );
}

#[test]
fn glob_tokens_become_glob_specs() {
    let n = normalize(os(&["ocr", "shots/*.png"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Glob("shots/*.png".into())]);
    // `?` and `[` are metacharacters too.
    let n = normalize(os(&["ocr", "shot-?.png"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Glob("shot-?.png".into())]);
    // A directory argument parses as a plain File; expansion is
    // gather's job, where the filesystem lives.
    let n = normalize(os(&["ocr", "shots/"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::File(PathBuf::from("shots/"))]);
    // `--text` values are literal text, never patterns.
    let n = normalize(os(&["ask", "--text", "a*b"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Text("a*b".into())]);
}

#[test]
fn separator_tokens_stay_literal_files() {
    let n = normalize(os(&["ocr", "--", "shots/*.png"])).unwrap();
    assert_eq!(
        n.specs,
        vec![SourceSpec::File(PathBuf::from("shots/*.png"))]
    );
    // `-` stays stdin even after `--`, as at every other position.
    let n = normalize(os(&["ocr", "--", "-"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Stdin]);
    // The ask path produces the same Glob spec.
    let n = normalize(os(&["-p", "hi", "shots/*.png"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Glob("shots/*.png".into())]);
}

#[test]
fn watch_parses_dir_and_task_argv() {
    let w = watch_of(os(&["watch", "shots", "--", "ocr", "--copy"]));
    assert_eq!(w.dir, PathBuf::from("shots"));
    assert_eq!(w.task_argv, os(&["ocr", "--copy"]));
    assert!(w.parent_argv.is_empty());
    assert!(!w.include_existing);
    assert_eq!(w.interval, None);
    assert_eq!(w.stable_ms, None);
}

#[test]
fn watch_flags_parse_in_both_spellings() {
    let w = watch_of(os(&[
        "watch",
        "d",
        "--interval",
        "0.5",
        "--stable-ms=250",
        "--include-existing",
        "--dry-run",
        "--quiet",
        "--json",
        "--",
        "tts",
        "--out-dir",
        "o/",
    ]));
    assert_eq!(w.interval, Some(0.5));
    assert_eq!(w.stable_ms, Some(250));
    assert!(w.include_existing);
    assert_eq!(w.task_argv, os(&["tts", "--out-dir", "o/"]));
    assert_eq!(w.parent_argv, os(&["--dry-run", "--quiet", "--json"]));
}

#[test]
fn watch_requires_dir_and_task() {
    let e = normalize(os(&["watch", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("watch requires a directory"), "{e}");
    let e = normalize(os(&["watch", "d"])).unwrap_err().to_string();
    assert!(e.contains("`--` separator"), "{e}");
    let e = normalize(os(&["watch", "d", "--"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("`--` separator"), "{e}");
    let e = normalize(os(&["watch", "a", "b", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("exactly one directory"), "{e}");
}

#[test]
fn watch_refuses_task_flags_before_separator() {
    for flag in ["--copy", "--out-dir", "-o", "-ofile"] {
        let e = normalize(os(&["watch", "d", flag, "--", "ocr"]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("belong after the `--` separator"), "{flag}: {e}");
    }
    // A bare positional is a second directory, not material: watch
    // takes its input from the filesystem, not the command line.
    let e = normalize(os(&["watch", "d", "out.png", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("exactly one directory"), "{e}");
}

#[test]
fn watch_refuses_dry_run_inside_the_task() {
    let e = normalize(os(&["watch", "d", "--", "ocr", "--dry-run"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot be part of a watched task"), "{e}");
}

#[test]
fn watch_must_open_the_command_line() {
    let e = normalize(os(&["--quiet", "watch", "d", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("'watch' must be the first word"), "{e}");
}

#[test]
fn watch_help_is_plain_help() {
    // watch --help falls back to the single-run shape, whose argv is
    // what clap prints help from.
    match super::normalize(os(&["watch", "--help"])).unwrap() {
        Normalized::Single { argv, .. } => assert_eq!(argv, os(&["--help"])),
        _ => panic!("expected the single-run help fallback"),
    }
}

#[test]
fn watch_value_flags_validate_their_values() {
    let e = normalize(os(&["watch", "d", "--interval", "x", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("number of seconds"), "{e}");
    let e = normalize(os(&["watch", "d", "--interval", "0", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("positive"), "{e}");
    let e = normalize(os(&["watch", "d", "--stable-ms", "1.5", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("whole number"), "{e}");
    let e = normalize(os(&["watch", "d", "--interval", "--", "ocr"]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("--interval requires"), "{e}");
}

#[test]
fn path_hints_are_lexical() {
    for word in [
        "/missing",
        "dir/file",
        "dir/",
        ".",
        "..",
        ".hidden",
        "missing-file.txt",
        "~",
        "~user",
        "*",
        "file?",
        "file[abc]",
        "file[",
    ] {
        assert!(looks_like_path(word), "{word:?}");
    }
    for word in ["", "transalte", "missing-file", "name~", "name]"] {
        assert!(!looks_like_path(word), "{word:?}");
    }
    // F64: even an existing bare directory is not a lexical path hint;
    // callers can use `./src` to request the file-input diagnostic.
    assert!(!looks_like_path("src"));
    assert_eq!(looks_like_path(r"dir\file"), cfg!(windows));
}

#[test]
fn lexical_path_hints_select_file_input_guidance() {
    for word in ["missing-file.txt", "~user", "*", "file?", "file["] {
        let err = normalize(os(&[word])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires a task"), "{word:?}: {msg}");
        assert!(!msg.contains("unknown task"), "{word:?}: {msg}");
    }
    let err = normalize(os(&["missing-file"])).unwrap_err();
    assert!(err.to_string().contains("unknown task"), "{err}");
}

#[test]
fn files_without_task_or_prompt_are_an_error() {
    let err = normalize(os(&["notes.txt"])).unwrap_err();
    assert!(err.to_string().contains("requires a task"));
}

#[test]
fn unknown_task_without_prompt_lists_candidates() {
    let err = normalize(os(&["transalte"])).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown task"), "{msg}");
    assert!(msg.contains("translate"), "{msg}");
}

#[test]
fn prompt_like_first_word_with_p_becomes_ask_file() {
    // -p present: a non-task word in the task slot is a file, not an error
    let n = normalize(os(&["-p", "润色这段话", "笔记.txt"])).unwrap();
    assert_eq!(n.task.as_deref(), Some("ask"));
    assert_eq!(n.specs, vec![SourceSpec::File(PathBuf::from("笔记.txt"))]);
}

#[test]
fn removed_flags_error_with_guidance() {
    for flag in [
        "--save",
        "--save-dir",
        "--input-mode",
        "--adapter",
        "--base-url",
        "--api-key",
        "--preset",
        "--init",
        "--list-presets",
        "--no-spinner",
        "--output-mode",
    ] {
        let err = normalize(os(&[flag, "x", "ask", "-p", "hi"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no longer accepted"), "{flag}: {msg}");
    }
}

#[test]
fn last_is_a_pseudo_task_keeping_flags() {
    let n = normalize(os(&["--copy", "last"])).unwrap();
    assert_eq!(n.task.as_deref(), Some(LAST_TASK));
    assert!(n.argv.contains(&OsString::from("--copy")));
    let n = normalize(os(&["last", "--out-dir", "d"])).unwrap();
    assert_eq!(n.task.as_deref(), Some(LAST_TASK));
}

#[test]
fn management_subcommands_pass_through() {
    let n = normalize(os(&["tasks", "list"])).unwrap();
    assert_eq!(n.task, None);
    assert_eq!(n.argv[0], OsString::from("tasks"));
    let n = normalize(os(&["history", "show", "x"])).unwrap();
    assert_eq!(n.argv[1], OsString::from("show"));
    let n = normalize(os(&["config", "init"])).unwrap();
    assert_eq!(n.argv[0], OsString::from("config"));
    let n = normalize(os(&["profiles"])).unwrap();
    assert_eq!(n.argv[0], OsString::from("profiles"));
}

#[test]
fn hold_passes_through_untouched() {
    // The clipboard holder child is spawned as `aido __hold SECS
    // [--image]` with the word always first; task discovery would
    // reject it ("unknown task '__hold'"), so argv must reach clap
    // exactly as spawned.
    let n = normalize(os(&["__hold", "5", "--image"])).unwrap();
    assert_eq!(n.task, None);
    assert!(n.specs.is_empty());
    assert_eq!(n.argv, os(&["__hold", "5", "--image"]));
}

#[test]
fn flag_values_are_not_mistaken_for_tasks() {
    assert_eq!(task_of(&["--profile", "local", "ocr", "a.png"]), "ocr");
    assert_eq!(
        task_of(&["--to", "zh-CN", "translate", "a.md"]),
        "translate"
    );
}

#[test]
fn bare_argv_yields_no_task() {
    let n = normalize(os(&[])).unwrap();
    assert_eq!(n.task, None);
}

#[test]
fn attached_short_values_map_to_their_own_flags() {
    // -mVALUE must reach --model, never collapse into --prompt.
    let n = normalize(os(&["summarize", "-mmodel-x", "--text", "hi"])).unwrap();
    let i = n.argv.iter().position(|a| a == "--model").unwrap();
    assert_eq!(n.argv[i + 1], OsString::from("model-x"));
    assert!(
        !n.argv.contains(&OsString::from("--prompt")),
        "{:?}",
        n.argv
    );
    // -oVALUE reaches --output.
    let n = normalize(os(&["ask", "-phi", "-ofile.mp3"])).unwrap();
    let i = n.argv.iter().position(|a| a == "--output").unwrap();
    assert_eq!(n.argv[i + 1], OsString::from("file.mp3"));
    let i = n.argv.iter().position(|a| a == "--prompt").unwrap();
    assert_eq!(n.argv[i + 1], OsString::from("hi"));
}

#[test]
fn attached_short_equals_form_resolves() {
    let n = normalize(os(&["summarize", "-m=model-x"])).unwrap();
    let i = n.argv.iter().position(|a| a == "--model").unwrap();
    assert_eq!(n.argv[i + 1], OsString::from("model-x"));
}

#[test]
fn text_flag_accepts_the_equals_form() {
    let n = normalize(os(&["ask", "--text=hi"])).unwrap();
    assert_eq!(n.specs, vec![SourceSpec::Text("hi".into())]);
}

#[test]
fn value_flags_require_their_values() {
    let err = normalize(os(&["ask", "--text"])).unwrap_err();
    assert!(err.to_string().contains("--text requires a value"), "{err}");
    let err = normalize(os(&["ask", "-p"])).unwrap_err();
    assert!(err.to_string().contains("requires a value"), "{err}");
}

#[test]
fn unknown_flag_with_value_is_not_a_file() {
    let n = normalize(os(&["summarize", "--text", "hi", "--typo=x"])).unwrap();
    // The token stays in the clap argv (which rejects it); it never
    // becomes an input file.
    assert!(n.argv.contains(&OsString::from("--typo=x")), "{:?}", n.argv);
    assert!(n.specs.iter().all(|s| !matches!(s, SourceSpec::File(_))));
}

#[test]
fn negative_numbers_stay_flag_values() {
    let n = normalize(os(&["ask", "-p", "hi", "--temperature", "-0.5"])).unwrap();
    assert!(
        n.argv.contains(&OsString::from("--temperature=-0.5")),
        "{:?}",
        n.argv
    );
}

/// Normalize + clap, exactly what app.rs does with real argv: the
/// strongest assertion a spelling works is that the real parser
/// accepts it and keeps the value.
fn parse_cli(args: &[&str]) -> Cli {
    let n = normalize(os(args)).unwrap();
    Cli::try_parse_from(std::iter::once(OsString::from("aido")).chain(n.argv.clone()))
        .unwrap_or_else(|e| panic!("{args:?}: {e}; argv: {:?}", n.argv))
}

#[test]
fn leading_dash_prompt_values_parse_end_to_end() {
    // Every spelling of a prompt that starts with '-' must reach clap
    // with the value still attached, or clap rejects it as a flag.
    for args in [
        &["ask", "--prompt", "-x"][..],
        &["ask", "--prompt=-x"][..],
        &["ask", "-p", "-x"][..],
        &["ask", "-p-x"][..],
        &["ask", "-p=-x"][..],
    ] {
        let cli = parse_cli(args);
        assert_eq!(cli.prompt.as_deref(), Some("-x"), "{args:?}");
        assert_eq!(cli.task.as_deref(), Some("ask"), "{args:?}");
    }
}

#[test]
fn leading_dash_attached_short_values_parse_end_to_end() {
    // -m-x / -o-out.txt keep the dash inside the combined long form.
    let cli = parse_cli(&["ask", "-m-x", "-p", "hi"]);
    assert_eq!(cli.model.as_deref(), Some("-x"));
    let cli = parse_cli(&["ask", "-o-out.txt", "-p", "hi"]);
    assert_eq!(cli.output, Some(PathBuf::from("-out.txt")));
}

#[test]
fn non_dash_and_lone_dash_values_stay_separated() {
    // Without a leading dash the two-token form is unchanged; the
    // lone `-` is a value (stdin elsewhere), never a flag.
    for args in [&["ask", "-phi"][..], &["ask", "--prompt", "hi"][..]] {
        let n = normalize(os(args)).unwrap();
        let i = n.argv.iter().position(|a| a == "--prompt").unwrap();
        assert_eq!(n.argv[i + 1], OsString::from("hi"), "{args:?}");
    }
    for args in [&["ask", "--prompt", "-"][..], &["ask", "--prompt=-"][..]] {
        let cli = parse_cli(args);
        assert_eq!(cli.prompt.as_deref(), Some("-"), "{args:?}");
    }
}

#[test]
fn help_and_version_are_words_not_tasks() {
    let n = normalize(os(&["help"])).unwrap();
    assert_eq!(n.argv, vec![OsString::from("--help")]);
    let n = normalize(os(&["version"])).unwrap();
    assert_eq!(n.argv, vec![OsString::from("--version")]);
}

#[test]
fn last_rejects_input_material() {
    let err = normalize(os(&["last", "notes.txt"])).unwrap_err();
    assert!(err.to_string().contains("takes no input"), "{err}");
    let n = normalize(os(&["last", "--copy"])).unwrap();
    assert_eq!(n.task.as_deref(), Some(LAST_TASK));
}

#[test]
fn prompt_has_no_effect_on_management_commands() {
    let err = normalize(os(&["-p", "hi", "tasks", "list"])).unwrap_err();
    assert!(err.to_string().contains("no effect"), "{err}");
}

#[test]
fn material_flags_have_no_effect_on_management_commands() {
    // Every management word, both material flags, both spellings of
    // --text: the material must be refused, not silently dropped.
    for args in [
        &["tasks", "list", "--text", "x"][..],
        &["profiles", "--paste"][..],
        &["config", "check", "--text", "x"][..],
        &["history", "show", "1", "--paste"][..],
        &["tasks", "list", "--text=x"][..],
    ] {
        let err = normalize(os(args)).unwrap_err();
        assert!(
            err.to_string().contains("no effect on management commands"),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn management_passthrough_without_material() {
    // No material, no refusal: the words reach clap untouched.
    for args in [&["tasks", "list"][..], &["history", "show", "1"][..]] {
        let n = normalize(os(args)).unwrap();
        assert!(n.task.is_none(), "{args:?}");
        assert!(n.specs.is_empty(), "{args:?}");
        assert!(
            n.argv.contains(&OsString::from(args[0])),
            "{args:?}: {:?}",
            n.argv
        );
    }
}

#[test]
fn management_keeps_post_separator_literals() {
    let n = normalize(os(&["history", "show", "--", "--weird-id"])).unwrap();
    assert!(
        n.argv.contains(&OsString::from("--weird-id")),
        "{:?}",
        n.argv
    );
}

#[test]
fn wants_json_scan_mirrors_flag_arity() {
    // A "--json" that is a value-taking flag's value is not a request.
    assert!(!wants_json_in(&os(&["ask", "-p", "--json"])));
    assert!(!wants_json_in(&os(&["summarize", "--text", "--json"])));
    assert!(!wants_json_in(&os(&["--prompt=--json"])));
    assert!(!wants_json_in(&os(&["-p=--json"])));
    assert!(!wants_json_in(&os(&["-p--json"])));
    assert!(!wants_json_in(&os(&["-m", "--json"])));
    assert!(!wants_json_in(&os(&["-o", "--json"])));
    assert!(!wants_json_in(&os(&["--model", "--json"])));
    // Past the separator everything is a literal path.
    assert!(!wants_json_in(&os(&["--", "--json"])));
    assert!(!wants_json_in(&os(&["ask", "--", "--json"])));
    // A real --json flag, wherever it stands.
    assert!(wants_json_in(&os(&["--json"])));
    assert!(wants_json_in(&os(&["--json", "--bogus"])));
    assert!(wants_json_in(&os(&["ask", "--json", "-p", "hi"])));
    assert!(wants_json_in(&os(&["--text", "hi", "--json"])));
    // `-p` swallows the separator as its value, so a later --json
    // still parses as the flag — the normalizer consumes it too.
    assert!(wants_json_in(&os(&["-p", "--", "--json"])));
}

/// The pre-normalize scan sees `--json` inside the chain spec — one
/// shell token — so a normalize-time error (an unknown task) still
/// prints the JSON envelope. The judgment goes through the real
/// tokenizer and the arity-aware flag scan, never a substring check.
#[test]
fn raw_json_scan_sees_json_inside_chain_spec() {
    assert!(wants_json_before_normalize(&os(&[
        "chain",
        "summarize --json | transalte",
        "--text",
        "hi",
    ])));
    // The --then form is top-level tokens; the plain scan covers it.
    assert!(wants_json_before_normalize(&os(&[
        "summarize",
        "--then",
        "translate",
        "--json",
    ])));
    // A flag outside the spec still counts, spec or not.
    assert!(wants_json_before_normalize(&os(&[
        "chain",
        "summarize | translate",
        "--json",
    ])));
}

#[test]
fn raw_json_scan_ignores_json_used_as_stage_flag_value() {
    // `-p` swallows "--json" as its prompt, inside the spec or out.
    assert!(!wants_json_before_normalize(&os(&[
        "chain",
        "summarize -p --json | transalte",
        "--text",
        "hi",
    ])));
    assert!(!wants_json_before_normalize(&os(&[
        "chain",
        "summarize | translate -p --json",
        "--text",
        "hi",
    ])));
    // No chain sugar, no spec to look into.
    assert!(!wants_json_before_normalize(&os(&[
        "summarize",
        "--text",
        "hi"
    ])));
    // The chain word without a spec never reaches the tokenizer.
    assert!(!wants_json_before_normalize(&os(&["chain", "--help"])));
}

#[cfg(unix)]
#[test]
fn wants_json_scan_handles_non_utf8_tokens() {
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(vec![0xff, 0xfe]);
    // A non-UTF-8 token is a file path, never a flag: the scan goes on.
    assert!(wants_json_in(&[bad.clone(), OsString::from("--json")]));
    // A value flag still swallows it; the skip works on OsStrings.
    assert!(wants_json_in(&[
        OsString::from("-p"),
        bad,
        OsString::from("--json")
    ]));
}

#[cfg(unix)]
#[test]
fn text_flag_values_must_be_valid_utf8() {
    use std::os::unix::ffi::OsStringExt;
    // "你好" in GBK: real bytes a Chinese Windows shell would pass, and
    // invalid UTF-8 — a lossy conversion would hand the model two
    // U+FFFD instead of the words the user typed.
    let gbk = OsString::from_vec(vec![0xc4, 0xe3, 0xba, 0xc3]);
    // Every separated spelling refuses the bytes and names its flag.
    for (flag, argv) in [
        (
            "--text",
            vec![OsString::from("ask"), OsString::from("--text"), gbk.clone()],
        ),
        (
            "--prompt",
            vec![
                OsString::from("ask"),
                OsString::from("--prompt"),
                gbk.clone(),
            ],
        ),
        (
            "-p/--prompt",
            vec![OsString::from("ask"), OsString::from("-p"), gbk.clone()],
        ),
    ] {
        let err = normalize(argv).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(flag), "{flag}: {msg}");
        assert!(msg.contains("valid UTF-8"), "{flag}: {msg}");
    }
    // Combined spellings embed the bytes in the token itself; the same
    // refusal fires instead of the token quietly becoming a file named
    // after the flag.
    for (flag, token) in [
        ("--text", concat_bytes(b"--text=", &gbk)),
        ("--prompt", concat_bytes(b"--prompt=", &gbk)),
        ("-p/--prompt", concat_bytes(b"-p", &gbk)),
        ("-p/--prompt", concat_bytes(b"-p=", &gbk)),
    ] {
        let err = normalize(vec![OsString::from("ask"), token]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(flag), "{flag}: {msg}");
        assert!(msg.contains("valid UTF-8"), "{flag}: {msg}");
    }
}

#[cfg(unix)]
fn concat_bytes(prefix: &[u8], value: &std::ffi::OsStr) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(value.as_encoded_bytes());
    OsString::from_vec(bytes)
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_survive_but_never_become_text() {
    use std::os::unix::ffi::OsStringExt;
    let bad_name = OsString::from_vec(vec![0xff, 0xfe, 0x2e, 0x70, 0x6e, 0x67]);
    // A non-UTF-8 token that is not flag-shaped keeps its raw bytes as
    // a file path — the exact contrast that makes refusing the text
    // spellings right.
    let n = normalize(vec![
        OsString::from("ask"),
        OsString::from("-p"),
        OsString::from("hi"),
        bad_name.clone(),
    ])
    .unwrap();
    assert_eq!(n.specs[0], SourceSpec::File(PathBuf::from(&bad_name)));
    // After `--` even a flag-shaped token is a literal file.
    let token = concat_bytes(b"--text=", &OsString::from_vec(vec![0xc4, 0xe3]));
    let n = normalize(vec![
        OsString::from("ocr"),
        OsString::from("--"),
        token.clone(),
    ])
    .unwrap();
    assert_eq!(n.specs[0], SourceSpec::File(PathBuf::from(&token)));
}

#[test]
fn then_form_splits_into_stages() {
    let stages = stages_of(&[
        "ocr",
        "a.png",
        "--then",
        "translate",
        "--to",
        "zh-CN",
        "--then",
        "tts",
        "-o",
        "out.mp3",
    ]);
    assert_eq!(stages.len(), 3);
    assert_eq!(stages[0].task.as_deref(), Some("ocr"));
    assert_eq!(
        stages[0].specs,
        vec![SourceSpec::File(PathBuf::from("a.png"))]
    );
    assert_eq!(stages[1].task.as_deref(), Some("translate"));
    assert!(stages[1]
        .argv
        .windows(2)
        .any(|w| w == [OsString::from("--to"), OsString::from("zh-CN")]));
    assert_eq!(stages[2].task.as_deref(), Some("tts"));
    assert!(stages[2].argv.contains(&OsString::from("-o")));
}

#[test]
fn then_prompt_value_swallows_the_marker() {
    // `-p --then` is a prompt of "--then", not a stage boundary; the
    // next real marker splits.
    let stages = stages_of(&["ask", "-p", "--then", "--then", "tts"]);
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0].task.as_deref(), Some("ask"));
    let argv = stages[0]
        .argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        argv.iter().any(|a| a.contains("--then")),
        "the prompt's value must survive: {argv:?}"
    );
    assert_eq!(stages[1].task.as_deref(), Some("tts"));
}

#[test]
fn then_after_separator_is_literal() {
    // After `--` nothing splits: a file literally named "--then" is
    // material, and the separator's stage simply ends the argv's
    // marker interpretation (put later stages before `--`).
    let n = normalize(os(&["ocr", "--", "--then", "f.png", "--then", "tts"])).unwrap();
    assert_eq!(
        n.specs,
        vec![
            SourceSpec::File(PathBuf::from("--then")),
            SourceSpec::File(PathBuf::from("f.png")),
            SourceSpec::File(PathBuf::from("--then")),
            SourceSpec::File(PathBuf::from("tts")),
        ]
    );
    // A separator inside the last stage is fine: its material stays
    // literal while the marker before it still split.
    let stages = stages_of(&["ocr", "a.png", "--then", "tts", "--", "--then"]);
    assert_eq!(stages.len(), 2);
    assert_eq!(
        stages[1].specs,
        vec![SourceSpec::File(PathBuf::from("--then"))]
    );
}

#[test]
fn then_first_stage_without_a_task_is_rejected() {
    let err = super::normalize(os(&["--then", "tts"])).unwrap_err();
    assert!(err.to_string().contains("must start with a task"), "{err}");
}

#[test]
fn chain_sugar_desugars_material_and_outer_flags() {
    let stages = stages_of(&[
        "chain",
        "ocr | translate --to zh-CN | tts",
        "shot.png",
        "-o",
        "brief.mp3",
    ]);
    assert_eq!(stages.len(), 3);
    assert_eq!(stages[0].task.as_deref(), Some("ocr"));
    assert_eq!(
        stages[0].specs,
        vec![SourceSpec::File(PathBuf::from("shot.png"))]
    );
    assert_eq!(stages[1].task.as_deref(), Some("translate"));
    assert!(stages[1]
        .argv
        .windows(2)
        .any(|w| w == [OsString::from("--to"), OsString::from("zh-CN")]));
    assert_eq!(stages[2].task.as_deref(), Some("tts"));
    let argv = stages[2]
        .argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(argv.contains(&"-o".to_string()) && argv.contains(&"brief.mp3".to_string()));
}

#[test]
fn chain_outer_flags_are_position_independent() {
    // Flags before the chain word land on the last stage too.
    let stages = stages_of(&["--json", "chain", "ocr|tts", "shot.png", "--copy"]);
    assert_eq!(stages.len(), 2);
    for flag in ["--json", "--copy"] {
        assert!(
            stages[1].argv.contains(&OsString::from(flag)),
            "last stage missing {flag}: {:?}",
            stages[1].argv
        );
    }
    assert!(stages[0].specs.len() == 1);
}

#[test]
fn chain_stage_level_flag_outside_the_spec_is_rejected() {
    let err = super::normalize(os(&["chain", "ocr|tts", "--to", "zh-CN"])).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("--to"), "{msg}");
    assert!(msg.contains("inside the chain spec"), "{msg}");
}

#[test]
fn chain_needs_two_stages() {
    let err = super::normalize(os(&["chain", "ocr", "shot.png"])).unwrap_err();
    assert!(err.to_string().contains("at least two stages"), "{err}");
}

#[test]
fn chain_empty_stage_is_named() {
    let err = super::normalize(os(&["chain", "ocr || tts"])).unwrap_err();
    assert!(err.to_string().contains("stage 2 is empty"), "{err}");
}

#[test]
fn chain_unclosed_quote_points_at_then() {
    let err = super::normalize(os(&["chain", "ocr | 'tts"])).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unclosed"), "{msg}");
    assert!(msg.contains("--then"), "{msg}");
}

#[test]
fn chain_unclosed_quote_names_the_stage() {
    // The double-quote variant: the error names the 1-based stage the
    // spec died in, like the empty-stage error does.
    let err = tokenize_chain_spec("ocr | translate \"zh").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stage 2"), "{msg}");
    assert!(msg.contains("unclosed"), "{msg}");
}

#[test]
fn chain_and_then_cannot_mix() {
    let err = super::normalize(os(&["chain", "ocr|tts", "--then", "ask"])).unwrap_err();
    assert!(err.to_string().contains("cannot mix"), "{err}");
}

#[test]
fn chain_material_flags_reach_stage1() {
    let stages = stages_of(&["chain", "ask -p '总结' | tts", "--text", "你好"]);
    assert_eq!(stages[0].task.as_deref(), Some("ask"));
    assert!(stages[0].specs.contains(&SourceSpec::Text("你好".into())));
}

#[test]
fn chain_material_after_separator_stays_literal() {
    let stages = stages_of(&["chain", "ocr|tts", "--", "shots/*.png"]);
    assert_eq!(
        stages[0].specs,
        vec![SourceSpec::File(PathBuf::from("shots/*.png"))]
    );
}

#[test]
fn chain_spec_unknown_task_gets_did_you_mean() {
    let err = super::normalize(os(&["chain", "ocr|transalte"])).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown task 'transalte'"), "{msg}");
    assert!(msg.contains("translate"), "{msg}");
}

#[cfg(unix)]
#[test]
fn non_utf8_token_before_the_chain_word_is_not_swallowed() {
    use std::os::unix::ffi::OsStringExt;
    // A non-UTF-8 free token ahead of "chain" means the chain word
    // was never first: refuse as file-without-task instead of
    // silently eating the bytes and misreading the spec.
    let bad = OsString::from_vec(vec![0xff, 0xfe]);
    let argv = vec![bad, OsString::from("chain"), OsString::from("ocr|tts")];
    let err = super::normalize(argv).unwrap_err();
    assert!(err.to_string().contains("requires a task"), "{err}");
}

#[test]
fn tokenizer_cuts_segments_and_tokens_quote_aware() {
    let stages = tokenize_chain_spec("ocr |  translate  --to 'zh-CN' |tts").unwrap();
    assert_eq!(
        stages,
        vec![
            vec!["ocr".to_string()],
            vec![
                "translate".to_string(),
                "--to".to_string(),
                "zh-CN".to_string()
            ],
            vec!["tts".to_string()],
        ]
    );
    // `|` inside quotes is literal; a quoted empty is an argument.
    let stages = tokenize_chain_spec("ask -p 'a|b' ''|tts").unwrap();
    assert_eq!(
        stages,
        vec![
            vec![
                "ask".to_string(),
                "-p".to_string(),
                "a|b".to_string(),
                "".to_string(),
            ],
            vec!["tts".to_string()],
        ]
    );
    // Double quotes survive into the token for clap to see verbatim.
    let stages = tokenize_chain_spec("ask -p \"总结 这页\"|tts").unwrap();
    assert_eq!(stages[0][2], "总结 这页");
}
