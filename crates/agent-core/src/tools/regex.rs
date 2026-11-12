//! A small regular expression engine, for `grep -E`.
//!
//! The board has no `regex` crate (it is a large dependency on a 320 KB binary), and the
//! thing a model actually needs from `grep` is not full PCRE: it is "lines that start with
//! an error", "digits before a colon", "a word boundary I can write". So this is a documented
//! subset, and anything outside it is refused with the reason:
//!
//! ```text
//! literal      abc          matches itself
//! .            any one character except the end of the line
//! ^ $          start / end of the line
//! * + ?        zero-or-more, one-or-more, at-most-one (greedy, with backtracking)
//! [...]        character class: [abc], [a-z0-9], [^"]  (negated with a leading ^)
//! \d \w \s     digit, word character (alphanumeric or _), whitespace
//! \D \W \S     the negation of each
//! \\ \. \* ... an escaped metacharacter matches itself
//! ```
//!
//! Not supported, on purpose: `( )` groups, `|` alternation, `{n,m}` intervals, back
//! references, lookaround. Groups are what turns a matcher into a parser; the board has
//! `script` for a condition that needs them, and the error says so instead of matching
//! something else.

/// One character class entry.
#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

/// One element of a compiled pattern.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Literal(char),
    Any,
    Class { items: Vec<ClassItem>, negated: bool },
    Digit(bool),
    Word(bool),
    Space(bool),
    Start,
    End,
    Repeat { node: Box<Node>, min: usize, max: Option<usize> },
}

/// A compiled pattern: a sequence of nodes, matched left to right with backtracking.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    nodes: Vec<Node>,
}

const SUBSET: &str = "supported: literals, `.`, `^`, `$`, `*`, `+`, `?`, `[...]`, `\\d \\w \\s`";

impl Pattern {
    pub fn compile(src: &str) -> Result<Self, String> {
        let chars: Vec<char> = src.chars().collect();
        let mut nodes: Vec<Node> = Vec::with_capacity(chars.len());
        let mut i = 0usize;
        while i < chars.len() {
            let ch = chars[i];
            match ch {
                '(' | ')' | '|' => {
                    return Err(format!(
                        "`{ch}` (groups and alternation) is not supported; {SUBSET} — use `script` for a condition that needs them"
                    ))
                }
                '{' | '}' => {
                    return Err(format!(
                        "`{ch}` ({{n,m}} intervals) is not supported; {SUBSET} — write the repetition out"
                    ))
                }
                '*' | '+' | '?' => {
                    let Some(prev) = nodes.pop() else {
                        return Err(format!("`{ch}` has nothing in front of it; {SUBSET}"));
                    };
                    if matches!(prev, Node::Repeat { .. }) {
                        return Err(format!("`{ch}` on a repetition (as in `a*+`) is not supported"));
                    }
                    let (min, max) = match ch {
                        '*' => (0, None),
                        '+' => (1, None),
                        _ => (0, Some(1)),
                    };
                    nodes.push(Node::Repeat { node: Box::new(prev), min, max });
                    i += 1;
                }
                '^' if i == 0 => {
                    nodes.push(Node::Start);
                    i += 1;
                }
                '$' if i + 1 == chars.len() => {
                    nodes.push(Node::End);
                    i += 1;
                }
                '.' => {
                    nodes.push(Node::Any);
                    i += 1;
                }
                '[' => {
                    let (node, next) = parse_class(&chars, i)?;
                    nodes.push(node);
                    i = next;
                }
                '\\' => {
                    let Some(&esc) = chars.get(i + 1) else {
                        return Err("a trailing `\\` has nothing to escape".to_string());
                    };
                    let node = match esc {
                        'd' => Node::Digit(true),
                        'D' => Node::Digit(false),
                        'w' => Node::Word(true),
                        'W' => Node::Word(false),
                        's' => Node::Space(true),
                        'S' => Node::Space(false),
                        'n' => Node::Literal('\n'),
                        't' => Node::Literal('\t'),
                        c if !c.is_alphanumeric() => Node::Literal(c),
                        other => return Err(format!("unknown escape `\\{other}`; {SUBSET}")),
                    };
                    nodes.push(node);
                    i += 2;
                }
                c => {
                    nodes.push(Node::Literal(c));
                    i += 1;
                }
            }
        }
        Ok(Pattern { nodes })
    }

