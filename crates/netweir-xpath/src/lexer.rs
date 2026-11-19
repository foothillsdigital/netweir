//! Tokens of XPath 1.0 (section 3.7), including the rule that decides
//! whether `*` and `and`/`or`/`mod`/`div` are operators or names.

use crate::XPathError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    At,
    Comma,
    ColonColon,
    Slash,
    DoubleSlash,
    Pipe,
    Plus,
    Minus,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    /// `*` as multiplication.
    Multiply,
    And,
    Or,
    Mod,
    Div,
    Literal(String),
    Number(f64),
    Variable(String),
    /// `*`, `prefix:*`, `name` or `prefix:name` as a name test.
    Name(String),
}

impl Token {
    /// How the token looks in the query, for error messages.
    pub(crate) fn describe(&self) -> String {
        let text = match self {
            Token::LParen => "(",
            Token::RParen => ")",
            Token::LBracket => "[",
            Token::RBracket => "]",
            Token::Dot => ".",
            Token::DotDot => "..",
            Token::At => "@",
            Token::Comma => ",",
            Token::ColonColon => "::",
            Token::Slash => "/",
            Token::DoubleSlash => "//",
            Token::Pipe => "|",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Eq => "=",
            Token::Neq => "!=",
            Token::Lt => "<",
            Token::Le => "<=",
            Token::Gt => ">",
            Token::Ge => ">=",
            Token::Multiply => "*",
            Token::And => "and",
            Token::Or => "or",
            Token::Mod => "mod",
            Token::Div => "div",
            Token::Literal(s) => return format!("the string {s:?}"),
            Token::Number(n) => return format!("the number {n}"),
            Token::Variable(v) => return format!("${v}"),
            Token::Name(n) => return format!("{n:?}"),
        };
        format!("{text:?}")
    }

    /// After these, `*` and operator names are names, not operators.
    fn starts_operand(&self) -> bool {
        matches!(
            self,
            Token::At
                | Token::ColonColon
                | Token::LParen
                | Token::LBracket
                | Token::Comma
                | Token::Slash
                | Token::DoubleSlash
                | Token::Pipe
                | Token::Plus
                | Token::Minus
                | Token::Eq
                | Token::Neq
                | Token::Lt
                | Token::Le
                | Token::Gt
                | Token::Ge
                | Token::Multiply
                | Token::And
                | Token::Or
                | Token::Mod
                | Token::Div
        )
    }
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{b7}')
}

pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, XPathError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    let err = |reason: String| XPathError {
        query: src.to_string(),
        reason,
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let operator_context = out.last().is_some_and(|t| !t.starts_operand());
        let next = chars.get(i + 1).copied();
        let token = match c {
            '(' => Token::LParen,
            ')' => Token::RParen,
            '[' => Token::LBracket,
            ']' => Token::RBracket,
            '@' => Token::At,
            ',' => Token::Comma,
            '|' => Token::Pipe,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '=' => Token::Eq,
            '!' if next == Some('=') => {
                i += 1;
                Token::Neq
            }
            '<' if next == Some('=') => {
                i += 1;
                Token::Le
            }
            '<' => Token::Lt,
            '>' if next == Some('=') => {
                i += 1;
                Token::Ge
            }
            '>' => Token::Gt,
            ':' if next == Some(':') => {
                i += 1;
                Token::ColonColon
            }
            '/' if next == Some('/') => {
                i += 1;
                Token::DoubleSlash
            }
            '/' => Token::Slash,
            '.' if next == Some('.') => {
                i += 1;
                Token::DotDot
            }
            '.' if next.is_some_and(|n| n.is_ascii_digit()) => {
                let start = i;
                i += 1;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                out.push(Token::Number(
                    text.parse()
                        .map_err(|_| err(format!("bad number {text}")))?,
                ));
                continue;
            }
            '.' => Token::Dot,
            '*' if operator_context => Token::Multiply,
            '*' => Token::Name("*".into()),
            '"' | '\'' => {
                let end = chars[i + 1..]
                    .iter()
                    .position(|&q| q == c)
                    .ok_or_else(|| err("unclosed string literal".into()))?;
                let text: String = chars[i + 1..i + 1 + end].iter().collect();
                i += end + 2;
                out.push(Token::Literal(text));
                continue;
            }
            '$' => {
                let start = i + 1;
                let name = read_qname(&chars, start);
                if name.is_empty() {
                    return Err(err("$ must be followed by a variable name".into()));
                }
                i = start + name.chars().count();
                out.push(Token::Variable(name));
                continue;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                if chars.get(i) == Some(&'.') {
                    i += 1;
                    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1;
                    }
                }
                let text: String = chars[start..i].iter().collect();
                out.push(Token::Number(
                    text.parse()
                        .map_err(|_| err(format!("bad number {text}")))?,
                ));
                continue;
            }
            c if is_name_start(c) => {
                let name = read_qname(&chars, i);
                i += name.chars().count();
                let token = if operator_context {
                    match name.as_str() {
                        "and" => Token::And,
                        "or" => Token::Or,
                        "mod" => Token::Mod,
                        "div" => Token::Div,
                        _ => return Err(err(format!("expected an operator, found {name:?}"))),
                    }
                } else {
                    Token::Name(name)
                };
                out.push(token);
                continue;
            }
            other => return Err(err(format!("unexpected character {other:?}"))),
        };
        out.push(token);
        i += 1;
    }
    Ok(out)
}

/// `name`, `prefix:name` or `prefix:*` starting at `i`; empty if there is
/// no name there. A `::` after the name is left alone (it's an axis).
fn read_qname(chars: &[char], i: usize) -> String {
    let mut j = i;
    if !chars.get(j).is_some_and(|&c| is_name_start(c)) {
        return String::new();
    }
    while chars.get(j).is_some_and(|&c| is_name_char(c)) {
        j += 1;
    }
    if chars.get(j) == Some(&':') && chars.get(j + 1) != Some(&':') {
        match chars.get(j + 1) {
            Some('*') => j += 2,
            Some(&c) if is_name_start(c) => {
                j += 1;
                while chars.get(j).is_some_and(|&c| is_name_char(c)) {
                    j += 1;
                }
            }
            _ => {}
        }
    }
    chars[i..j].iter().collect()
}
