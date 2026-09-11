//! Material collection: ordered `SourceSpec`s become ordered `InputPart`s.
//!
//! Decision table (contract §2.2):
//!
//! | explicit material | piped stdin data | behavior                          |
//! |-------------------|------------------|-----------------------------------|
//! | none              | yes              | read stdin; empty is an error     |
//! | contains `-`      | yes              | read stdin at the `-` position    |
//! | none              | no               | clipboard (or instruction-only)   |
//! | some, no `-`      | yes              | error: consume the pipe with `-`  |
//! | some              | no               | explicit material; `-` reads EOF  |
//!
//! "Piped stdin data" means stdin actually carries unread bytes — a closed
//! pipe or `/dev/null` (what CI runners, cron and `docker run` without `-t`
//! attach) counts as *no* data, so automation keeps working with explicit
//! material. Only stdin that really holds bytes demands a `-`.
//!
//! A glob spec (a pattern the shell could not expand — quoted on Unix,
//! always on Windows) or a directory spec expands in place to its sorted
//! file list before any bytes are read: no match is an error, a directory
//! descends exactly one level, and dotfiles are never picked up.
//!
//! Nothing is ever silently dropped or re-sourced: an empty file, empty
//! stdin or an empty clipboard is an error at the position it occurred.

use crate::cli::SourceSpec;
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::io::{IsTerminal, Read};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

/// One material slot fully read into memory.
const MAX_PART_BYTES: u64 = 32 * 1024 * 1024;
/// Budget for the whole run's material (settings.input_bytes overrides).
const DEFAULT_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
/// Cap on the files one glob/directory spec may expand to: a typo
/// (`shots/*` over $HOME) must fail loudly, not become a half-read
/// mountain of material.
const MAX_EXPANSION: usize = 4096;

/// Injectable environment so tests never touch the real clipboard or tty.
pub struct InputEnv<'a> {
    pub stdin_is_terminal: bool,
    stdin: &'a mut dyn Read,
    /// Whether stdin actually carries unread bytes right now; a fresh
    /// process gets [`fd0_has_unread_bytes`], tests hand in a plain bool.
    stdin_data_probe: &'a mut (dyn FnMut() -> bool + 'a),
    clipboard: &'a mut (dyn FnMut() -> Result<crate::clipboard::ClipboardContent> + 'a),
}

impl InputEnv<'static> {
    pub fn real() -> Self {
        // Leaks nothing: stdin and the clipboard live for the process.
        Self {
            stdin_is_terminal: std::io::stdin().is_terminal(),
            stdin: Box::leak(Box::new(std::io::stdin())),
            stdin_data_probe: Box::leak(Box::new(fd0_has_unread_bytes)),
            clipboard: Box::leak(Box::new(crate::clipboard::read)),
        }
    }
}

impl<'a> InputEnv<'a> {
    pub fn custom(
        stdin: &'a mut dyn Read,
        stdin_is_terminal: bool,
        stdin_data_probe: &'a mut (dyn FnMut() -> bool + 'a),
        clipboard: &'a mut dyn FnMut() -> Result<crate::clipboard::ClipboardContent>,
    ) -> Self {
        Self {
            stdin_is_terminal,
            stdin,
            stdin_data_probe,
            clipboard,
        }
    }

    /// Whether stdin holds unread bytes — not just "is not a terminal".
    /// This is what separates a real pipe (`cat x | aido ...`) from the
    /// closed pipe or `/dev/null` CI attaches.
    fn stdin_has_data(&mut self) -> bool {
        (self.stdin_data_probe)()
    }

    fn read_stdin(&mut self) -> &mut dyn Read {
        self.stdin
    }

    fn read_clipboard(&mut self) -> Result<crate::clipboard::ClipboardContent> {
        (self.clipboard)()
    }
}

