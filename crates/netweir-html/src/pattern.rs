//! The `pattern` attribute's regular expressions: JavaScript's, compiled
//! with the `v` flag and anchored to the whole value, as the HTML Standard
//! says. Only that much of JavaScript is here: no flags but `v`, so
//! matching is case-sensitive, `.` stops at line terminators, and `^` and
//! `$` mean the ends of the value. A pattern that doesn't compile under the
//! `v` flag is None, which the standard treats as no pattern.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};

/// Steps a match may take before it gives up, so a pattern built to
/// backtrack hard can't hang a query. A match that runs out doesn't match.
const STEP_LIMIT: u32 = 1_000_000;

/// Steps every match in one select may take together, so a page with many
/// inputs carrying a hard pattern can't multiply the cost. Once it's spent,
/// the rest of the matches in that select give up the same way.
pub(crate) const SELECT_STEP_LIMIT: u32 = 10 * STEP_LIMIT;

/// How deep groups and classes may nest. Deeper doesn't compile, so a page
/// can't overflow the stack with `((((...`.
const MAX_DEPTH: u32 = 32;

/// How much stack one match may use. Matching recurses: each character a
/// repeat consumes goes deeper, unless the repeat is of one character, which
/// loops. A match that would go past this gives up, so a long value against
/// `(ab)*` can't overflow a small thread stack. It's measured in bytes
/// rather than levels because a level's frames are several times bigger in
/// a debug build than in a release one.
const MATCH_STACK_LIMIT: usize = 64 * 1024;

const MAX_CHAR: u32 = 0x10FFFF;

pub(crate) struct Pattern {
    node: Node,
    groups: usize,
}

impl Pattern {
    /// Compiles `source` as `^(?:source)$` with the `v` flag.
    pub(crate) fn compile(source: &str) -> Option<Pattern> {
        let chars: Vec<u32> = source.chars().map(u32::from).collect();
        let (groups, names) = count_groups(&chars)?;
        let mut p = Parser {
            src: &chars,
            pos: 0,
            names,
            next_group: 0,
            backrefs: Vec::new(),
            depth: 0,
        };
        let node = p.disjunction()?;
        if p.pos != chars.len() {
            return None; // an unmatched ')'
        }
        if p.backrefs.iter().any(|&n| n == 0 || n > groups) {
            return None;
        }
        Some(Pattern { node, groups })
    }

    /// Whether the whole of `value` matches, taking steps from `budget`,
    /// which a select shares across its matches.
    pub(crate) fn matches(&self, value: &str, budget: &Cell<u32>) -> bool {
        let text: Vec<u32> = value.chars().map(u32::from).collect();
        let m = Matcher {
            text: &text,
            steps: Cell::new(0),
            budget,
            stack_top: stack_address(),
        };
        let mut caps = vec![None; self.groups + 1];
        m.run(&self.node, 0, &mut caps, &mut |end, _| end == text.len())
    }
}

#[derive(Debug)]
enum Node {
    Empty,
    Char(u32),
    /// `.`: anything but a line terminator.
    Any,
    /// A class, with its `\q{}` strings longest first, sorted once here.
    Class(Box<Set>, Vec<Vec<u32>>),
    Start,
    End,
    WordBoundary(bool),
    /// A group, capturing when it has a number.
    Group(Box<Node>, Option<usize>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        /// The capture groups inside, cleared at each repetition.
        groups: (usize, usize),
    },
    Look {
        node: Box<Node>,
        ahead: bool,
        negate: bool,
    },
    BackRef(usize),
}

/// A `v`-flag character class: code point ranges, and strings from `\q{}`
/// whose length isn't one.
#[derive(Debug, Clone, Default)]
struct Set {
    ranges: Vec<(u32, u32)>,
    strings: BTreeSet<Vec<u32>>,
}

impl Set {
    fn of(ranges: &[(u32, u32)]) -> Set {
        let mut s = Set {
            ranges: ranges.to_vec(),
            strings: BTreeSet::new(),
        };
        s.normalize();
        s
    }

