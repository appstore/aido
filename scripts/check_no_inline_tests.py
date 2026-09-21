#!/usr/bin/env python3
"""Reject #[cfg(test)]-gated inline module bodies under src/ (AGENTS.md).

The unit-test placement convention requires `#[cfg(test)] mod tests;`
declarations with bodies in separate src/<module>/tests.rs files. This
checker is token-aware rather than line-based: from each `#[cfg(test)]`
attribute it skips whitespace, comments and further attributes before
deciding, so formatting cannot hide an inline `mod name {` body. String
and raw-string literals are skipped by the lexer, so gate mentions
inside them are ignored.

Usage:
    python3 scripts/check_no_inline_tests.py [paths...]   # default: src
    python3 scripts/check_no_inline_tests.py --self-test

Designed for the `No embedded test modules` step in
.github/workflows/test.yml (python3 is preinstalled on the runner).
"""

import sys
from pathlib import Path

GATES = ("#[cfg(test)]", "#![cfg(test)]")
WHITESPACE = " \t\r\n\f\v"


def skip_block_comment(text, i):
    """text[i:i+2] == '/*'; return index after the nested comment."""
    depth = 0
    n = len(text)
    while i < n - 1:
        if text.startswith("/*", i):
            depth += 1
            i += 2
        elif text.startswith("*/", i):
            depth -= 1
            i += 2
            if depth == 0:
                return i
        else:
            i += 1
    return n


def skip_string(text, i, raw_hashes):
    """text[i] == '\"'; return index after the closing quote.

    raw_hashes >= 0 means a raw string terminated by '\"' + that many '#'.
    """
    n = len(text)
    i += 1
    while i < n:
        c = text[i]
        if raw_hashes >= 0:
            if c == '"':
                k = i + 1
                while k < n and text[k] == "#":
                    k += 1
                if k - i - 1 == raw_hashes:
                    return k
                i = k
            else:
                i += 1
        elif c == "\\":
            i += 2
        elif c == '"':
            return i + 1
        else:
            i += 1
    return n


def skip_balanced_attribute(text, i):
    """text[i] == '#' starting an attribute; return index after its ']'."""
    n = len(text)
    j = i + 1
    if j < n and text[j] == "!":
        j += 1
    if j >= n or text[j] != "[":
        return i + 1  # not an attribute after all; let the scanner move on
    depth = 0
    while j < n:
        c = text[j]
        if c == '"':
            k = j + 1
            while k < n and text[k] == "#":
                k += 1
            hashes = k - j - 1 if k < n and text[k] == '"' else -1
            j = skip_string(text, j, hashes)
            continue
        if text.startswith("//", j):
            j = text.find("\n", j)
            j = n if j < 0 else j
            continue
        if text.startswith("/*", j):
            j = skip_block_comment(text, j)
            continue
        if c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    return n


def read_word(text, i):
    j = i
    n = len(text)
    while j < n and (text[j].isalnum() or text[j] == "_"):
        j += 1
    return text[i:j], j


def find_inline_test_modules(text):
    """Return 1-based line numbers of inline cfg(test)-gated module bodies."""
    violations = []
    line = 1
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            j = skip_block_comment(text, i)
            line += text.count("\n", i, j)
            i = j
        elif c == '"':
            k = i + 1
            while k < n and text[k] == "#":
                k += 1
            hashes = k - i - 1 if k < n and text[k] == '"' else -1
            j = skip_string(text, i, hashes)
            line += text.count("\n", i, j)
            i = j
        elif c == "r" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            k = i + 1
            while k < n and text[k] == "#":
                k += 1
            if k < n and text[k] == '"':
                j = skip_string(text, k, k - i - 1)
                line += text.count("\n", i, j)
                i = j
            else:
                i += 1
        elif c == "'":
            if i + 1 < n and text[i + 1] == "\\":
                i += 3
            elif i + 2 < n and text[i + 2] == "'" and text[i + 1] != "\n":
                i += 3
            else:  # lifetime
                i += 1
                while i < n and (text[i].isalnum() or text[i] == "_"):
                    i += 1
        elif c == "#" and text.startswith(GATES, i):
            gate_line = line
            gate = "#![cfg(test)]" if text.startswith(GATES[1], i) else GATES[0]
            j, violation, mod_name = skip_post_gate(text, i + len(gate))
            line += text.count("\n", i, j)
            if violation:
                violations.append((gate_line, mod_name))
            i = j
        else:
            i += 1
    return violations