/// Whether fd 0 actually carries unread bytes, by handle kind:
///
/// * character devices (`/dev/null`) — never;
/// * a redirected file — whatever remains past the current offset;
/// * pipes and sockets — the exact pending byte count, with a
///   zero-timeout poll as the last resort.
///
/// When nothing can be determined the answer is `true`: the caller only
/// uses this to *reject* a run, so uncertainty keeps the old strict
/// behavior instead of silently ignoring possible input.
#[cfg(unix)]
fn fd0_has_unread_bytes() -> bool {
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(libc::STDIN_FILENO, &mut stat) != 0 {
            return true;
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFCHR => false,
            libc::S_IFREG => {
                let offset = libc::lseek(libc::STDIN_FILENO, 0, libc::SEEK_CUR);
                stat.st_size > offset
            }
            _ => {
                let mut pending: libc::c_int = 0;
                if libc::ioctl(
                    libc::STDIN_FILENO,
                    libc::FIONREAD as libc::c_ulong,
                    &mut pending,
                ) == 0
                {
                    pending > 0
                } else {
                    let mut fds = [libc::pollfd {
                        fd: libc::STDIN_FILENO,
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    libc::poll(fds.as_mut_ptr(), 1, 0) > 0 && fds[0].revents & libc::POLLIN != 0
                }
            }
        }
    }
}

/// Windows twin of the unix probe: pipes are peeked, redirected files
/// compare size against the current position, and character devices (the
/// `NUL` automation attaches) never carry bytes.
#[cfg(windows)]
fn fd0_has_unread_bytes() -> bool {
    use std::os::raw::{c_int, c_ulong, c_void};
    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: c_ulong = 0xFFFF_FFF6;
    const FILE_TYPE_DISK: c_ulong = 1;
    const FILE_TYPE_PIPE: c_ulong = 3;
    const FILE_CURRENT: c_ulong = 1;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(kind: c_ulong) -> Handle;
        fn GetFileType(handle: Handle) -> c_ulong;
        fn PeekNamedPipe(
            handle: Handle,
            buffer: *mut c_void,
            buffer_size: c_ulong,
            bytes_read: *mut c_ulong,
            total_bytes_avail: *mut c_ulong,
            bytes_left_in_message: *mut c_ulong,
        ) -> c_int;
        fn GetFileSizeEx(handle: Handle, size: *mut i64) -> c_int;
        fn SetFilePointerEx(
            handle: Handle,
            distance: i64,
            new_position: *mut i64,
            method: c_ulong,
        ) -> c_int;
    }
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        if stdin.is_null() {
            return true;
        }
        match GetFileType(stdin) {
            FILE_TYPE_DISK => {
                let mut size = 0i64;
                let mut position = 0i64;
                if GetFileSizeEx(stdin, &mut size) != 0
                    && SetFilePointerEx(stdin, 0, &mut position, FILE_CURRENT) != 0
                {
                    size > position
                } else {
                    true
                }
            }
            FILE_TYPE_PIPE => {
                let mut available = 0u32;
                PeekNamedPipe(
                    stdin,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                ) != 0
                    && available > 0
            }
            _ => false,
        }
    }
}