    fn normalize(&mut self) {
        self.ranges.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
        for &(lo, hi) in &self.ranges {
            match out.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => out.push((lo, hi)),
            }
        }
        self.ranges = out;
    }

    fn contains(&self, c: u32) -> bool {
        self.ranges
            .binary_search_by(|&(lo, hi)| {
                if hi < c {
                    std::cmp::Ordering::Less
                } else if lo > c {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    fn union(mut self, other: Set) -> Set {
        self.ranges.extend(other.ranges);
        self.strings.extend(other.strings);
        self.normalize();
        self
    }

    fn complement_ranges(ranges: &[(u32, u32)]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut next = 0;
        for &(lo, hi) in ranges {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi + 1;
        }
        if next <= MAX_CHAR {
            out.push((next, MAX_CHAR));
        }
        out
    }

    fn intersect(self, other: Set) -> Set {
        // A ∩ B = ¬(¬A ∪ ¬B) for the ranges.
        let not_a = Set::complement_ranges(&self.ranges);
        let not_b = Set::complement_ranges(&other.ranges);
        let either = Set::of(&[not_a, not_b].concat());
        Set {
            ranges: Set::complement_ranges(&either.ranges),
            strings: self.strings.intersection(&other.strings).cloned().collect(),
        }
    }

    fn subtract(self, other: Set) -> Set {
        let not_b = Set::complement_ranges(&other.ranges);
        let mut s = self.intersect(Set::of(&not_b));
        s.strings.retain(|x| !other.strings.contains(x));
        s
    }

    /// `[^...]`: only a set without strings has a complement.
    fn complement(self) -> Option<Set> {
        self.strings
            .is_empty()
            .then(|| Set::of(&Set::complement_ranges(&self.ranges)))
    }
}

fn class_node(set: Set) -> Node {
    let mut strings: Vec<Vec<u32>> = set.strings.iter().cloned().collect();
    strings.sort_by_key(|s| std::cmp::Reverse(s.len()));
    Node::Class(Box::new(set), strings)
}

fn digit_set() -> Set {
    Set::of(&[(0x30, 0x39)])
}

fn word_set() -> Set {
    Set::of(&[(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)])
}

/// JavaScript's WhiteSpace and LineTerminator.
fn space_set() -> Set {
    Set::of(&[
        (0x09, 0x0D),
        (0x20, 0x20),
        (0xA0, 0xA0),
        (0x1680, 0x1680),
        (0x2000, 0x200A),
        (0x2028, 0x2029),
        (0x202F, 0x202F),
        (0x205F, 0x205F),
        (0x3000, 0x3000),
        (0xFEFF, 0xFEFF),
    ])
}

fn is_line_terminator(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_word(c: u32) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

/// Counts the capturing groups and collects their names before parsing,
/// since a backreference may come before its group. None for a duplicate
/// name.
fn count_groups(src: &[u32]) -> Option<(usize, HashMap<Vec<u32>, usize>)> {
    let mut count = 0;
    let mut names = HashMap::new();
    let mut i = 0;
    let mut depth = 0; // inside a class, which can nest under the v flag
    while i < src.len() {
        match src[i] {
            0x5C => i += 1, // skip what a backslash escapes
            0x5B => depth += 1,
            0x5D if depth > 0 => depth -= 1,
            0x28 if depth == 0 => {
                if src.get(i + 1) != Some(&0x3F) {
                    count += 1;
                } else if src.get(i + 2) == Some(&0x3C)
                    && !matches!(src.get(i + 3), Some(&0x3D) | Some(&0x21))
                {
                    count += 1;
                    let start = i + 3;
                    let end = start + src[start..].iter().position(|&c| c == 0x3E)?;
                    if names.insert(src[start..end].to_vec(), count).is_some() {
                        return None;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Some((count, names))
}

struct Parser<'a> {
    src: &'a [u32],
    pos: usize,
    names: HashMap<Vec<u32>, usize>,
    next_group: usize,
    backrefs: Vec<usize>,
    depth: u32,
}

impl Parser<'_> {
    /// Runs `f` one nesting level deeper, or fails past MAX_DEPTH.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn peek(&self) -> Option<u32> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u32> {
        self.src.get(self.pos + n).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c as u32) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, s: &str) -> bool {
        let n = s.chars().count();
        if self.src.len() >= self.pos + n
            && s.chars()
                .zip(&self.src[self.pos..])
                .all(|(a, &b)| a as u32 == b)
        {
            self.pos += n;
            true
        } else {
            false
        }
    }

    fn disjunction(&mut self) -> Option<Node> {
        let mut alts = vec![self.alternative()?];
        while self.eat('|') {
            alts.push(self.alternative()?);
        }
        Some(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        })
    }

    fn alternative(&mut self) -> Option<Node> {
        let mut terms = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' as u32 || c == ')' as u32 {
                break;
            }
            terms.push(self.term()?);
        }
        Some(match terms.len() {
            0 => Node::Empty,
            1 => terms.pop().unwrap(),
            _ => Node::Concat(terms),
        })
    }

    fn term(&mut self) -> Option<Node> {
        let first_group = self.next_group;
        let (atom, quantifiable) = self.atom()?;
        let Some((min, max)) = self.quantifier()? else {
            return Some(atom);
        };
        if !quantifiable {
            return None;
        }
        let greedy = !self.eat('?');
        Some(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
            groups: (first_group + 1, self.next_group + 1),
        })
    }

    /// `*`, `+`, `?` or `{n}`, `{n,}`, `{n,m}`. Under the v flag a `{` that
    /// doesn't start one is an error.
    fn quantifier(&mut self) -> Option<Option<(u32, Option<u32>)>> {
        let q = match self.peek() {
            Some(0x2A) => (0, None),
            Some(0x2B) => (1, None),
            Some(0x3F) => (0, Some(1)),
            Some(0x7B) => {
                self.pos += 1;
                let min = self.number()?;
                let max = if self.eat(',') {
                    if self.peek() == Some('}' as u32) {
                        None
                    } else {
                        Some(self.number()?)
                    }
                } else {
                    Some(min)
                };
                if !self.eat('}') || max.is_some_and(|m| m < min) {
                    return None;
                }
                return Some(Some((min, max)));
            }
            _ => return Some(None),
        };
        self.pos += 1;
        Some(Some(q))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut n: u32 = 0;
        while let Some(d) = self.peek().and_then(|c| char::from_u32(c)?.to_digit(10)) {
            n = n.saturating_mul(10).saturating_add(d);
            self.pos += 1;
        }
        (self.pos > start).then_some(n)
    }

    /// An atom, and whether a quantifier may follow it.
    fn atom(&mut self) -> Option<(Node, bool)> {
        let c = self.peek()?;
        self.pos += 1;
        Some(match char::from_u32(c)? {
            '^' => (Node::Start, false),
            '$' => (Node::End, false),
            '.' => (Node::Any, true),
            '[' => (self.nested(Self::class)?, true),
            '(' => self.nested(Self::group)?,
            '\\' => self.atom_escape()?,
            '*' | '+' | '?' | '{' | '}' | ']' => return None,
            _ => (Node::Char(c), true),
        })
    }

    fn group(&mut self) -> Option<(Node, bool)> {
        let look = |ahead, negate| Some((ahead, negate));
        let kind = if self.eat_str("?:") {
            None
        } else if self.eat_str("?=") {
            look(true, false)
        } else if self.eat_str("?!") {
            look(true, true)
        } else if self.eat_str("?<=") {
            look(false, false)
        } else if self.eat_str("?<!") {
            look(false, true)
        } else if self.eat_str("?<") {
            let end = self.pos + self.src[self.pos..].iter().position(|&c| c == 0x3E)?;
            if !valid_group_name(&self.src[self.pos..end]) {
                return None;
            }
            self.pos = end + 1;
            self.next_group += 1;
            let n = self.next_group;
            let inner = self.disjunction()?;
            return self
                .eat(')')
                .then(|| (Node::Group(Box::new(inner), Some(n)), true));
        } else if self.peek() == Some('?' as u32) {
            return None; // modifiers such as (?i:...) aren't covered
        } else {
            self.next_group += 1;
            let n = self.next_group;
            let inner = self.disjunction()?;
            return self
                .eat(')')
                .then(|| (Node::Group(Box::new(inner), Some(n)), true));
        };
        let inner = self.disjunction()?;
        if !self.eat(')') {
            return None;
        }
        Some(match kind {
            None => (Node::Group(Box::new(inner), None), true),
            // Under the v flag no lookaround takes a quantifier.
            Some((ahead, negate)) => (
                Node::Look {
                    node: Box::new(inner),
                    ahead,
                    negate,
                },
                false,
            ),
        })
    }

    fn atom_escape(&mut self) -> Option<(Node, bool)> {
        let c = char::from_u32(self.peek()?)?;
        match c {
            'b' | 'B' => {
                self.pos += 1;
                Some((Node::WordBoundary(c == 'b'), false))
            }
            '1'..='9' => {
                let n = self.number()? as usize;
                self.backrefs.push(n);
                Some((Node::BackRef(n), true))
            }
            'k' => {
                self.pos += 1;
                if !self.eat('<') {
                    return None;
                }
                let end = self.pos + self.src[self.pos..].iter().position(|&c| c == 0x3E)?;
                let n = *self.names.get(&self.src[self.pos..end])?;
                self.pos = end + 1;
                Some((Node::BackRef(n), true))
            }
            _ => {
                if let Some(set) = self.class_escape()? {
                    return Some((class_node(set), true));
                }
                Some((Node::Char(self.char_escape(false)?), true))
            }
        }
    }

    /// `\d`, `\D`, `\w`, `\W`, `\s` or `\S`; None inside for anything
    /// else, and an outer None for `\p{}`, which needs Unicode tables.
    fn class_escape(&mut self) -> Option<Option<Set>> {
        let set = match char::from_u32(self.peek()?)? {
            'd' => digit_set(),
            'D' => digit_set().complement()?,
            'w' => word_set(),
            'W' => word_set().complement()?,
            's' => space_set(),
            'S' => space_set().complement()?,
            // shortcut: no Unicode property tables, so \p{...} and \P{...} don't compile and the pattern counts as none; add tables if anyone needs them.
            'p' | 'P' => return None,
            _ => return Some(None),
        };
        self.pos += 1;
        Some(Some(set))
    }

    /// The character an escape stands for. In a class, `\b` is a
    /// backspace, and the v flag's reserved punctuators may be escaped.
    fn char_escape(&mut self, in_class: bool) -> Option<u32> {
        let c = self.peek()?;
        self.pos += 1;
        Some(match char::from_u32(c)? {
            't' => 9,
            'n' => 10,
            'v' => 11,
            'f' => 12,
            'r' => 13,
            'b' if in_class => 8,
            '0' if !self.peek().is_some_and(|d| (0x30..=0x39).contains(&d)) => 0,
            'c' => {
                let l = self
                    .peek()
                    .filter(|&l| char::from_u32(l).is_some_and(|c| c.is_ascii_alphabetic()))?;
                self.pos += 1;
                l % 32
            }
            'x' => self.hex(2)?,
            'u' => self.unicode_escape()?,
            '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            | '/' => c,
            '-' if in_class => c,
            '&' | '!' | '#' | '%' | ',' | ':' | ';' | '<' | '=' | '>' | '@' | '`' | '~'
                if in_class =>
            {
                c
            }
            _ => return None,
        })
    }

    fn hex(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v * 16 + char::from_u32(self.peek()?)?.to_digit(16)?;
            self.pos += 1;
        }
        Some(v)
    }

    /// `\uXXXX`, a surrogate pair of them, or `\u{...}`.
    fn unicode_escape(&mut self) -> Option<u32> {
        if self.eat('{') {
            let start = self.pos;
            let mut v: u32 = 0;
            while let Some(d) = self.peek().and_then(|c| char::from_u32(c)?.to_digit(16)) {
                v = v.checked_mul(16)?.checked_add(d)?;
                self.pos += 1;
            }
            return (self.pos > start && self.eat('}') && v <= MAX_CHAR).then_some(v);
        }
        let hi = self.hex(4)?;
        if (0xD800..0xDC00).contains(&hi)
            && self.peek() == Some('\\' as u32)
            && self.peek_at(1) == Some('u' as u32)
        {
            let save = self.pos;
            self.pos += 2;
            match self.hex(4) {
                Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                    return Some(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                }
                _ => self.pos = save,
            }
        }
        Some(hi)
    }

    /// A class after its `[`, through its `]`.
    fn class(&mut self) -> Option<Node> {
        let negated = self.eat('^');
        let set = self.class_contents()?;
        if negated {
            return Some(class_node(set.complement()?));
        }
        Some(class_node(set))
    }

    /// The v flag's ClassContents: a union with ranges, or operands joined
    /// all by `&&` or all by `--`. Mixing them needs nested classes.
    fn class_contents(&mut self) -> Option<Set> {
        if self.eat(']') {
            return Some(Set::default());
        }
        let first = self.class_operand()?;
        if self.eat_str("&&") {
            let mut set = first.set();
            loop {
                // ClassIntersection: && [lookahead ≠ &] operand.
                if self.peek() == Some('&' as u32) {
                    return None;
                }
                set = set.intersect(self.class_operand()?.set());
                if self.eat(']') {
                    return Some(set);
                }
                if !self.eat_str("&&") {
                    return None;
                }
            }
        }
        if self.eat_str("--") {
            let mut set = first.set();
            loop {
                set = set.subtract(self.class_operand()?.set());
                if self.eat(']') {
                    return Some(set);
                }
                if !self.eat_str("--") {
                    return None;
                }
            }
        }
        let mut set = Set::default();
        let mut operand = first;
        loop {
            if let Operand::Char(lo) = operand
                && self.peek() == Some('-' as u32)
                && self.peek_at(1) != Some('-' as u32)
            {
                self.pos += 1;
                let Operand::Char(hi) = self.class_operand()? else {
                    return None;
                };
                if hi < lo {
                    return None;
                }
                set = set.union(Set::of(&[(lo, hi)]));
            } else {
                set = set.union(operand.set());
            }
            if self.eat(']') {
                return Some(set);
            }
            if self.peek().is_none()
                || self.src[self.pos..].starts_with(&[0x26, 0x26])
                || self.src[self.pos..].starts_with(&[0x2D, 0x2D])
            {
                return None;
            }
            operand = self.class_operand()?;
        }
    }

    fn class_operand(&mut self) -> Option<Operand> {
        let c = self.peek()?;
        self.pos += 1;
        match char::from_u32(c)? {
            '[' => {
                let negated = self.eat('^');
                let set = self.nested(Self::class_contents)?;
                Some(Operand::Set(if negated { set.complement()? } else { set }))
            }
            '\\' => {
                if self.eat('q') {
                    return self.class_strings().map(Operand::Set);
                }
                if let Some(set) = self.class_escape()? {
                    return Some(Operand::Set(set));
                }
                self.char_escape(true).map(Operand::Char)
            }
            '(' | ')' | ']' | '{' | '}' | '/' | '-' | '|' => None,
            _ => {
                // The v flag reserves doubled punctuators such as && and !!.
                let doubled = "&!#$%*+,.:;<=>?@^`~".contains(char::from_u32(c)?);
                if doubled && self.peek() == Some(c) {
                    return None;
                }
                Some(Operand::Char(c))
            }
        }
    }

    /// `\q{abc|d|}` after its `\q`: strings, each matched as a whole.
    fn class_strings(&mut self) -> Option<Set> {
        if !self.eat('{') {
            return None;
        }
        let mut set = Set::default();
        let mut cur = Vec::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match char::from_u32(c)? {
                '}' | '|' => {
                    if cur.len() == 1 {
                        set = set.union(Set::of(&[(cur[0], cur[0])]));
                    } else {
                        set.strings.insert(std::mem::take(&mut cur));
                    }
                    cur.clear();
                    if c == '}' as u32 {
                        return Some(set);
                    }
                }
                '\\' => cur.push(self.char_escape(true)?),
                '(' | ')' | '[' | ']' | '{' | '/' | '-' => return None,
                _ => cur.push(c),
            }
        }
    }
}