    /// Is there a match anywhere in `text`? (A line, in `grep`'s case.)
    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        if matches!(self.nodes.first(), Some(Node::Start)) {
            return self.match_seq(&self.nodes[1..], &chars, 0).is_some();
        }
        // Unanchored: a match may begin at any position, including the empty one at the end
        // (`x*` matches every line, as it does in every grep).
        (0..=chars.len()).any(|start| self.match_seq(&self.nodes, &chars, start).is_some())
    }

    fn match_seq(&self, nodes: &[Node], text: &[char], pos: usize) -> Option<usize> {
        let Some((head, rest)) = nodes.split_first() else {
            return Some(pos);
        };
        match head {
            Node::Start => (pos == 0).then(|| self.match_seq(rest, text, pos)).flatten(),
            Node::End => (pos == text.len()).then(|| self.match_seq(rest, text, pos)).flatten(),
            Node::Repeat { node, min, max } => {
                // Greedy, then back off one character at a time. Every atom here is one
                // character wide (there are no groups), so stepping back by one is exact.
                let mut count = 0usize;
                let mut end = pos;
                while max.map(|limit| count < limit).unwrap_or(true) {
                    match self.match_one(node, text, end) {
                        Some(next) => {
                            count += 1;
                            end = next;
                        }
                        None => break,
                    }
                }
                loop {
                    if count >= *min {
                        if let Some(found) = self.match_seq(rest, text, end) {
                            return Some(found);
                        }
                    }
                    if count == 0 {
                        return None;
                    }
                    count -= 1;
                    end -= 1;
                }
            }
            other => self.match_one(other, text, pos).and_then(|next| self.match_seq(rest, text, next)),
        }
    }

    fn match_one(&self, node: &Node, text: &[char], pos: usize) -> Option<usize> {
        let c = *text.get(pos)?;
        let hit = match node {
            Node::Literal(expected) => c == *expected,
            Node::Any => true,
            Node::Digit(want) => c.is_ascii_digit() == *want,
            Node::Word(want) => (c.is_alphanumeric() || c == '_') == *want,
            Node::Space(want) => c.is_whitespace() == *want,
            Node::Class { items, negated } => {
                let inside = items.iter().any(|item| match item {
                    ClassItem::Char(x) => c == *x,
                    ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
                });
                inside != *negated
            }
            _ => return None,
        };
        hit.then_some(pos + 1)
    }
}

