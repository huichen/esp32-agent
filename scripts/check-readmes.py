#!/usr/bin/env python3
"""Offline README translation guard; Python 3.10+, standard library only.

Checks the repository's plain Markdown (ATX/setext headings, fenced code, pipe
tables, inline/reference links and images). This is not a general CommonMark
renderer or a semantic translation checker. Digit sequences (including versions
and inline configuration) are compared per level-2 section; decimal commas are
normalized, spelled-out numbers are not.
"""

import argparse
from collections import Counter
from dataclasses import dataclass
import html
from pathlib import Path
import re
import string
import sys
import unicodedata
from urllib.parse import unquote, urlsplit


LANGUAGES = (
    ("English", "README.md"),
    ("简体中文", "README.zh.md"),
    ("Français", "README.fr.md"),
    ("Español", "README.es.md"),
    ("Русский", "README.ru.md"),
    ("العربية", "README.ar.md"),
    ("日本語", "README.ja.md"),
    ("한국어", "README.ko.md"),
)
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})([^\n]*)$")
ATX = re.compile(r"^ {0,3}(#{1,6})(?:[ \t]+|$)(.*)$")
SETEXT = re.compile(r"^ {0,3}(=+|-+)[ \t]*$")
REFERENCE = re.compile(
    r"^ {0,3}\[([^\]\n]+)\]:[ \t]*(<[^>\n]*>|\S+)"
    r"(?:[ \t]+(?:\"[^\"]*\"|'[^']*'|\([^)]*\)))?[ \t]*$", re.MULTILINE
)
NUMBER = re.compile(r"[0-9]+(?:[.,][0-9]+)*")
HTML_ANCHOR = re.compile(r"<a\b[^>]*\b(?:name|id)\s*=", re.IGNORECASE)


def switcher(filename):
    return " · ".join(
        f"**{label}**" if target == filename else f"[{label}]({target})"
        for label, target in LANGUAGES
    )


def blank(text):
    return re.sub(r"[^\n]", " ", text)


def fenced(text):
    """Keep offsets/newlines while masking code; compare complete raw blocks."""
    masked, blocks, current = [], [], []
    marker = ""
    for line in text.splitlines(keepends=True):
        raw = line.rstrip("\n")
        if marker:
            current.append(line)
            masked.append(blank(line))
            if re.fullmatch(r" {0,3}" + re.escape(marker[0]) +
                            "{" + str(len(marker)) + r",}[ \t]*", raw):
                blocks.append("".join(current))
                marker, current = "", []
        else:
            match = FENCE.match(raw)
            if match:
                marker, current = match[1], [line]
                masked.append(blank(line))
            else:
                masked.append(line)
    return "".join(masked), blocks, bool(marker)


def code_spans(text):
    opening = None
    for match in re.finditer(r"`+", text):
        if opening is None:
            opening = match
        elif match[0] == opening[0]:
            yield opening.start(), match.end(), len(opening[0])
            opening = None


def without_code(text):
    result = list(text)
    for start, end, _ in code_spans(text):
        result[start:end] = blank(text[start:end])
    return "".join(result)


def escaped(text, position):
    count = 0
    while position > 0 and text[position - 1] == "\\":
        position -= 1
        count += 1
    return count % 2 != 0


def unescape(text):
    return re.sub(r"\\(.)", lambda m: m[1] if m[1] in string.punctuation else m[0], text)


def closing(text, start, left="[", right="]"):
    depth = 0
    for position in range(start, len(text)):
        if escaped(text, position):
            continue
        if text[position] == left:
            depth += 1
        elif text[position] == right:
            depth -= 1
            if depth == 0:
                return position
    return None