enum Operand {
    Char(u32),
    Set(Set),
}

impl Operand {
    fn set(self) -> Set {
        match self {
            Operand::Char(c) => Set::of(&[(c, c)]),
            Operand::Set(s) => s,
        }
    }
}

/// A group name: letters, digits, `_` and `$`, not starting with a digit.
fn valid_group_name(name: &[u32]) -> bool {
    let chars: Option<Vec<char>> = name.iter().map(|&c| char::from_u32(c)).collect();
    let Some(chars) = chars else { return false };
    chars
        .first()
        .is_some_and(|&c| c.is_alphabetic() || c == '_' || c == '$')
        && chars
            .iter()
            .all(|&c| c.is_alphanumeric() || c == '_' || c == '$')
}

type Caps = Vec<Option<(usize, usize)>>;

struct Matcher<'a> {
    text: &'a [u32],
    steps: Cell<u32>,
    budget: &'a Cell<u32>,
    /// Where the stack was when the match began.
    stack_top: usize,
}

/// The address of a local in the caller's frame, to see how far the stack
/// has grown between two calls. Either direction of growth works.
#[inline(always)]
fn stack_address() -> usize {
    let here = 0u8;
    std::hint::black_box(std::ptr::addr_of!(here)) as usize
}

impl Matcher<'_> {
    /// Whether `node` matches at `pos` with a continuation `k` that accepts.
    fn run(
        &self,
        node: &Node,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        if !self.step() || self.stack_top.abs_diff(stack_address()) > MATCH_STACK_LIMIT {
            return false;
        }
        self.run_node(node, pos, caps, k)
    }

    /// Takes one step from this match's limit and the select's budget.
    fn step(&self) -> bool {
        let steps = self.steps.get() + 1;
        self.steps.set(steps);
        let left = self.budget.get();
        if steps > STEP_LIMIT || left == 0 {
            return false;
        }
        self.budget.set(left - 1);
        true
    }

    fn run_node(
        &self,
        node: &Node,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        let text = self.text;
        match node {
            Node::Empty => k(pos, caps),
            Node::Char(c) => text.get(pos) == Some(c) && k(pos + 1, caps),
            Node::Any => text.get(pos).is_some_and(|&c| !is_line_terminator(c)) && k(pos + 1, caps),
            Node::Class(set, strings) => {
                // Strings first, longest first, then one code point.
                for s in strings {
                    if text[pos..].starts_with(s) && k(pos + s.len(), caps) {
                        return true;
                    }
                }
                text.get(pos).is_some_and(|&c| set.contains(c)) && k(pos + 1, caps)
            }
            Node::Start => pos == 0 && k(pos, caps),
            Node::End => pos == text.len() && k(pos, caps),
            Node::WordBoundary(want) => {
                let before = pos > 0 && is_word(text[pos - 1]);
                let after = text.get(pos).is_some_and(|&c| is_word(c));
                (before != after) == *want && k(pos, caps)
            }
            Node::Group(inner, n) => match n {
                None => self.run(inner, pos, caps, k),
                Some(n) => {
                    let n = *n;
                    self.run(inner, pos, caps, &mut |end, caps: &mut Caps| {
                        let old = caps[n];
                        caps[n] = Some((pos, end));
                        if k(end, caps) {
                            return true;
                        }
                        caps[n] = old;
                        false
                    })
                }
            },
            Node::Concat(nodes) => self.seq(nodes, pos, caps, k),
            Node::Alt(alts) => alts.iter().any(|a| self.run(a, pos, caps, k)),
            Node::Repeat {
                node,
                min,
                max,
                greedy,
                groups,
            } => match one_char(node) {
                Some(fits) => self.repeat_chars(&fits, *min, *max, *greedy, pos, caps, k),
                None => self.repeat(node, *min, *max, *greedy, *groups, 0, pos, caps, k),
            },
            Node::Look {
                node,
                ahead,
                negate,
            } => {
                // Lookarounds are atomic: no backtracking into them.
                let mut found: Option<Caps> = None;
                let mut try_from = |start: usize, need_end: Option<usize>, caps: &Caps| {
                    let mut c = caps.clone();
                    self.run(node, start, &mut c, &mut |end, c: &mut Caps| {
                        if need_end.is_some_and(|e| e != end) {
                            return false;
                        }
                        found = Some(c.clone());
                        true
                    })
                };
                let matched = if *ahead {
                    try_from(pos, None, caps)
                } else {
                    // shortcut: a lookbehind is tried forwards from each earlier start, not matched backwards as JavaScript does; only captures and backreferences inside one can differ.
                    (0..=pos).rev().any(|s| try_from(s, Some(pos), caps))
                };
                if *negate {
                    !matched && k(pos, caps)
                } else if let Some(mut c) = found.filter(|_| matched) {
                    k(pos, &mut c)
                } else {
                    false
                }
            }
            Node::BackRef(n) => match caps.get(*n).copied().flatten() {
                // A group that hasn't matched matches the empty string.
                None => k(pos, caps),
                Some((s, e)) => {
                    let len = e - s;
                    text.len() >= pos + len
                        && text[pos..pos + len] == text[s..e]
                        && k(pos + len, caps)
                }
            },
        }
    }

    fn seq(
        &self,
        nodes: &[Node],
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        match nodes.split_first() {
            None => k(pos, caps),
            Some((first, rest)) => self.run(first, pos, caps, &mut |p, c: &mut Caps| {
                self.seq(rest, p, c, k)
            }),
        }
    }

    /// A repeat of something that matches exactly one character, as a loop:
    /// it can't capture or match empty, so trying each count from the most
    /// (or, lazily, the fewest) is the same as recursing, without a stack
    /// frame per character.
    #[allow(clippy::too_many_arguments)]
    fn repeat_chars(
        &self,
        fits: &dyn Fn(u32) -> bool,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        let limit = max.map_or(usize::MAX, |m| m as usize);
        let mut most = 0;
        while most < limit && self.text.get(pos + most).is_some_and(|&c| fits(c)) {
            if !self.step() {
                return false;
            }
            most += 1;
        }
        let min = min as usize;
        if most < min {
            return false;
        }
        if greedy {
            (min..=most).rev().any(|n| self.step() && k(pos + n, caps))
        } else {
            (min..=most).any(|n| self.step() && k(pos + n, caps))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        groups: (usize, usize),
        count: u32,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        if max == Some(0) && count == 0 {
            return k(pos, caps);
        }
        let again = |caps: &mut Caps, k: &mut dyn FnMut(usize, &mut Caps) -> bool| {
            if max.is_some_and(|m| count >= m) {
                return false;
            }
            let saved: Vec<_> = caps[groups.0..groups.1].to_vec();
            for c in &mut caps[groups.0..groups.1] {
                *c = None;
            }
            let ok = self.run(node, pos, caps, &mut |p, c: &mut Caps| {
                // An iteration past the minimum that matched nothing stops
                // the loop, as JavaScript's RepeatMatcher does.
                if p == pos && count >= min {
                    return false;
                }
                self.repeat(node, min, max, greedy, groups, count + 1, p, c, k)
            });
            if !ok {
                caps[groups.0..groups.1].copy_from_slice(&saved);
            }
            ok
        };
        if count < min {
            return again(caps, k);
        }
        // Greedy tries one more first; lazy tries stopping first.
        if greedy {
            return again(caps, k) || k(pos, caps);
        }
        k(pos, caps) || again(caps, k)
    }
}

