use regex::Regex;
use std::sync::OnceLock;

/// Best-effort markdown cleanup for plain-text surfaces (the Quicker
/// popup being the motivating one): keep the words, drop the markup —
/// bold, headings, fenced-block markers, inline backticks, quote marks,
/// bullet markers, links, table pipes, horizontal rules. Best-effort
/// means common constructs are handled and anything ambiguous is left
/// alone rather than mangled: single `*` / `_` italics and intraword
/// `__` stay put so `2*3*4` and `__init__` survive, and text inside
/// code fences and inline code spans is never rewritten.
pub fn strip(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    // The fence character is tracked so a ~~~ line inside a ``` block
    // stays content instead of closing it.
    let mut fence: Option<char> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match fence {
            Some(mark) => {
                if is_fence_close(trimmed, mark) {
                    fence = None;
                } else {
                    // Inside a fence the text is code: keep every byte.
                    out.push(line.to_string());
                }
            }
            None => {
                if let Some(mark) = fence_open(trimmed) {
                    fence = Some(mark);
                } else if let Some(cleaned) = strip_line(line) {
                    out.push(cleaned);
                }
            }
        }
    }
    out.join("\n")
}

/// A line that opens a fenced block: three or more backticks or tildes
/// (an info string after the marks is fine).
fn fence_open(trimmed: &str) -> Option<char> {
    for mark in ['`', '~'] {
        let mut n = 0usize;
        for c in trimmed.chars() {
            if c == mark {
                n += 1;
            } else {
                break;
            }
        }
        if n >= 3 {
            return Some(mark);
        }
    }
    None
}

fn is_fence_close(trimmed: &str, mark: char) -> bool {
    let t = trimmed.trim_end();
    t.chars().count() >= 3 && t.chars().all(|c| c == mark)
}

/// Returns `None` for lines that disappear entirely (horizontal rules,
/// table separator rows), the cleaned line otherwise.
fn strip_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    // Blockquote markers, outermost first; each ">" eats one space.
    let mut s = trimmed;
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.strip_prefix(' ').unwrap_or(rest);
    }

    // ATX headings: up to six "#" followed by a space (or end of line);
    // "#hashtag" is not a heading.
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &s[hashes..];
        if rest.is_empty() || rest.starts_with([' ', '\t']) {
            s = rest.trim_start_matches([' ', '\t']);
        }
    }

    // Inline code spans move into placeholders first so their content
    // survives the table and emphasis passes below untouched.
    let mut spans = Vec::new();
    let mut s = extract_code_spans(s, &mut spans);

    // Thematic breaks (---, ***, ___, ===) vanish with their line.
    let t = s.trim_end();
    if let Some(first) = t.chars().next() {
        if matches!(first, '-' | '*' | '_' | '=') {
            let body: String = t.chars().filter(|c| *c != ' ' && *c != '\t').collect();
            if body.len() >= 3 && body.chars().all(|c| c == first) {
                return None;
            }
        }
    }

    // Tables: separator rows vanish; the rest keeps its cell text with
    // pipe separators turned into tabs.
    if s.contains('|') {
        if is_table_delimiter_row(&s) {
            return None;
        }
        let cells: Vec<String> = s
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect();
        s = cells.join("\t");
    }

    // Bullet markers go; the indentation stays so nesting still reads.
    // Ordered "1." markers are plain text already and are kept.
    for mark in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(mark) {
            s = rest.to_string();
            break;
        }
    }

    let p = patterns();
    let s = p.image.replace_all(&s, "$1");
    let s = p.link.replace_all(&s, "$1");
    let s = p.bare_link.replace_all(&s, "$1");
    let s = p.bold_italic.replace_all(&s, "$1");
    let s = p.bold.replace_all(&s, "$1");
    let s = p.strike.replace_all(&s, "$1");

    // Put the code spans back exactly as they were.
    let mut out = s.into_owned();
    for (i, span) in spans.iter().enumerate() {
        out = out.replace(&placeholder(i), span);
    }
    Some(format!("{indent}{out}"))
}

fn is_table_delimiter_row(s: &str) -> bool {
    let body: String = s
        .chars()
        .filter(|c| !matches!(c, '|' | ':' | '-' | ' ' | '\t'))
        .collect();
    body.is_empty() && s.contains('|') && s.contains('-')
}