def skip_post_gate(text, i):
    """Skip whitespace/comments/attributes after a gate attribute.

    Returns (next_index, violation, module_name): violation is True when
    an inline `mod name {` follows the gate; False when the module is an
    external declaration (`mod name;`) or the gate belongs to some other
    item, which the placement convention does not cover.
    """
    n = len(text)
    while i < n:
        c = text[i]
        if c in WHITESPACE:
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            i = skip_block_comment(text, i)
        elif c == "#":
            j = skip_balanced_attribute(text, i)
            if j <= i:
                return i + 1, False, ""
            i = j
        elif c.isalpha() or c == "_":
            word, j = read_word(text, i)
            if word != "mod":
                return i, False, ""  # gate on a non-module item; resume scan
            while j < n and text[j] in WHITESPACE:
                j += 1
            name, j = read_word(text, j)
            while j < n and text[j] in WHITESPACE:
                j += 1
            if j < n and text[j] == "{":
                return j, True, name
            return j, False, ""  # `mod name;` or something unparseable
        else:
            return i, False, ""
    return i, False, ""


SELF_TEST_CASES = [
    ("external declaration passes", "#[cfg(test)]\nmod tests;\n", False),
    ("inline body on the same line fails", "#[cfg(test)] mod tests {\n}\n", True),
    ("inner attribute gate fails", "#![cfg(test)]\nmod tests {\n}\n", True),
    ("inline body fails", "#[cfg(test)]\nmod tests {\n}\n", True),
    ("attribute between gate and body fails",
     "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n}\n", True),
    ("blank line and comment before body fail",
     "#[cfg(test)]\n\n// why not\nmod tests {\n}\n", True),
    ("block comment between gate and body fails",
     "#[cfg(test)]\n/* inline */ mod tests {\n}\n", True),
    ("gate inside comment is ignored", "/* #[cfg(test)] */ mod x { }\n", False),
    ("gate inside string is ignored",
     'const S: &str = "#[cfg(test)] mod x { }";\n', False),
    ("gate inside raw string is ignored",
     'const S: &str = r#"#[cfg(test)] mod x {"#;\n', False),
    ("gate on a function is out of scope", "#[cfg(test)]\nfn helper() {}\n", False),
    ("ungated module is out of scope", "#[allow(dead_code)]\nmod internal { }\n", False),
    ("external declaration after comment passes",
     "#[cfg(test)] // gated\nmod tests;\n", False),
    ("raw string in attribute does not confuse the scanner",
     '#[doc = r#"mod x {"#]\n#[cfg(test)]\nmod tests;\n', False),
]


def self_test():
    failures = 0
    for name, source, expect_violation in SELF_TEST_CASES:
        found = find_inline_test_modules(source)
        if bool(found) != expect_violation:
            print(f"self-test FAIL: {name!r}: expected "
                  f"{'violation' if expect_violation else 'clean'}, got {found}")
            failures += 1
    print(f"self-test: {len(SELF_TEST_CASES) - failures}/{len(SELF_TEST_CASES)} passed")
    return 1 if failures else 0


def main(argv):
    if "--self-test" in argv:
        return self_test()
    paths = argv[1:] or ["src"]
    violations = 0
    for raw in paths:
        for path in sorted(Path(raw).rglob("*.rs")):
            # Rust sources are UTF-8 by definition; don't inherit the
            # runner's locale encoding.
            for line, name in find_inline_test_modules(path.read_text(encoding="utf-8")):
                print(f"::error file={path},line={line}::embedded #[cfg(test)] module body "
                      f"`mod {name}` — move it to a separate tests.rs file (AGENTS.md)")
                violations += 1
    if violations:
        print(f"{violations} embedded test module(s) found")
        return 1
    print("no embedded test modules")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