/// What a node that always matches exactly one character accepts, or None
/// for any other node.
fn one_char(node: &Node) -> Option<Box<dyn Fn(u32) -> bool + '_>> {
    match node {
        Node::Char(c) => Some(Box::new(move |x| x == *c)),
        Node::Any => Some(Box::new(|x| !is_line_terminator(x))),
        Node::Class(set, strings) if strings.is_empty() => Some(Box::new(move |x| set.contains(x))),
        Node::Group(inner, None) => one_char(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    fn m(pattern: &str, value: &str) -> bool {
        let budget = std::cell::Cell::new(super::SELECT_STEP_LIMIT);
        Pattern::compile(pattern)
            .unwrap_or_else(|| panic!("{pattern} doesn't compile"))
            .matches(value, &budget)
    }

    #[test]
    fn matches_the_whole_value() {
        assert!(m(r"\d{3}", "123"));
        assert!(!m(r"\d{3}", "1234"));
        assert!(!m(r"\d{3}", "12"));
        assert!(m("[a-z]+", "abc"));
        assert!(!m("[a-z]+", "abC"));
        assert!(m("a|bc", "bc"));
        assert!(!m("a|bc", "abc"), "the alternation is inside the anchors");
        assert!(m("ab$|c", "c"));
        assert!(m("", ""));
    }

    #[test]
    fn escapes_are_javascripts() {
        assert!(!m(r"\d", "٣"), "\\d is ASCII digits only");
        assert!(!m(r"\w", "é"));
        assert!(m(r"\s", "\u{3000}"));
        assert!(m(r"\u{1F600}", "😀"));
        assert!(m(r"😀", "😀"));
        assert!(m(r"\x41B\cJ", "AB\n"));
        assert!(!m(".", "\n"));
        assert!(m(".", "😀"), "the v flag matches by code point");
    }

    #[test]
    fn groups_quantifiers_and_lookaround() {
        assert!(m(r"(a+)b\1", "aabaa"));
        assert!(!m(r"(a+)b\1", "aaba"));
        assert!(m(r"(?<x>[0-9])-\k<x>", "7-7"));
        assert!(m("(?:ab){2,3}", "ababab"));
        assert!(!m("(?:ab){2,3}", "ab"));
        assert!(m("a+?b", "aaab"));
        assert!(m("(?=.*\\d)[a-z0-9]{4}", "ab1c"));
        assert!(!m("(?=.*\\d)[a-z0-9]{4}", "abcd"));
        assert!(m("(?!x).+", "abc"));
        assert!(m("[a-z]+(?<=c)", "abc"));
        assert!(!m("[a-z]+(?<!c)", "abc"));
        assert!(m(r"\bab\b", "ab"));
        assert!(
            m("(a*)*", "aaa"),
            "an empty iteration ends the loop instead of spinning"
        );
    }

    #[test]
    fn v_flag_classes() {
        assert!(m(r"[\w--\d]+", "abc_"));
        assert!(!m(r"[\w--\d]+", "ab1"));
        assert!(m("[[a-z]&&[^aeiou]]+", "bcd"));
        assert!(!m("[[a-z]&&[^aeiou]]+", "bad"));
        assert!(m(r"[\q{abc|d}x]", "abc"));
        assert!(m(r"[\q{abc|d}x]+", "dxabc"));
        assert!(m("[^]", "x"));
        assert!(!m("[]", "x"));
        assert!(m(r"[\-a]", "-"));
    }

    #[test]
    fn what_doesnt_compile_under_the_v_flag() {
        for bad in [
            "(",
            ")",
            "[",
            "a{",
            "}",
            "]",
            "*a",
            "a**",
            r"\p{L}",
            r"\P{L}",
            r"\q{a}",
            r"\e",
            "[a-]",
            "[(]",
            "[a&&&b]",
            "[a&&b--c]",
            "[^\\q{ab}]",
            r"\2(a)",
            r"\k<y>(?<x>a)",
            "(?<x>a)(?<x>b)",
            "(?=a)*",
            "[z-a]",
            "a{3,1}",
            "(?i:a)",
            "[a--]",
        ] {
            assert!(Pattern::compile(bad).is_none(), "{bad} compiled");
        }
    }

    #[test]
    fn runaway_backtracking_gives_up() {
        let p = Pattern::compile("(a+)+b").unwrap();
        let budget = std::cell::Cell::new(super::SELECT_STEP_LIMIT);
        assert!(!p.matches(&"a".repeat(40), &budget));
    }

    /// Runs `f` on a thread with a small stack, as a Python worker's can
    /// be, so a frame per character or per nesting level fails here rather
    /// than passing by luck on a big main-thread stack.
    fn on_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_nesting_doesnt_compile() {
        on_small_stack(|| {
            assert!(
                Pattern::compile(&format!("{}a{}", "(".repeat(10_000), ")".repeat(10_000)))
                    .is_none()
            );
            assert!(
                Pattern::compile(&format!("{}a{}", "[".repeat(10_000), "]".repeat(10_000)))
                    .is_none()
            );
            // 32 deep still compiles.
            assert!(m(&format!("{}a{}", "(".repeat(32), ")".repeat(32)), "a"));
            assert!(m(&format!("{}a{}", "[".repeat(32), "]".repeat(32)), "a"));
        });
    }

    #[test]
    fn long_values_dont_recurse_per_character() {
        on_small_stack(|| {
            let long = "a".repeat(100_000);
            assert!(m("a*", &long));
            assert!(m("[a-z]+", &long));
            assert!(m(".*?", &long));
            assert!(m("(?:a)*", &long));
            assert!(!m("a*b", &long));
            assert!(m("a{3,5}b", "aaaab"));
            assert!(!m("a{3,5}", "aaaaaa"));
            // A repeat that captures recurses, and gives up past the stack
            // limit instead of overflowing.
            assert!(!m("(a)*", &long));
            assert!(m("(a)*", "aaaa"));
        });
    }

    #[test]
    fn a_select_shares_one_budget() {
        let p = Pattern::compile("(a+)+b").unwrap();
        let budget = std::cell::Cell::new(super::SELECT_STEP_LIMIT);
        let value = "a".repeat(40);
        for _ in 0..20 {
            assert!(!p.matches(&value, &budget));
        }
        assert_eq!(budget.get(), 0);
        // Once it's spent, even an easy match gives up.
        assert!(!Pattern::compile("a").unwrap().matches("a", &budget));
    }
}