/// Read every spec into an ordered list of parts. `requires_material`
/// decides whether a task with no specs and a terminal stdin may run on
/// the instruction alone. Under `dry_run` the clipboard is never touched:
/// paste slots become clearly-labeled placeholders so a plan can be
/// checked without reading (or depending on) desktop state.
pub fn gather(
    specs: &[SourceSpec],
    requires_material: bool,
    total_limit: Option<u64>,
    dry_run: bool,
    env: &mut InputEnv<'_>,
) -> Result<Vec<InputPart>> {
    if specs
        .iter()
        .filter(|s| matches!(s, SourceSpec::Stdin))
        .count()
        > 1
    {
        bail!("stdin can be read once; `-` appears multiple times");
    }
    if specs
        .iter()
        .filter(|s| matches!(s, SourceSpec::Paste))
        .count()
        > 1
    {
        bail!("the clipboard can be read once; --paste appears multiple times");
    }

    let limit = total_limit.unwrap_or(DEFAULT_TOTAL_BYTES);

    if specs.is_empty() {
        if !env.stdin_is_terminal {
            let bytes = read_limited(env.stdin, MAX_PART_BYTES, "stdin")?;
            if bytes.is_empty() {
                bail!("stdin is empty; nothing to send (the clipboard is never a fallback)");
            }
            return Ok(vec![classify("stdin", bytes, InputSource::Stdin, 0)?]);
        }
        if requires_material {
            if dry_run {
                return Ok(vec![dry_run_clipboard_part(0)]);
            }
            let content = env
                .read_clipboard()
                .map_err(|e| crate::domain::AppError::usage(format!("clipboard: {e}")))?;
            return Ok(vec![clipboard_part(content, 0)?]);
        }
        // No material at all: the instruction alone drives the run.
        return Ok(Vec::new());
    }

    if env.stdin_has_data() && !specs.iter().any(|s| matches!(s, SourceSpec::Stdin)) {
        bail!(
            "stdin is piped but not consumed; add `-` where the piped data belongs \
             (e.g. `aido code-review -`), or redirect stdin from the terminal"
        );
    }

    let mut parts = Vec::new();
    let mut total: u64 = 0;
    for spec in specs {
        match spec {
            SourceSpec::File(path) => {
                for path in expand_file_spec(path, MAX_EXPANSION)? {
                    push_material(&mut parts, &mut total, limit, &path)?;
                }
            }
            SourceSpec::Glob(pattern) => {
                for path in expand_glob(pattern, MAX_EXPANSION)? {
                    push_material(&mut parts, &mut total, limit, &path)?;
                }
            }
            SourceSpec::Stdin => {
                let bytes = read_limited(env.read_stdin(), MAX_PART_BYTES, "stdin")?;
                if bytes.is_empty() {
                    bail!("stdin is empty; nothing to send");
                }
                let part = classify("stdin", bytes, InputSource::Stdin, parts.len())?;
                push_part(&mut parts, &mut total, limit, part)?;
            }
            SourceSpec::Paste => {
                if dry_run {
                    parts.push(dry_run_clipboard_part(parts.len()));
                    continue;
                }
                let content = env
                    .read_clipboard()
                    .map_err(|e| crate::domain::AppError::usage(format!("clipboard: {e}")))?;
                let part = clipboard_part(content, parts.len())?;
                push_part(&mut parts, &mut total, limit, part)?;
            }
            SourceSpec::Text(value) => {
                if value.trim().is_empty() {
                    bail!("--text is empty or whitespace-only");
                }
                let part = InputPart {
                    id: parts.len(),
                    source: InputSource::Literal,
                    name: format!("--text #{}", parts.len() + 1),
                    kind: MediaKind::Text,
                    unknown_kind: false,
                    mime: "text/plain".into(),
                    content: InputContent::Text(value.clone()),
                };
                push_part(&mut parts, &mut total, limit, part)?;
            }
        }
    }
    Ok(parts)
}

/// Append a part and charge it to the run's shared budget.
fn push_part(
    parts: &mut Vec<InputPart>,
    total: &mut u64,
    limit: u64,
    part: InputPart,
) -> Result<()> {
    *total = total.saturating_add(part_size(&part) as u64);
    if *total > limit {
        bail!("inputs exceed the {} MB total limit", limit / (1024 * 1024));
    }
    parts.push(part);
    Ok(())
}

/// Read one file fully and append its part.
fn push_material(
    parts: &mut Vec<InputPart>,
    total: &mut u64,
    limit: u64,
    path: &Path,
) -> Result<()> {
    let bytes = read_file(path, MAX_PART_BYTES, limit.saturating_sub(*total))?;
    let origin = path.display().to_string();
    let part = classify(
        &origin,
        bytes,
        InputSource::File(path.to_path_buf()),
        parts.len(),
    )?;
    push_part(parts, total, limit, part)
}