def destination(text, start):
    """Parse an inline destination, balanced/escaped parentheses and optional title."""
    position = start + 1
    while position < len(text) and text[position].isspace():
        position += 1
    begin = position
    if position < len(text) and text[position] == "<":
        end = text.find(">", position + 1)
        if end < 0:
            return None
        target, position = text[position + 1:end], end + 1
    else:
        depth = 0
        while position < len(text):
            char = text[position]
            if char == "\\" and position + 1 < len(text):
                position += 2
                continue
            if char == "(":
                depth += 1
            elif char == ")":
                if depth == 0:
                    break
                depth -= 1
            elif char.isspace() and depth == 0:
                break
            position += 1
        if depth:
            return None
        target = text[begin:position]
    while position < len(text) and text[position].isspace():
        position += 1
    if position < len(text) and text[position] in "\"'(":
        quote = text[position]
        end_quote = ")" if quote == "(" else quote
        position += 1
        while position < len(text) and (text[position] != end_quote or escaped(text, position)):
            position += 1
        position += 1
        while position < len(text) and text[position].isspace():
            position += 1
    if position < len(text) and text[position] == ")":
        return html.unescape(unescape(target)), position + 1
    return None


@dataclass(frozen=True)
class Link:
    start: int
    label_end: int
    end: int
    target: str


def reference_id(label):
    return " ".join(unescape(label).split()).casefold()


def reference_targets(text):
    result = {}
    for match in REFERENCE.finditer(without_code(text)):
        target = html.unescape(unescape(match[2].strip("<>")))
        result.setdefault(reference_id(match[1]), target)
    return result


def links(text, references=None):
    scan = without_code(text)
    references = dict(references or {})
    for label, target in reference_targets(text).items():
        references.setdefault(label, target)
    scan = REFERENCE.sub(lambda m: blank(m[0]), scan)
    suffixes = []
    for match in re.finditer(r"\[", scan):
        start = match.start()
        if escaped(scan, start) or any(left <= start < right for left, right in suffixes):
            continue
        end = closing(scan, start)
        if end is None:
            continue
        following = end + 1
        if following < len(scan) and scan[following] == "(":
            parsed = destination(scan, following)
            if parsed:
                suffixes.append((end + 1, parsed[1]))
                yield Link(start, end, parsed[1], parsed[0])
            continue
        while following < len(scan) and scan[following].isspace():
            following += 1
        label = scan[start + 1:end]
        link_end = end + 1
        if following < len(scan) and scan[following] == "[":
            ref_end = closing(scan, following)
            if ref_end is None:
                continue
            label = scan[following + 1:ref_end] or label
            link_end = ref_end + 1
        target = references.get(reference_id(label))
        if target is not None:
            suffixes.append((end + 1, link_end))
            yield Link(start, end, link_end, target)


def without_targets(text, preserve_offsets=True, references=None):
    spans = [(link.label_end + 1, link.end) for link in links(text, references)]
    spans.extend((match.start(), match.end()) for match in REFERENCE.finditer(text))
    result, position = [], 0
    for start, end in sorted(spans):
        if end <= position:
            continue
        start = max(start, position)
        result.append(text[position:start])
        result.append(blank(text[start:end]) if preserve_offsets else "")
        position = end
    result.append(text[position:])
    return "".join(result)


def heading_slug(title, references=None):
    title = without_targets(title, preserve_offsets=False, references=references)
    # Render code span contents literally; strip formatting only outside code.
    rendered, position = [], 0
    for start, end, width in code_spans(title):
        rendered.append(plain_heading(title[position:start]))
        code = title[start + width:end - width].replace("\n", " ")
        if code.startswith(" ") and code.endswith(" ") and code.strip():
            code = code[1:-1]
        rendered.append(code)
        position = end
    rendered.append(plain_heading(title[position:]))
    text = "".join(rendered).strip().lower()
    return "".join(
        "-" if char.isspace() else char
        for char in text
        if char.isspace() or char in "-_" or unicodedata.category(char)[0] in "LMN"
    )


def plain_heading(text):
    text = re.sub(r"</?[A-Za-z][^>]*>", "", text)
    text = re.sub(r"(?<!\w)(_+)(?=\S)(.+?)(?<=\S)\1(?!\w)", r"\2", text)
    return html.unescape(unescape(text))


def headings(text):
    found = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        match = ATX.match(line)
        if match:
            title = re.sub(r"[ \t]+#+[ \t]*$", "", match[2])
            found.append((index, len(match[1]), title))
        elif SETEXT.match(line) and index and lines[index - 1].strip():
            previous = lines[index - 1]
            if not ATX.match(previous) and not re.match(r"^( {4}|\t| {0,3}[>\-*+])", previous):
                found.append((index - 1, 1 if line.lstrip().startswith("=") else 2, previous.strip()))
    return found


