use super::*;

#[test]
fn builtin_tasks_load_and_are_consistent() {
    let _lock = table_lock();
    let all = load_all().unwrap();
    for name in [
        "ask",
        "code-review",
        "ocr",
        "summarize",
        "transcribe",
        "translate",
        "tts",
        "image",
    ] {
        let task = all.get(name).unwrap_or_else(|| panic!("missing {name}"));
        assert!(!task.output_types.is_empty());
    }
    let ocr = &all["ocr"];
    assert_eq!(ocr.processor, ProcessorKind::OcrTiles);
    assert!(ocr.required_types.contains(&MediaKind::Image));
    // The two text strategies are separate now: translate joins the
    // chunk replies, summarize consolidates them in one more request.
    assert_eq!(all["translate"].processor, ProcessorKind::ChunkJoin);
    assert_eq!(all["summarize"].processor, ProcessorKind::ChunkReduce);
    let tts = &all["tts"];
    assert_eq!(tts.operation, Operation::Speech);
    // tts is useless without material to speak
    assert!(tts.requires_material);
    // ask runs without material
    assert!(!all["ask"].requires_material);
    // transcribe allows exactly one audio
    let tr = &all["transcribe"];
    assert_eq!(tr.max_inputs, Some(1));
}

#[test]
fn unknown_fields_are_rejected_with_context() {
    let err = parse_task(
        "x",
        "operation = 'generate'\noutput_types = ['text']\nsistem = 'x'\n",
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("x"));
}

#[test]
fn non_defaultable_defaults_keys_are_rejected_with_their_destinations() {
    // speed is a real parameter but CLI/options-only: a task default
    // for it was silently ignored before.
    let err = parse_task(
        "x",
        "operation = 'speech'\noutput_types = ['audio']\nparams = ['voice', 'speed']\n\
             [defaults]\nspeed = 1.2\n",
        false,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("speed"), "{msg}");
    assert!(msg.contains("[options]"), "{msg}");

    // voice defaults only when the task declares the parameter.
    let err = parse_task(
        "x",
        "operation = 'speech'\noutput_types = ['audio']\nparams = ['speed']\n\
             [defaults]\nvoice = 'alloy'\n",
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("voice"), "{}", err.to_string());

    // the defaultable pair keeps working: declared to with a default.
    let task = parse_task(
        "x",
        "operation = 'generate'\noutput_types = ['text']\nparams = ['to']\n\
             [defaults]\nto = 'auto'\n",
        false,
    )
    .unwrap();
    assert_eq!(task.default_param("to").unwrap(), "auto");
}

#[test]
fn processor_names_parse_with_the_old_name_aliasing_join() {
    let parse = |processor: &str| {
        parse_task(
            "x",
            &format!(
                "operation = 'generate'\noutput_types = ['text']\nprocessor = '{processor}'\n"
            ),
            false,
        )
        .unwrap()
        .processor
    };
    assert_eq!(parse("chunk-join"), ProcessorKind::ChunkJoin);
    assert_eq!(parse("chunk-reduce"), ProcessorKind::ChunkReduce);
    // The pre-split name keeps its old meaning: join, no reduce.
    assert_eq!(parse("chunk-map-reduce"), ProcessorKind::ChunkJoin);
}

#[test]
fn closest_suggests() {
    assert_eq!(
        closest("transalte", &["translate", "ocr"]),
        Some("translate")
    );
}

/// The task table is process-cached; these tests set AIDO_TASKS_DIR and
/// drop the cache, so they (and anything else reading the table) must
/// not interleave. One lock for the whole module keeps the env var and
/// the cache coherent without a dev-dependency on serial test runners.
fn table_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap()
}

/// Guard that restores an environment variable on drop (None = it was
/// absent), so a failing assert cannot poison later tests.
struct RestoreEnv(&'static str, Option<String>);
impl Drop for RestoreEnv {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

#[test]
fn invalidate_reloads_new_and_removed_task_files() {
    let _lock = table_lock();
    let dir = std::env::temp_dir().join(format!(
        "aido-tasks-cache-{}-{}",
        std::process::id(),
        crate::history::stamp_now()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let _restore = RestoreEnv("AIDO_TASKS_DIR", std::env::var("AIDO_TASKS_DIR").ok());
    std::env::set_var("AIDO_TASKS_DIR", &dir);
    invalidate();

    // The cached table answers built-ins only…
    assert!(!load_all().unwrap().contains_key("ui-cache-test"));
    // …a new file lands in the table only after the cache is dropped,
    // which is exactly what the UI's task editor does after writing.
    std::fs::write(
        dir.join("ui-cache-test.toml"),
        "operation = 'generate'\noutput_types = ['text']\n",
    )
    .unwrap();
    assert!(!load_all().unwrap().contains_key("ui-cache-test"));
    invalidate();
    let reloaded = load_all().unwrap();
    let task = reloaded.get("ui-cache-test").expect("reloaded");
    assert!(!task.builtin);
    assert_eq!(task.operation, Operation::Generate);

    // …and removing the file disappears the same way.
    std::fs::remove_file(dir.join("ui-cache-test.toml")).unwrap();
    invalidate();
    assert!(!load_all().unwrap().contains_key("ui-cache-test"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn file_path_and_builtin_source_answer_the_two_origins() {
    let _lock = table_lock();
    let _restore = RestoreEnv("AIDO_TASKS_DIR", std::env::var("AIDO_TASKS_DIR").ok());
    std::env::set_var("AIDO_TASKS_DIR", "/tmp/aido-tasks-probe");
    assert_eq!(
        file_path("my-task").unwrap(),
        std::path::Path::new("/tmp/aido-tasks-probe/my-task.toml")
    );
    // The embedded source is the built-in's own bytes, not a file read.
    let ask = builtin_source("ask").expect("ask is built-in");
    assert!(
        ask.contains("operation") && ask.contains("generate"),
        "{ask}"
    );
    assert!(builtin_source("no-such-builtin").is_none());
}