fn part_size(part: &InputPart) -> usize {
    match &part.content {
        InputContent::Text(s) => s.len(),
        InputContent::Media(b) => b.len(),
    }
}

/// A stand-in for clipboard material under `--dry-run`: the plan can be
/// checked without reading (or requiring) desktop clipboard state. The
/// real run still reads the clipboard and still fails on an empty one.
/// `unknown_kind` marks the kind above as a placeholder: type validation
/// cannot judge what has not been read, so the plan stays checkable.
fn dry_run_clipboard_part(id: usize) -> InputPart {
    InputPart {
        id,
        source: InputSource::Clipboard,
        name: "clipboard (not read under --dry-run)".into(),
        kind: MediaKind::Text,
        unknown_kind: true,
        mime: "text/plain".into(),
        content: InputContent::Text(String::new()),
    }
}

fn clipboard_part(content: crate::clipboard::ClipboardContent, id: usize) -> Result<InputPart> {
    match content {
        crate::clipboard::ClipboardContent::Text(t) => {
            if t.trim().is_empty() {
                bail!("the clipboard is empty; nothing to send");
            }
            Ok(InputPart {
                id,
                source: InputSource::Clipboard,
                name: "clipboard".into(),
                kind: MediaKind::Text,
                unknown_kind: false,
                mime: "text/plain".into(),
                content: InputContent::Text(t),
            })
        }
        crate::clipboard::ClipboardContent::Png(png) => {
            if png.is_empty() {
                bail!("the clipboard is empty; nothing to send");
            }
            Ok(InputPart {
                id,
                source: InputSource::Clipboard,
                name: "clipboard.png".into(),
                kind: MediaKind::Image,
                unknown_kind: false,
                mime: "image/png".into(),
                content: InputContent::Media(png),
            })
        }
    }
}

fn read_file(path: &Path, max: u64, remaining: u64) -> Result<Vec<u8>> {
    if path.is_dir() {
        bail!("'{}' is a directory, not a file", path.display());
    }
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    if meta.len() > max {
        bail!(
            "'{}' is {} MB; refusing input files over {} MB (they are fully loaded into memory)",
            path.display(),
            meta.len() / (1024 * 1024),
            max / (1024 * 1024)
        );
    }
    if meta.len() > remaining {
        bail!("inputs exceed the total input limit");
    }
    std::fs::read(path).with_context(|| format!("cannot read '{}'", path.display()))
}

fn read_limited(reader: &mut dyn Read, max: u64, origin: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(max + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("failed to read {origin}"))?;
    if buf.len() as u64 > max {
        bail!("{origin} exceeds the 32 MB input limit");
    }
    Ok(buf)
}

/// One path per `File` spec — except a directory, which becomes its
/// one-level, dot-free, lexicographically sorted files. Descending further
/// is refused with a glob hint so nothing is picked up implicitly.
fn expand_file_spec(path: &Path, cap: usize) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files = Vec::new();
    let entries =
        std::fs::read_dir(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read '{}'", path.display()))?;
        // Dotfiles (including dot directories like .git) are skipped
        // before the directory check, per the shell convention.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        if entry_path.is_dir() {
            bail!(
                "'{}' is a directory; a directory argument expands one level — \
                 use a glob like '{}/*' to select files inside it",
                entry_path.display(),
                entry_path.display()
            );
        }
        files.push(entry_path);
        if files.len() > cap {
            bail!(
                "'{}' holds more than {cap} files; narrow the input",
                path.display()
            );
        }
    }
    if files.is_empty() {
        bail!("'{}' contains no input files", path.display());
    }
    files.sort();
    Ok(files)
}