def anchors(heading_list, references=None):
    used, counts = set(), Counter()
    for _, _, title in heading_list:
        base = heading_slug(title, references)
        slug = base
        while slug in used:
            counts[base] += 1
            slug = f"{base}-{counts[base]}"
        used.add(slug)
    return used


def table_cells(line):
    scan = without_code(line.strip())
    pipes = [i for i, char in enumerate(scan) if char == "|" and not escaped(scan, i)]
    if not pipes:
        return None
    edges = [-1] + pipes + [len(line.strip())]
    cells = [line.strip()[left + 1:right].strip() for left, right in zip(edges, edges[1:])]
    if pipes[0] == 0:
        cells.pop(0)
    if pipes[-1] == len(scan) - 1:
        cells.pop()
    return cells


def tables(text):
    result, index = [], 0
    lines = text.splitlines()
    while index + 1 < len(lines):
        header, separator = table_cells(lines[index]), table_cells(lines[index + 1])
        if not header or not separator or not all(re.fullmatch(r":?-{3,}:?", c) for c in separator):
            index += 1
            continue
        shape = [len(header), len(separator)]
        alignment = tuple((c.startswith(":"), c.endswith(":")) for c in separator)
        index += 2
        while index < len(lines):
            cells = table_cells(lines[index])
            if cells is None or not lines[index].strip():
                break
            shape.append(len(cells))
            index += 1
        result.append((tuple(shape), alignment))
    return result


class Document:
    def __init__(self, text):
        self.text = text
        self.prose, self.blocks, self.unclosed = fenced(text)
        self.headings = headings(self.prose)
        self.h2_count = sum(level == 2 for _, level, _ in self.headings)
        self.anchors = anchors(self.headings, reference_targets(self.prose))
        self.tables = tables(self.prose)
        numbers = without_targets(self.prose).splitlines()
        starts = {index for index, level, _ in self.headings if level == 2}
        self.numbers = [Counter()]
        for index, line in enumerate(numbers):
            if index in starts:
                self.numbers.append(Counter())
            self.numbers[-1].update(value.replace(",", ".") for value in NUMBER.findall(line))


class Validator:
    def __init__(self, root, fixtures=None):
        self.root = root.resolve()
        self.fixtures = None if fixtures is None else {(self.root / p).resolve(): t for p, t in fixtures.items()}
        self.documents = {}

    def exists(self, path):
        return path in self.fixtures if self.fixtures is not None else path.exists()

    def document(self, path):
        if path not in self.documents:
            text = self.fixtures[path] if self.fixtures is not None else path.read_text(encoding="utf-8")
            self.documents[path] = Document(text)
        return self.documents[path]

    def local_link_error(self, path, target):
        try:
            url = urlsplit(target)
        except ValueError as error:
            return f"invalid link {target!r}: {error}"
        if url.scheme or url.netloc or url.path.startswith("/"):
            return None  # Remote, mailto, and website-root links are not fetched.
        destination_path = (path.parent / unquote(url.path)).resolve() if url.path else path
        if not self.exists(destination_path):
            return f"link {target!r} targets a missing local path"
        fragment = unquote(url.fragment)
        if fragment:
            if self.fixtures is None and destination_path.is_dir():
                destination_path = destination_path / "README.md"
            if not self.exists(destination_path):
                return f"link {target!r} has no README for its directory fragment"
            if destination_path.suffix.lower() not in (".md", ".markdown"):
                return f"link {target!r}: fragments must target a Markdown heading"
            try:
                doc = self.document(destination_path)
            except (OSError, UnicodeError, KeyError) as error:
                return f"cannot read target of {target!r}: {error}"
            if fragment not in doc.anchors:
                return f"link {target!r} has no matching heading (including duplicate suffixes)"
        return None

    def check(self):
        errors, documents = [], {}
        for label, name in LANGUAGES:
            path = (self.root / name).resolve()
            try:
                doc = self.document(path)
            except (OSError, UnicodeError, KeyError) as error:
                errors.append(f"{name}: cannot read README: {error}")
                continue
            documents[name] = doc
            lines = doc.text.splitlines()
            if len(lines) < 3 or lines[2] != switcher(name):
                errors.append(f"{name}: line 3 must be the eight-language switcher, with only {label!r} bold and not linked")
            if doc.unclosed:
                errors.append(f"{name}: unclosed fenced code block")
            if HTML_ANCHOR.search(without_code(doc.prose)):
                errors.append(f"{name}: HTML anchor hacks are not allowed; use native heading slugs")
            for link in links(doc.prose):
                error = self.local_link_error(path, link.target)
                if error:
                    line = doc.prose.count("\n", 0, link.start) + 1
                    errors.append(f"{name}:{line}: {error}")
        source = documents.get("README.md")
        if source:
            for name, doc in documents.items():
                if name == "README.md":
                    continue
                if doc.h2_count != source.h2_count:
                    errors.append(f"{name}: level-2 heading count {doc.h2_count} != English {source.h2_count}")
                if doc.blocks != source.blocks:
                    errors.append(f"{name}: fenced block count or exact contents differ from English")
                if doc.tables != source.tables:
                    errors.append(f"{name}: table row/column structure or alignment differs from English")
                if len(doc.numbers) == len(source.numbers):
                    for section, (expected, actual) in enumerate(zip(source.numbers, doc.numbers)):
                        if actual != expected:
                            errors.append(f"{name}: numeric literals differ in level-2 section {section}: "
                                          f"missing {dict(expected - actual)}, extra {dict(actual - expected)}")
        return list(dict.fromkeys(errors))