/// Swap `` `code` `` spans for placeholders; an unbalanced backtick is
/// kept literally.
fn extract_code_spans(s: &str, spans: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                spans.push(after[..close].to_string());
                out.push_str(&placeholder(spans.len() - 1));
                rest = &after[close + 1..];
            }
            None => {
                out.push('`');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// NUL-framed indices: never produced by a model reply, invisible to all
/// the passes in between, and always replaced before anything is printed.
fn placeholder(i: usize) -> String {
    format!("\u{0}{i}\u{0}")
}

struct Patterns {
    image: Regex,       // ![alt](url) -> alt
    link: Regex,        // [text](url) -> text
    bare_link: Regex,   // [](url) -> url
    bold_italic: Regex, // ***x*** -> x
    bold: Regex,        // **x** -> x
    strike: Regex,      // ~~x~~ -> x
}

static PATTERNS: OnceLock<Patterns> = OnceLock::new();

fn patterns() -> &'static Patterns {
    PATTERNS.get_or_init(|| Patterns {
        image: Regex::new(r"!\[([^\]]*)\]\([^)]*\)").unwrap(),
        link: Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap(),
        bare_link: Regex::new(r"\[\]\(([^)]*)\)").unwrap(),
        bold_italic: Regex::new(r"\*\*\*(.+?)\*\*\*").unwrap(),
        bold: Regex::new(r"\*\*(.+?)\*\*").unwrap(),
        strike: Regex::new(r"~~(.+?)~~").unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::strip;

    #[test]
    fn keeps_plain_text_untouched() {
        assert_eq!(strip("just words"), "just words");
        // no single-* italics, no intraword __: math and dunders survive
        assert_eq!(
            strip("2*3*4=12 and __init__(self)"),
            "2*3*4=12 and __init__(self)"
        );
        assert_eq!(strip("snake_case_name"), "snake_case_name");
        assert_eq!(strip(""), "");
    }

    #[test]
    fn strips_bold_strikethrough_and_cjk() {
        assert_eq!(strip("**bold** tail"), "bold tail");
        assert_eq!(strip("***both***"), "both");
        assert_eq!(strip("~~gone~~"), "gone");
        assert_eq!(strip("中文**加粗**测试"), "中文加粗测试");
    }

    #[test]
    fn strips_headings_quotes_and_bullet_markers() {
        assert_eq!(strip("## Title"), "Title");
        assert_eq!(strip("# Heading"), "Heading");
        assert_eq!(strip("#hashtag stays"), "#hashtag stays");
        assert_eq!(strip("> quoted"), "quoted");
        assert_eq!(strip("> > deep"), "deep");
        assert_eq!(strip("- item"), "item");
        assert_eq!(strip("* item"), "item");
        assert_eq!(strip("  - nested"), "  nested");
        // ordered markers are plain text already
        assert_eq!(strip("1. first"), "1. first");
    }

    #[test]
    fn fence_content_is_kept_verbatim() {
        let md = "before\n```python\nprint('a**b')\n# not a heading\n```\nafter";
        assert_eq!(strip(md), "before\nprint('a**b')\n# not a heading\nafter");
        let md = "```\nx\n~~~\ny\n```";
        assert_eq!(strip(md), "x\n~~~\ny");
    }

    #[test]
    fn inline_code_is_protected_from_other_rules() {
        assert_eq!(strip("run `x ** y` now"), "run x ** y now");
        assert_eq!(strip("dunder `__init__` kept"), "dunder __init__ kept");
        assert_eq!(strip("bold around code **`x`**"), "bold around code x");
    }

    #[test]
    fn links_become_their_text() {
        assert_eq!(strip("see [docs](https://example.com) now"), "see docs now");
        assert_eq!(strip("pic ![alt](https://x.com/i.png) end"), "pic alt end");
        assert_eq!(
            strip("bare [](https://example.com)"),
            "bare https://example.com"
        );
    }

    #[test]
    fn tables_flatten_and_separator_rows_vanish() {
        let md = "| a | b |\n|---|:---:|\n| 1 | 2 |";
        assert_eq!(strip(md), "a\tb\n1\t2");
        // pipes inside inline code are not cell separators
        assert_eq!(strip("try `ls | wc -l` ok"), "try ls | wc -l ok");
    }

    #[test]
    fn horizontal_rules_disappear() {
        assert_eq!(strip("above\n---\nbelow"), "above\nbelow");
        assert_eq!(strip("***"), "");
        assert_eq!(strip("___"), "");
        // a list item is not a rule
        assert_eq!(strip("- item"), "item");
    }
}