/// Expand a pattern the shell could not (quoted on Unix, always on
/// Windows): files only, sorted, explicit error on no match. A literal
/// file whose name contains metacharacters wins whenever it exists — the
/// closest aido can get to the shell's "quoting disables expansion"
/// without seeing the quotes.
fn expand_glob(pattern: &str, cap: usize) -> Result<Vec<PathBuf>> {
    if Path::new(pattern).is_file() {
        return Ok(vec![PathBuf::from(pattern)]);
    }
    // On Windows `\` separates paths instead of escaping pattern atoms,
    // so `shots\*.png` must behave like `shots/*.png`.
    let glob_pattern = if cfg!(windows) {
        Cow::Owned(pattern.replace('\\', "/"))
    } else {
        Cow::Borrowed(pattern)
    };
    // Shell conventions on every platform: `*` never crosses `/` nor
    // matches a leading dot; `**` stays the explicit way to descend.
    // Case-sensitive even on Windows, like fnmatch, so `*.PNG` and
    // `*.png` stay distinct patterns.
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let paths = glob::glob_with(&glob_pattern, options)
        .map_err(|e| anyhow::anyhow!("invalid glob pattern '{pattern}': {e}"))?;
    // glob 0.3 unwraps `to_str()` on scanned directory entries while
    // filtering leading dots, so a non-UTF-8 filename (legacy zip
    // extraction, GBK names) would panic the whole run. Expansion reads
    // no shared state, so the iteration is contained and turned into
    // this module's kind of error instead of a crash.
    let walked = std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in paths {
            let path = entry.with_context(|| format!("cannot expand '{pattern}'"))?;
            if path.is_dir() {
                bail!(
                    "'{}' is a directory; use '**' in the pattern to descend \
                     (e.g. '{}/**/*')",
                    path.display(),
                    pattern
                );
            }
            files.push(path);
            if files.len() > cap {
                bail!("'{pattern}' matches more than {cap} files; narrow the pattern");
            }
        }
        Ok(files)
    }));
    let mut files = match walked {
        Ok(files) => files?,
        Err(_) => bail!(
            "'{pattern}' hit a filename that is not valid UTF-8 while expanding; \
             rename it or pass the file directly"
        ),
    };
    if files.is_empty() {
        bail!("no files match '{pattern}'");
    }
    files.sort();
    Ok(files)
}

