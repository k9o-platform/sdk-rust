//! A small backtracking regular-expression engine for the `pattern` keyword of Moat schemas.
//!
//! Supported: literals, `.`, classes (`[a-z]`, `[^...]`, with `\d \w \s` inside), the escapes
//! `\d \D \w \W \s \S \b \B \n \r \t` and escaped punctuation, anchors `^` and `$`, groups
//! `( )` and `(?: )`, alternation `|`, and the greedy quantifiers `* + ? {m} {m,} {m,n}`
//! (a trailing `?` makes them lazy). Look-around and back-references are not supported and are
//! rejected when the pattern is compiled. Matching is searched, not anchored, over Unicode scalar
//! values, with a step budget so a hostile pattern cannot run for long (an exhausted budget is
//! reported as "no match").

use std::fmt;

#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
    Start,
    End,
    WordBoundary(bool),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        lazy: bool,
    },
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub(crate) struct Regex {
    root: Node,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegexError(pub String);

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

fn err<T>(msg: &str) -> Result<T, RegexError> {
    Err(RegexError(msg.to_string()))
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn parse_alt(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            items.push(self.parse_quantifier(atom)?);
        }
        Ok(Node::Concat(items))
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, RegexError> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, None)
            }
            Some('+') => {
                self.pos += 1;
                (1, None)
            }
            Some('?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some('{') => match self.try_braces()? {
                Some(r) => r,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Start | Node::End | Node::WordBoundary(_)) {
            return err("nothing to repeat");
        }
        let lazy = if self.peek() == Some('?') {
            self.pos += 1;
            true
        } else {
            false
        };
        if let Some(m) = max {
            if m < min {
                return err("quantifier bounds are reversed");
            }
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            lazy,
        })
    }

    /// `{m}`, `{m,}`, `{m,n}`; anything else is a literal `{`.
    fn try_braces(&mut self) -> Result<Option<(usize, Option<usize>)>, RegexError> {
        let save = self.pos;
        self.pos += 1;
        let num = |p: &mut Parser| -> Option<usize> {
            let start = p.pos;
            while p.peek().map_or(false, |c| c.is_ascii_digit()) {
                p.pos += 1;
            }
            if p.pos == start {
                return None;
            }
            p.chars[start..p.pos]
                .iter()
                .collect::<String>()
                .parse()
                .ok()
        };
        let min = match num(self) {
            Some(n) => n,
            None => {
                self.pos = save;
                return Ok(None);
            }
        };
        let result = match self.peek() {
            Some('}') => {
                self.pos += 1;
                (min, Some(min))
            }
            Some(',') => {
                self.pos += 1;
                let max = num(self);
                if self.peek() != Some('}') {
                    self.pos = save;
                    return Ok(None);
                }
                self.pos += 1;
                (min, max)
            }
            _ => {
                self.pos = save;
                return Ok(None);
            }
        };
        if result.0 > 1000 || result.1.map_or(false, |m| m > 1000) {
            return err("quantifier bound is too large");
        }
        Ok(Some(result))
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let c = self.peek().unwrap();
        self.pos += 1;
        match c {
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '(' => {
                if self.peek() == Some('?') {
                    if self.chars.get(self.pos + 1) == Some(&':') {
                        self.pos += 2;
                    } else {
                        return err("look-around and named groups are not supported");
                    }
                }
                let inner = self.parse_alt()?;
                if self.peek() != Some(')') {
                    return err("unclosed group");
                }
                self.pos += 1;
                Ok(inner)
            }
            '[' => self.parse_class(),
            '\\' => self.parse_escape(false).map(|e| match e {
                Escaped::Char(c) => Node::Char(c),
                Escaped::Class(item) => Node::Class {
                    negated: false,
                    items: vec![item],
                },
                Escaped::Boundary(b) => Node::WordBoundary(b),
            }),
            '*' | '+' | '?' => err("nothing to repeat"),
            c => Ok(Node::Char(c)),
        }
    }

    fn parse_escape(&mut self, in_class: bool) -> Result<Escaped, RegexError> {
        let c = match self.peek() {
            Some(c) => c,
            None => return err("trailing backslash"),
        };
        self.pos += 1;
        Ok(match c {
            'd' => Escaped::Class(ClassItem::Digit(true)),
            'D' => Escaped::Class(ClassItem::Digit(false)),
            'w' => Escaped::Class(ClassItem::Word(true)),
            'W' => Escaped::Class(ClassItem::Word(false)),
            's' => Escaped::Class(ClassItem::Space(true)),
            'S' => Escaped::Class(ClassItem::Space(false)),
            'b' if !in_class => Escaped::Boundary(true),
            'B' if !in_class => Escaped::Boundary(false),
            'b' => Escaped::Char('\u{8}'),
            'n' => Escaped::Char('\n'),
            'r' => Escaped::Char('\r'),
            't' => Escaped::Char('\t'),
            'f' => Escaped::Char('\u{c}'),
            'v' => Escaped::Char('\u{b}'),
            '0' => Escaped::Char('\0'),
            c if c.is_ascii_digit() => return err("back-references are not supported"),
            c if c.is_ascii_alphabetic() => return err("unsupported escape"),
            c => Escaped::Char(c),
        })
    }

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        let mut negated = false;
        if self.peek() == Some('^') {
            negated = true;
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return err("unclosed character class"),
            };
            self.pos += 1;
            if c == ']' && !first {
                break;
            }
            first = false;
            let lo = if c == '\\' {
                match self.parse_escape(true)? {
                    Escaped::Char(c) => c,
                    Escaped::Class(item) => {
                        items.push(item);
                        continue;
                    }
                    Escaped::Boundary(_) => unreachable!(),
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).map_or(false, |n| *n != ']')
            {
                self.pos += 1;
                let hc = self.peek().unwrap();
                self.pos += 1;
                let hi = if hc == '\\' {
                    match self.parse_escape(true)? {
                        Escaped::Char(c) => c,
                        _ => return err("a class escape cannot end a range"),
                    }
                } else {
                    hc
                };
                if hi < lo {
                    return err("class range is reversed");
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Ok(Node::Class { negated, items })
    }
}

enum Escaped {
    Char(char),
    Class(ClassItem),
    Boundary(bool),
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn class_matches(items: &[ClassItem], c: char) -> bool {
    items.iter().any(|i| match i {
        ClassItem::Range(a, b) => *a <= c && c <= *b,
        ClassItem::Digit(pos) => c.is_ascii_digit() == *pos,
        ClassItem::Word(pos) => is_word(c) == *pos,
        ClassItem::Space(pos) => c.is_whitespace() == *pos,
    })
}

struct Matcher<'a> {
    text: &'a [char],
    steps: usize,
}

const STEP_BUDGET: usize = 2_000_000;

impl Matcher<'_> {
    fn m(&mut self, node: &Node, pos: usize, k: &mut dyn FnMut(&mut Self, usize) -> bool) -> bool {
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return false;
        }
        match node {
            Node::Char(c) => pos < self.text.len() && self.text[pos] == *c && k(self, pos + 1),
            Node::Any => pos < self.text.len() && self.text[pos] != '\n' && k(self, pos + 1),
            Node::Class { negated, items } => {
                pos < self.text.len()
                    && class_matches(items, self.text[pos]) != *negated
                    && k(self, pos + 1)
            }
            Node::Start => pos == 0 && k(self, pos),
            Node::End => pos == self.text.len() && k(self, pos),
            Node::WordBoundary(want) => {
                let before = pos > 0 && is_word(self.text[pos - 1]);
                let after = pos < self.text.len() && is_word(self.text[pos]);
                ((before != after) == *want) && k(self, pos)
            }
            Node::Concat(items) => self.concat(items, pos, k),
            Node::Alt(branches) => {
                for b in branches {
                    if self.m(b, pos, k) {
                        return true;
                    }
                }
                false
            }
            Node::Repeat {
                node,
                min,
                max,
                lazy,
            } => self.repeat(node, *min, *max, *lazy, 0, pos, k),
        }
    }

    fn concat(
        &mut self,
        items: &[Node],
        pos: usize,
        k: &mut dyn FnMut(&mut Self, usize) -> bool,
    ) -> bool {
        match items.split_first() {
            None => k(self, pos),
            Some((first, rest)) => self.m(first, pos, &mut |s: &mut Self, p| s.concat(rest, p, k)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &mut self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        lazy: bool,
        count: usize,
        pos: usize,
        k: &mut dyn FnMut(&mut Self, usize) -> bool,
    ) -> bool {
        let can_more = max.map_or(true, |m| count < m);
        let try_more = |s: &mut Self, k: &mut dyn FnMut(&mut Self, usize) -> bool| -> bool {
            can_more
                && s.m(node, pos, &mut |s2: &mut Self, p| {
                    // an iteration that consumes nothing cannot make progress
                    if p == pos && count >= min {
                        return false;
                    }
                    s2.repeat(node, min, max, lazy, count + 1, p, k)
                })
        };
        if count < min {
            return try_more(self, k);
        }
        if lazy {
            k(self, pos) || try_more(self, k)
        } else {
            try_more(self, k) || k(self, pos)
        }
    }
}

impl Regex {
    pub(crate) fn new(pattern: &str) -> Result<Regex, RegexError> {
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
        };
        let root = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return err("unmatched )");
        }
        Ok(Regex { root })
    }

    /// True when the pattern matches anywhere in `text`.
    pub(crate) fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let mut m = Matcher {
            text: &chars,
            steps: 0,
        };
        for start in 0..=chars.len() {
            if m.m(&self.root, start, &mut |_, _| true) {
                return true;
            }
            if m.steps > STEP_BUDGET {
                return false;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(p: &str, t: &str) -> bool {
        Regex::new(p).unwrap().is_match(t)
    }

    #[test]
    fn searched_not_anchored() {
        assert!(is("abc", "xxabcxx"));
        assert!(!is("^abc$", "xxabcxx"));
        assert!(is("^abc$", "abc"));
        assert!(is("", "anything"));
    }

    #[test]
    fn classes_quantifiers_groups_alternation() {
        assert!(is(r"^\d{3}-\d{2,}$", "123-4567"));
        assert!(!is(r"^\d{3}-\d{2,}$", "12-4567"));
        assert!(is(r"^(cat|dog)s?$", "dogs"));
        assert!(!is(r"^(cat|dog)s?$", "bird"));
        assert!(is(r"^[a-c]+[^a-c]$", "abcd"));
        assert!(is(r"^[\w.-]+@[\w-]+\.[a-z]{2,}$", "a.b@ex-ample.com"));
        assert!(is(r"\bfoo\b", "a foo b"));
        assert!(!is(r"\bfoo\b", "afoob"));
        assert!(is(r"^(?:ab)*$", "ababab"));
        assert!(is(r"^a.*?b$", "axxb"));
    }

    #[test]
    fn rejects_unsupported_syntax() {
        assert!(Regex::new("(?=a)").is_err());
        assert!(Regex::new(r"(a)\1").is_err());
        assert!(Regex::new("(a").is_err());
        assert!(Regex::new("a)").is_err());
        assert!(Regex::new("[a").is_err());
        assert!(Regex::new("*a").is_err());
    }

    #[test]
    fn hostile_pattern_is_bounded() {
        let r = Regex::new("^(a+)+$").unwrap();
        let text = format!("{}b", "a".repeat(40));
        assert!(!r.is_match(&text));
    }
}
