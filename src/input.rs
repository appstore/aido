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
//! material. A writer that is attached but still silent at probe time
//! (`curl … | aido file`) reads the same way: the probe is a snapshot and
//! cannot predict a silent writer, so the explicit material runs and the
//! pipe is never drained. Only stdin that already holds bytes demands a `-`.
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
use std::io::Read;
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
            stdin: Box::leak(Box::new(std::io::stdin())),
            stdin_data_probe: Box::leak(Box::new(fd0_has_unread_bytes)),
            clipboard: Box::leak(Box::new(crate::clipboard::read)),
        }
    }
}

impl<'a> InputEnv<'a> {
    pub fn custom(
        stdin: &'a mut dyn Read,
        stdin_data_probe: &'a mut (dyn FnMut() -> bool + 'a),
        clipboard: &'a mut dyn FnMut() -> Result<crate::clipboard::ClipboardContent>,
    ) -> Self {
        Self {
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
/// behavior instead of silently ignoring possible input. One fstat
/// failure is not uncertainty but a verdict: `EBADF` means fd 0 is
/// closed (`0<&-`, a daemonized process), and a closed fd carries no
/// unread bytes.
#[cfg(unix)]
fn fd0_has_unread_bytes() -> bool {
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(libc::STDIN_FILENO, &mut stat) != 0 {
            return !matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFCHR => false,
            libc::S_IFREG => {
                let offset = libc::lseek(libc::STDIN_FILENO, 0, libc::SEEK_CUR);
                stat.st_size > offset
            }
            _ => {
                let mut pending: libc::c_int = 0;
                // FIONREAD is typed as the platform's ioctl request type in
                // libc (c_ulong under glibc, c_int under musl): no cast, or
                // the musl build breaks.
                if libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut pending) == 0 {
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
/// decides whether a task with no specs and no stdin data may run on the
/// instruction alone. Under `dry_run` the clipboard is never touched:
/// paste slots become clearly-labeled placeholders so a plan can be
/// checked without reading (or depending on) desktop state.
pub fn gather(
    specs: &[SourceSpec],
    requires_material: bool,
    total_limit: Option<u64>,
    dry_run: bool,
    env: &mut InputEnv<'_>,
) -> Result<Vec<InputPart>> {
    let mut notes = Vec::new();
    gather_with_notes(
        specs,
        requires_material,
        total_limit,
        dry_run,
        env,
        &mut notes,
    )
}

/// [`gather`] plus a sink for the materializers' notes about partial
/// material (a page that yielded nothing, an image in a codec aido
/// cannot carry): the caller decides whether and how to surface them.
pub fn gather_with_notes(
    specs: &[SourceSpec],
    requires_material: bool,
    total_limit: Option<u64>,
    dry_run: bool,
    env: &mut InputEnv<'_>,
    notes: &mut Vec<String>,
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
        // The probe, not tty-ness: a closed pipe or /dev/null carries no
        // data, so automation keeps the clipboard (or instruction-only)
        // row of the decision table instead of a spurious "stdin is empty".
        if env.stdin_has_data() {
            let bytes = read_limited(env.stdin, MAX_PART_BYTES, "stdin")?;
            if bytes.is_empty() {
                bail!("stdin is empty; nothing to send (the clipboard is never a fallback)");
            }
            // Materialization makes the expanded output bigger than the
            // bytes read, so this path charges the same budget the loop
            // below charges: piped material is never exempt from the run's
            // total limit.
            let mut parts = Vec::new();
            let mut total = 0u64;
            for part in make_parts("stdin", bytes, InputSource::Stdin, 0, 0, notes)? {
                push_part(&mut parts, &mut total, limit, part)?;
            }
            return Ok(parts);
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
    // One sequence number per materialized document, so two specs of the
    // same file (`ocr book.pdf book.pdf`) keep separate per-part units
    // instead of silently merging into one.
    let mut document = 0usize;
    for spec in specs {
        match spec {
            SourceSpec::File(path) => {
                for path in expand_file_spec(path, MAX_EXPANSION)? {
                    push_material(&mut parts, &mut total, limit, &path, document, notes)?;
                    document += 1;
                }
            }
            SourceSpec::Glob(pattern) => {
                for path in expand_glob(pattern, MAX_EXPANSION)? {
                    push_material(&mut parts, &mut total, limit, &path, document, notes)?;
                    document += 1;
                }
            }
            SourceSpec::Stdin => {
                let bytes = read_limited(env.read_stdin(), MAX_PART_BYTES, "stdin")?;
                if bytes.is_empty() {
                    bail!("stdin is empty; nothing to send");
                }
                for part in make_parts(
                    "stdin",
                    bytes,
                    InputSource::Stdin,
                    document,
                    parts.len(),
                    notes,
                )? {
                    push_part(&mut parts, &mut total, limit, part)?;
                }
                document += 1;
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
                    unit: None,
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

/// Bytes become parts: recognized document containers materialize into
/// several text/image parts ([`materialize::expand`]); everything else
/// classifies into one part, exactly as before materialization existed.
/// The materializer's notes about partial material are appended to the
/// run's sink, in gather order.
///
/// [`materialize::expand`]: crate::materialize::expand
fn make_parts(
    origin: &str,
    bytes: Vec<u8>,
    source: InputSource,
    document: usize,
    start_id: usize,
    notes: &mut Vec<String>,
) -> Result<Vec<InputPart>> {
    match crate::materialize::expand(origin, &bytes, &source, document, start_id)? {
        Some((parts, doc_notes)) => {
            notes.extend(doc_notes);
            Ok(parts)
        }
        None => Ok(vec![classify(origin, bytes, source, start_id)?]),
    }
}

/// Read one file fully and append its part.
fn push_material(
    parts: &mut Vec<InputPart>,
    total: &mut u64,
    limit: u64,
    path: &Path,
    document: usize,
    notes: &mut Vec<String>,
) -> Result<()> {
    let bytes = read_file(path, MAX_PART_BYTES, limit.saturating_sub(*total))?;
    let origin = path.display().to_string();
    for part in make_parts(
        &origin,
        bytes,
        InputSource::File(path.to_path_buf()),
        document,
        parts.len(),
        notes,
    )? {
        push_part(parts, total, limit, part)?;
    }
    Ok(())
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
        unit: None,
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
                unit: None,
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
                unit: None,
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
    // The metadata checks above only carry the friendlier messages: a file
    // grown between the stat and the read, or a special file reporting
    // len 0 (such as a FIFO or /proc/self/status), is still bounded below.
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    let bytes =
        read_take(&mut file, max).with_context(|| format!("cannot read '{}'", path.display()))?;
    if bytes.len() as u64 > max {
        bail!(
            "'{}' is {} MB; refusing input files over {} MB (they are fully loaded into memory)",
            path.display(),
            bytes.len() / (1024 * 1024),
            max / (1024 * 1024)
        );
    }
    if bytes.len() as u64 > remaining {
        bail!("inputs exceed the total input limit");
    }
    Ok(bytes)
}

/// Read at most `max + 1` bytes, so an over-budget reader is detected by
/// length instead of by trusting any size metadata.
fn read_take(reader: &mut dyn Read, max: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.take(max + 1).read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_limited(reader: &mut dyn Read, max: u64, origin: &str) -> Result<Vec<u8>> {
    let buf = read_take(reader, max).with_context(|| format!("failed to read {origin}"))?;
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
    // The literal file already got its chance above, so an unparseable
    // pattern is usually a typo'd filename: lead with the shell-equivalent
    // "no such file" fact, keeping the syntax detail as secondary context.
    let paths = glob::glob_with(&glob_pattern, options).map_err(|e| {
        anyhow::anyhow!("no files match '{pattern}' (it is also not a valid glob pattern: {e})")
    })?;
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
            unit: None,
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
            unit: None,
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
                unit: None,
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
        || (bytes.len() >= 2
            && bytes[0] == 0xff
            && bytes[1] & 0xe0 == 0xe0
            && bytes[1] & 0x06 != 0
            && bytes[1] & 0x18 != 0x08)
    {
        Some(("audio/mpeg", "mp3"))
    } else if bytes.starts_with(b"fLaC") {
        Some(("audio/flac", "flac"))
    } else if bytes.starts_with(b"OggS") {
        Some(("audio/ogg", "ogg"))
    } else if bytes.get(4..8) == Some(b"ftyp") {
        Some(("audio/mp4", "m4a"))
    } else {
        ebml_flavor(bytes)
    }
}

/// EBML magic (`1A 45 DF A3`) plus the DocType element, matched as an
/// anchored whole: element ID 0x4282, then a size vint, then the string.
/// MKV and WebM share the magic; only the DocType tells them apart.
/// Matroska's DocType is "matroska" (8 bytes → vint 0x88), WebM's is
/// "webm" (4 bytes → vint 0x84).
fn ebml_flavor(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if !bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return None;
    }
    let head = &bytes[..bytes.len().min(128)];
    if head
        .windows(7)
        .any(|w| w == [0x42, 0x82, 0x84, b'w', b'e', b'b', b'm'])
    {
        Some(("audio/webm", "webm"))
    } else if head.windows(11).any(|w| {
        w == [
            0x42, 0x82, 0x88, b'm', b'a', b't', b'r', b'o', b's', b'k', b'a',
        ]
    }) {
        Some(("audio/x-matroska", "mkv"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