/// Classify raw bytes by content, keeping the original bytes and MIME.
fn classify(origin: &str, bytes: Vec<u8>, source: InputSource, id: usize) -> Result<InputPart> {
    let name = match &source {
        InputSource::File(p) => p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(origin)
            .to_string(),
        _ => origin.to_string(),
    };
    if bytes.is_empty() {
        bail!("'{origin}' is empty; nothing to send");
    }
    if let Some(mime) = image_mime(&bytes) {
        return Ok(InputPart {
            id,
            source,
            name,
            kind: MediaKind::Image,
            unknown_kind: false,
            mime: mime.into(),
            content: InputContent::Media(bytes),
        });
    }
    if let Some((mime, _)) = audio_type(&bytes) {
        return Ok(InputPart {
            id,
            source,
            name,
            kind: MediaKind::Audio,
            unknown_kind: false,
            mime: mime.into(),
            content: InputContent::Media(bytes),
        });
    }
    match String::from_utf8(bytes) {
        Ok(text) => {
            if text.trim().is_empty() {
                bail!("'{origin}' is empty or whitespace-only; nothing to send");
            }
            Ok(InputPart {
                id,
                source,
                name,
                kind: MediaKind::Text,
                unknown_kind: false,
                mime: "text/plain".into(),
                content: InputContent::Text(text),
            })
        }
        Err(_) => bail!("'{origin}' is neither valid UTF-8 text nor a supported image/audio file"),
    }
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn audio_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some(("audio/wav", "wav"))
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        Some(("audio/mpeg", "mp3"))
    } else if bytes.starts_with(b"fLaC") {
        Some(("audio/flac", "flac"))
    } else if bytes.starts_with(b"OggS") {
        Some(("audio/ogg", "ogg"))
    } else if bytes.get(4..8) == Some(b"ftyp") {
        Some(("audio/mp4", "m4a"))
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some(("audio/webm", "webm"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardContent;
    use std::io::Cursor;

    fn env(stdin_data: &'static [u8], terminal: bool) -> InputEnv<'static> {
        // The cursor and closure are leaked here on purpose: test-scoped,
        // tiny, and it keeps every call site a one-liner. The data probe
        // mirrors what a real pipe would report: bytes pending or not.
        let has_data = !stdin_data.is_empty();
        InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(stdin_data))),
            stdin_is_terminal: terminal,
            stdin_data_probe: Box::leak(Box::new(move || has_data)),
            clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
        }
    }

    fn file(p: &str) -> SourceSpec {
        SourceSpec::File(p.into())
    }

    #[test]
    fn piped_stdin_alone_is_material() {
        let mut e = env(b"hello\n", false);
        let parts = gather(&[], true, None, false, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text(), Some("hello\n"));
        assert_eq!(parts[0].source, InputSource::Stdin);
    }

    #[test]
    fn empty_piped_stdin_is_an_error_without_clipboard_fallback() {
        let mut e = env(b"", false);
        let err = gather(&[], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("stdin is empty"));
    }

    #[test]
    fn unconsumed_pipe_with_explicit_material_is_an_error() {
        let mut e = env(b"pipe data\n", false);
        let err = gather(&[file("a.txt")], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("add `-`"));
    }

    #[test]
    fn closed_empty_stdin_with_explicit_material_is_not_an_error() {
        // CI runners, cron and `docker run` without `-t` attach a closed
        // pipe or /dev/null: not a terminal, and no bytes either. The
        // explicit material must run exactly as it would on a terminal.
        let dir = std::env::temp_dir().join(format!("aido-input-null-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.txt");
        std::fs::write(&real, b"real\n").unwrap();
        let mut e = env(b"", false);
        let parts = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A data probe that says yes regardless of the cursor: stdin state
    /// and probe answer are independent injections.
    #[test]
    fn probe_drives_the_pipe_guard_not_the_cursor() {
        let dir = std::env::temp_dir().join(format!("aido-input-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.txt");
        std::fs::write(&real, b"real\n").unwrap();
        let mut e = InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(b""))),
            stdin_is_terminal: false,
            stdin_data_probe: Box::leak(Box::new(|| true)),
            clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
        };
        let err = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("add `-`"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dash_reads_stdin_at_its_position() {
        let dir = std::env::temp_dir().join(format!("aido-input-dash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, b"from file\n").unwrap();
        let mut e = env(b"pipe\n", false);
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
        let mut e = env(b"", true);
        let parts = gather(&[], true, None, false, &mut e).unwrap();
        assert_eq!(parts[0].text(), Some("clip"));
        assert_eq!(parts[0].source, InputSource::Clipboard);
    }

    #[test]
    fn no_material_task_runs_on_instruction_alone() {
        let mut e = env(b"", true);
        let parts = gather(&[], false, None, false, &mut e).unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn empty_file_is_an_error_not_a_skip() {
        let dir = std::env::temp_dir().join(format!("aido-input-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
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
        let dir = std::env::temp_dir().join(format!("aido-input-jpg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.jpg");
        std::fs::write(&path, &jpg).unwrap();
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::File(path)], true, None, false, &mut e).unwrap();
        assert_eq!(parts[0].kind, MediaKind::Image);
        assert_eq!(parts[0].mime, "image/jpeg");
        assert_eq!(parts[0].content, InputContent::Media(jpg));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_stdin_and_paste_rejected() {
        let mut e = env(b"x", false);
        let err = gather(
            &[SourceSpec::Stdin, SourceSpec::Stdin],
            true,
            None,
            false,
            &mut e,
        );
        assert!(err.is_err());
        let mut e = env(b"", true);
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
        let mut e = env(b"pipe\n", false);
        let err = gather(&[SourceSpec::Paste], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("add `-`"));
    }

    #[test]
    fn dry_run_never_touches_the_clipboard() {
        // The closure would panic if called; the placeholder keeps the
        // plan checkable without desktop clipboard state.
        let mut e = InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(b""))),
            stdin_is_terminal: true,
            stdin_data_probe: Box::leak(Box::new(|| false)),
            clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
        };
        let parts = gather(&[SourceSpec::Paste], true, None, true, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].source, InputSource::Clipboard);
        assert!(parts[0].name.contains("dry-run"), "{}", parts[0].name);

        let mut e = InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(b""))),
            stdin_is_terminal: true,
            stdin_data_probe: Box::leak(Box::new(|| false)),
            clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
        };
        let parts = gather(&[], true, None, true, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].source, InputSource::Clipboard);
    }

    /// A fresh temp dir with the given relative entries (parents created
    /// as needed); leftovers from a crashed earlier run are cleared.
    fn write_dir(name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aido-input-{name}-{}", std::process::id()));
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
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("is a directory"), "{err}");
        let pattern = format!("{}/*.txt", dir.display());
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap();
        let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["top.txt"]);
    }

    #[test]
    fn literal_file_with_metachars_wins_over_pattern() {
        let dir = write_dir("glob-literal", &[("note[1].txt", b"literal\n")]);
        let pattern = dir.join("note[1].txt").display().to_string();
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::File(dir.clone())], true, None, false, &mut e).unwrap();
        let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        assert_eq!(parts[0].source, InputSource::File(dir.join("a.txt")));
    }

    #[test]
    fn directory_with_subdirectory_is_an_error_with_a_glob_hint() {
        let dir = write_dir("dir-sub", &[("a.txt", b"a\n")]);
        std::fs::create_dir(dir.join("raw")).unwrap();
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("expands one level"), "{msg}");
        assert!(msg.contains("glob"), "{msg}");
    }

    #[test]
    fn directory_without_usable_files_is_an_error() {
        let dir = write_dir("dir-empty", &[]);
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::File(dir.clone())], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("no input files"), "{err}");
        std::fs::write(dir.join(".dot.txt"), b"d\n").unwrap();
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("no input files"), "{err}");
    }

    #[test]
    fn directory_and_dash_keep_argv_order() {
        let dir = write_dir("dir-dash", &[("a.txt", b"a\n"), ("b.txt", b"b\n")]);
        let mut e = env(b"pipe\n", false);
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
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::Glob(pattern)], true, None, true, &mut e).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text(), Some("a\n"));
    }

    #[test]
    fn missing_file_spec_still_errors_normally() {
        let mut e = env(b"", true);
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
        let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        // `**` matches zero directories too, and skips dotfiles on the way.
        assert_eq!(
            names,
            vec![
                format!("{}/sub/nested.txt", dir.display()),
                format!("{}/top.txt", dir.display()),
            ]
        );
    }

    #[test]
    fn invalid_patterns_error_cleanly() {
        let err = expand_glob("a**b", MAX_EXPANSION).unwrap_err();
        assert!(err.to_string().contains("invalid glob pattern"), "{err}");
        let err = expand_glob("[b", MAX_EXPANSION).unwrap_err();
        assert!(err.to_string().contains("invalid glob pattern"), "{err}");
    }

    #[test]
    fn empty_file_inside_expansion_is_an_error_at_its_position() {
        let dir = write_dir("expansion-empty", &[("a.txt", b"a\n"), ("empty.txt", b"")]);
        let pattern = format!("{}/*.txt", dir.display());
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
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
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::Glob(pattern)], true, None, false, &mut e).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid UTF-8"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_inside_a_directory_arg_is_refused() {
        let dir = write_dir("dir-symlink", &[("a.txt", b"a\n")]);
        std::os::unix::fs::symlink(dir.join(".."), dir.join("up")).unwrap();
        let mut e = env(b"", true);
        let err = gather(&[SourceSpec::File(dir)], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("expands one level"), "{err}");
    }
}