/// `[...]` at `start`; returns the node and the index just past the closing bracket.
fn parse_class(chars: &[char], start: usize) -> Result<(Node, usize), String> {
    let mut i = start + 1;
    let negated = chars.get(i) == Some(&'^');
    if negated {
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    let mut first = true;
    loop {
        let Some(&c) = chars.get(i) else {
            return Err("`[` is never closed; write a class like `[a-z0-9]`".to_string());
        };
        // A `]` in the first position is a literal `]`, as in every regex dialect.
        if c == ']' && !first {
            return Ok((Node::Class { items, negated }, i + 1));
        }
        first = false;
        let (lo, used) = match c {
            '\\' => match chars.get(i + 1) {
                Some('d') => {
                    items.extend([ClassItem::Range('0', '9')]);
                    i += 2;
                    continue;
                }
                Some('w') => {
                    items.extend([
                        ClassItem::Range('a', 'z'),
                        ClassItem::Range('A', 'Z'),
                        ClassItem::Range('0', '9'),
                        ClassItem::Char('_'),
                    ]);
                    i += 2;
                    continue;
                }
                Some('s') => {
                    items.extend([ClassItem::Char(' '), ClassItem::Char('\t')]);
                    i += 2;
                    continue;
                }
                Some(&other) => (other, 2),
                None => return Err("a trailing `\\` inside a class".to_string()),
            },
            other => (other, 1),
        };
        i += used;
        // A range, unless the `-` is the last character before `]`.
        if chars.get(i) == Some(&'-') && chars.get(i + 1).map(|c| *c != ']').unwrap_or(false) {
            let Some(&hi) = chars.get(i + 1) else {
                return Err("a class ends inside a range".to_string());
            };
            if hi < lo {
                return Err(format!("the range `{lo}-{hi}` runs backwards"));
            }
            items.push(ClassItem::Range(lo, hi));
            i += 2;
        } else {
            items.push(ClassItem::Char(lo));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        Pattern::compile(pattern).expect("a pattern").is_match(text)
    }

    #[test]
    fn literals_and_the_wildcard() {
        assert!(matches("error", "an error happened"));
        assert!(!matches("error", "an ERROR happened"), "case matters");
        assert!(matches("a.c", "abc"));
        assert!(!matches("a.c", "ac"));
        assert!(matches("", "anything"), "an empty pattern matches, as grep does");
    }

    #[test]
    fn anchors_bind_to_the_whole_line() {
        assert!(matches("^ERR", "ERR: one"));
        assert!(!matches("^ERR", "an ERR: one"));
        assert!(matches("done$", "all done"));
        assert!(!matches("done$", "done already"));
        assert!(matches("^$", ""), "an empty line");
    }

    #[test]
    fn quantifiers_are_greedy_and_backtrack() {
        assert!(matches("ab*c", "ac"));
        assert!(matches("ab*c", "abbbc"));
        assert!(matches("ab+c", "abbbc"));
        assert!(!matches("ab+c", "ac"), "+ needs one");
        assert!(matches("colou?r", "color") && matches("colou?r", "colour"));
        // Greedy, then back off: `.*` must leave a `c` for the last node.
        assert!(matches("^.*c$", "abc"));
        assert!(matches("^a*$", "aaaa"));
        assert!(!matches("^a+$", ""), "+ on an empty line is still no");
    }

    #[test]
    fn classes_ranges_and_negation() {
        assert!(matches("[0-9]+:[0-9]+", "at 12:30"));
        assert!(!matches("[0-9]+:[0-9]+", "at aa:bb"));
        assert!(matches("[^abc]", "xyz"));
        assert!(!matches("^[^abc]+$", "abc"));
        assert!(matches("[]a]", "]"), "a `]` first is literal");
        assert!(matches("[a-]", "-"), "a trailing `-` is literal");
    }

    #[test]
    fn the_shorthands_and_escapes() {
        assert!(matches("\\d+", "abc 42"));
        assert!(!matches("^\\D+$", "abc1"));
        assert!(matches("\\w+_\\w+", "bme280_read"));
        assert!(matches("^\\s", " indented"));
        assert!(matches("a\\*b", "a*b"));
        assert!(!matches("a\\*b", "aXb"), "the escape is a literal star");
        assert!(matches("\\.txt$", "/notes/a.txt"));
    }

    #[test]
    fn what_is_not_supported_says_so() {
        for (pattern, needle) in [
            ("(a|b)", "not supported"),
            ("a|b", "not supported"),
            ("a{2}", "not supported"),
            ("*a", "nothing in front"),
            ("[abc", "never closed"),
            ("[z-a]", "backwards"),
            ("\\q", "unknown escape"),
        ] {
            let Err(error) = Pattern::compile(pattern) else {
                panic!("`{pattern}` compiled, but it is not in the subset")
            };
            assert!(error.contains(needle), "`{pattern}` gave: {error}");
        }
    }
}