def self_test(root):
    titles = ("Overview", "概览", "Vue d’ensemble", "Resumen", "Обзор", "نظرة عامة", "概要", "개요")
    fixtures = {}
    for (_, name), title in zip(LANGUAGES, titles):
        measurement = "13,7" if name == "README.fr.md" else "13.7"
        fixtures[name] = (
            f"# fixture\n\n{switcher(name)}\n\n[local](#{heading_slug(title)})\n\n## {title}\n\n"
            f"Measured {measurement} s; `context_tokens=64000`; version v5.5.2.\n\n"
            "| Item | State |\n|---|---|\n| sample | ready |\n\n"
            "```bash\n# [not a link](missing-in-code.md)\necho unchanged\n```\n\n"
            "`[not a link](missing-inline-code.md)`\n\n"
            "[target](docs/unicode.md#日本語한국어-1)\n"
        )
    fixtures["docs/unicode.md"] = (
        "# Target\n\n## 日本語・한국어\n\n## 日本語・한국어\n\n"
        "## 日本語한국어-1\n\n## 日本語・한국어\n"
    )
    expected_slugs = {"target", "日本語한국어", "日本語한국어-1", "日本語한국어-1-1", "日本語한국어-2"}
    if Document(fixtures["docs/unicode.md"]).anchors != expected_slugs:
        raise AssertionError("Unicode/duplicate heading slugs")
    if heading_slug("**Été** _한글_ `a_b` &amp; [日本語](somewhere.md)") != "été-한글-a_b--日本語":
        raise AssertionError("rendered heading formatting")
    if heading_slug("[日本語](somewhere.md) and ` a_b `") != "日本語-and-a_b":
        raise AssertionError("inline link/code rendering within headings")
    if Document("Setext title\n============\n\nDetails\n-------\n").anchors != {"setext-title", "details"}:
        raise AssertionError("setext headings")
    if Document("## [한글][target]\n\n[target]: docs/unicode.md\n").anchors != {"한글"}:
        raise AssertionError("reference links within headings")
    if table_cells(r"| `a|b` | c\|d |") != ["`a|b`", r"c\|d"]:
        raise AssertionError("table pipes in code/escaped pipes")
    if Validator(root, fixtures).check():
        raise AssertionError("valid eight-language fixture rejected")
    mutations = (
        ("switcher/self-link", "**한국어**", "[한국어](README.ko.md)", "line 3"),
        ("heading count", "## 개요", "### 개요", "heading count"),
        ("code contents", "echo unchanged", "echo changed", "fenced block"),
        ("table columns", "| sample | ready |", "| sample | ready | extra |", "table row/column"),
        ("numeric fact", "13.7", "13.8", "numeric literals"),
        ("inline configuration", "64000", "64001", "numeric literals"),
        ("version digits", "v5.5.2", "v5.5.3", "numeric literals"),
        ("native anchor", "#개요", "#overview", "no matching heading"),
        ("duplicate anchor", "#日本語한국어-1", "#日本語한국어-3", "no matching heading"),
        ("missing file", "docs/unicode.md#", "docs/missing.md#", "missing local path"),
        ("HTML anchor", "## 개요", '<a name="overview"></a>\n## 개요', "HTML anchor"),
    )
    example = None
    for description, old, new, diagnostic in mutations:
        mutated = dict(fixtures)
        mutated["README.ko.md"] = mutated["README.ko.md"].replace(old, new, 1)
        errors = Validator(root, mutated).check()
        matching = [error for error in errors if diagnostic in error]
        if not matching:
            raise AssertionError(f"mutation not rejected: {description}: {errors}")
        if description == "native anchor":
            example = matching[0]
    nested = dict(fixtures)
    nested["README.ko.md"] += (
        "\n[full][ref] [ref][] [ref]\n\n[ref]: docs/unicode.md#日本語한국어-1\n\n"
        "[![alt](docs/image.svg)](docs/unicode.md)\n"
    )
    nested["docs/image.svg"] = "<svg/>"
    if Validator(root, nested).check():
        raise AssertionError("valid reference links/nested image rejected")
    broken_reference = dict(nested)
    broken_reference["README.ko.md"] = nested["README.ko.md"].replace(
        "[ref]: docs/unicode.md#日本語한국어-1", "[ref]: docs/unicode.md#absent"
    )
    if not any("no matching heading" in e for e in Validator(root, broken_reference).check()):
        raise AssertionError("bad reference fragment accepted")
    del nested["docs/image.svg"]
    if not any("missing local path" in e for e in Validator(root, nested).check()):
        raise AssertionError("missing nested image accepted")
    del fixtures["README.ja.md"]
    if not any("missing local path" in e for e in Validator(root, fixtures).check()):
        raise AssertionError("missing switcher target accepted")
    # Reference links, angle destinations, percent encoding and escaped parentheses.
    sample = (
        "[inline](<docs/with (space).md> \"title\") [escaped](docs/a\\(b\\).md)\n"
        "[full][ref] [ref][] [ref]\n\n[ref]: docs/unicode.md#%E6%97%A5%E6%9C%AC%E8%AA%9E%ED%95%9C%EA%B5%AD%EC%96%B4-1\n"
    )
    found = [link.target for link in links(sample)]
    if found[:2] != ["docs/with (space).md", "docs/a(b).md"] or len(found) != 5:
        raise AssertionError(f"Markdown link syntax: {found}")
    path = (root / "README.md").resolve()
    checker = Validator(root, fixtures)
    if checker.local_link_error(path, found[-1]):
        raise AssertionError("percent-encoded Unicode fragment rejected")
    if not checker.local_link_error(path, "docs/unicode.md#absent"):
        raise AssertionError("bad reference fragment accepted")
    print(f"SELF-TEST PASS: valid fixtures and {len(mutations) + 3} rejected mutations; Unicode/duplicate slugs and link syntax checked")
    print(f"  Malformed in-memory fixture rejected: {example}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent,
                        help="repository root (default: this script's parent repository)")
    parser.add_argument("--self-test", action="store_true", help="also run in-memory fixtures/mutation checks; writes no files")
    args = parser.parse_args()
    if args.self_test:
        try:
            self_test(args.root.resolve())
        except AssertionError as error:
            print(f"SELF-TEST FAILED: {error}", file=sys.stderr)
            return 1
    validator = Validator(args.root)
    errors = validator.check()
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    source = validator.document((args.root / "README.md").resolve())
    rows = sum(len(shape) for shape, _ in source.tables)
    print("PASS: " + ", ".join(name for _, name in LANGUAGES))
    print(f"  {source.h2_count} level-2 headings; {len(source.blocks)} fenced block(s), exact contents; "
          f"{len(source.tables)} tables/{rows} rows; switchers, numeric literals and local links checked")
    return 0


if __name__ == "__main__":
    sys.exit(main())
