//! The lexer of the configuration language, which only reads text and knows nothing of a project.

use super::value::Data;

#[derive(Clone, Copy)]
pub struct Span {
    pub file: usize,
    pub offset: usize,
    pub len: usize,
}

pub(super) struct Parser<'a> {
    pub(super) file: usize,
    pub(super) data: &'a str,
    pub(super) offset: usize,
}

impl <'a> Parser<'a> {
    pub fn new(file: usize, data: &'a str) -> Self {
        Self {
            file,
            data,
            offset: 0,
        }
    }
    pub fn as_str(&self) -> &'a str {
        &self.data[self.offset..]
    }
    pub fn as_bytes(&self) -> &'a [u8] {
        &self.data.as_bytes()[self.offset..]
    }
    pub fn is_empty(&self) -> bool {
        self.offset >= self.data.len()
    }
    pub fn peek(&mut self) -> Option<char> {
        self.as_str().chars().next()
    }
    pub fn expect(&mut self, c: char) -> bool {
        match self.as_str().chars().next() {
            None => false,
            Some(other) if other != c => false,
            Some(other) => {
                self.offset += other.len_utf8();
                true
            },
        }
    }
    pub fn skip_empty(&mut self) {
        while let Some(b) = self.as_bytes().first() {
            match b {
                b'#' => {
                    self.skip_statement();
                },
                b'\n' => self.offset += 1,
                _ => return,
            }
        }
    }
    pub fn skip_line(&mut self) -> Span {
        let start = self.offset;
        match self.as_str().find('\n') {
            Some(end) => self.offset += end + 1,
            None => self.offset = self.data.len(),
        }
        Span {
            file: self.file,
            offset: start,
            len: self.offset - start,
        }
    }
    pub fn skip_statement(&mut self) -> Span {
        let offset = self.offset;
        while self.offset < self.data.len() {
            // continue from the next non-indented line
            if let Some(end) = self.as_str().find('\n') {
                self.offset += end + 1;
            } else {
                self.offset = self.data.len();
            }
            match self.as_bytes().first() {
                Some(b' ') => (),
                _ => break,
            }
        }
        Span {
            file: self.file,
            offset,
            len: self.offset - offset,
        }
    }
    /// Take the indentation of an attribute, skipping comments at that indentation, which comment out an attribute.
    pub fn indent(&mut self) -> Option<Span> {
        while self.as_str().starts_with("    #") {
            self.skip_line();
        }
        if self.as_str().starts_with("    ") {
            let offset = self.offset;
            self.offset += 4;
            Some(Span {
                file: self.file,
                offset,
                len: 4,
            })
        } else {
            None
        }
    }
    pub fn ident(&mut self) -> Option<(&'a str, Span)> {
        let data = self.data.as_bytes();
        let start = self.offset;
        let mut i = self.offset;
        while i < data.len() {
            match data[i] {
                b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' => i += 1,
                // a leading digit would be an integer
                b'0'..=b'9' if i > start => i += 1,
                _ => break,
            }
        }
        if i == self.offset {
            return None;
        }
        let ident = &self.data[self.offset..i];
        self.offset = i;
        Some((ident, Span { file: self.file, offset: start, len: ident.len() }))
    }
    /// An operator with a single space on either side.
    ///
    /// The operator is only taken if its precedence is at least `min_precedence`.
    pub fn operator(&mut self, min_precedence: u8) -> Option<Operator> {
        let rest = self.as_str().strip_prefix(' ')?;
        let end = rest.find(' ')?;
        let operator = match &rest[..end] {
            "||" => Operator::Or,
            "&&" => Operator::And,
            "==" => Operator::Equal,
            "!=" => Operator::NotEqual,
            "<" => Operator::Less,
            "<=" => Operator::LessEqual,
            ">" => Operator::Greater,
            ">=" => Operator::GreaterEqual,
            "+" => Operator::Add,
            "-" => Operator::Subtract,
            "*" => Operator::Multiply,
            "/" => Operator::Divide,
            "%" => Operator::Remainder,
            "|" => Operator::BitOr,
            "^" => Operator::BitXor,
            "&" => Operator::BitAnd,
            "<<" => Operator::ShiftLeft,
            ">>" => Operator::ShiftRight,
            _ => return None,
        };
        if operator.precedence() < min_precedence {
            return None;
        }
        self.offset += end + 2;
        Some(operator)
    }
    /// An integer with an optional `-` or `+` sign.
    ///
    /// The prefixes `0x`, `0o` and `0b` select hexadecimal, octal and binary, otherwise it is decimal.
    /// Digits may be separated with `_`, but the first character must be a digit.
    /// Returns [`Data::Signed`] if the sign is `-` and [`Data::Unsigned`] otherwise.
    pub fn integer(&mut self) -> Option<(Data, Span)> {
        let s = self.as_str();
        let (negative, unsigned) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (radix, digits) = if let Some(rest) = unsigned.strip_prefix("0x") {
            (16, rest)
        } else if let Some(rest) = unsigned.strip_prefix("0o") {
            (8, rest)
        } else if let Some(rest) = unsigned.strip_prefix("0b") {
            (2, rest)
        } else {
            (10, unsigned)
        };
        let end = digits.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(digits.len());
        if digits.starts_with('_') {
            return None;
        }
        let magnitude = u64::from_str_radix(&digits[..end].replace('_', ""), radix).ok()?;
        let data = if negative {
            Data::Signed(0i64.checked_sub_unsigned(magnitude)?)
        } else {
            Data::Unsigned(magnitude)
        };
        let start = self.offset;
        self.offset += s.len() - digits.len() + end;
        Some((data, self.span_from(start)))
    }
    /// A `'` quoted string on a single line.
    pub fn string(&mut self) -> Option<(&'a str, Span)> {
        let data = self.as_str();
        let rest = data.strip_prefix('\'')?;
        let end = string_end(rest)?;
        let start = self.offset;
        self.offset += end + 2;
        Some((&rest[..end], self.span_from(start)))
    }
    pub fn span_char(&self) -> Span {
        Span {
            file: self.file,
            offset: self.offset,
            len: self.as_str().chars().next().map(char::len_utf8).unwrap_or_default(),
        }
    }
    /// Returns the span from `start` to the current offset.
    pub fn span_from(&self, start: usize) -> Span {
        Span {
            file: self.file,
            offset: start,
            len: self.offset - start,
        }
    }
    pub fn span_until(&self, pat: impl Fn(char) -> bool) -> Span {
        let len = self.as_str().find(pat).unwrap_or(self.data.len() - self.offset);
        Span {
            file: self.file,
            offset: self.offset,
            len,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Operator {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    BitOr,
    BitXor,
    BitAnd,
    ShiftLeft,
    ShiftRight,
}
impl Operator {
    /// The precedence of the operator, which is the same as in Rust.
    pub(super) const fn precedence(self) -> u8 {
        match self {
            Self::Or => 0,
            Self::And => 1,
            Self::Equal | Self::NotEqual | Self::Less | Self::LessEqual | Self::Greater | Self::GreaterEqual => 2,
            Self::BitOr => 3,
            Self::BitXor => 4,
            Self::BitAnd => 5,
            Self::ShiftLeft | Self::ShiftRight => 6,
            Self::Add | Self::Subtract => 7,
            Self::Multiply | Self::Divide | Self::Remainder => 8,
        }
    }
}

/// Returns the index of the `'` which closes a string, given the text after the opening `'`.
///
/// A string is on a single line. An expression in `{}` within it may contain strings, so their quotes do not close it,
/// while `{{` and `}}` are escaped braces. An unclosed `{` is ignored, so that it is reported as an invalid template.
fn string_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None,
            b'\'' => return Some(i),
            b'{' | b'}' if bytes.get(i + 1) == Some(&bytes[i]) => i += 1,
            b'{' => {
                if let Some(end) = expression_end(&s[i + 1..]) {
                    i += 1 + end;
                }
            },
            _ => (),
        }
        i += 1;
    }
    None
}

/// Returns the index of the `}` which closes an expression in a string template, given the text after the `{`.
///
/// The expression may itself contain `{}` to coerce a value, and strings, whose braces are not counted.
pub(super) fn expression_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None,
            b'\'' => i += 1 + string_end(&s[i + 1..])?,
            b'{' => depth += 1,
            b'}' if depth == 0 => return Some(i),
            b'}' => depth -= 1,
            _ => (),
        }
        i += 1;
    }
    None
}

/// Returns the index of the end of an argument, at the `,` or `)` after it, given the text from its start.
///
/// Brackets and strings within the argument are skipped, and the argument must end on the same line.
pub(super) fn argument_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None,
            b'\'' => i += 1 + string_end(&s[i + 1..])?,
            b'(' | b'[' | b'{' => depth += 1,
            b',' | b')' if depth == 0 => return Some(i),
            b')' | b']' | b'}' if depth > 0 => depth -= 1,
            b']' | b'}' => return None,
            _ => (),
        }
        i += 1;
    }
    None
}

/// Returns whether a character is part of a word in a name, including those which are reported as invalid in one.
pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Returns whether the parser is at more of a name, after some of it: a word, an expression in `{}`, or a `/` before
/// either, which division cannot be confused with, as an operator must have spaces around it.
pub(super) fn is_name_continuation(parser: &Parser) -> bool {
    let mut chars = parser.as_str().chars();
    match chars.next() {
        Some('/') => chars.next().is_some_and(|c| is_word_char(c) || c == '{'),
        Some(c) => is_word_char(c) || c == '{',
        None => false,
    }
}

/// Returns the length of the name at the start of `s`, which is words and expressions in `{}`, joined by `/`.
fn name_len(s: &str) -> usize {
    let mut len = 0;
    loop {
        let rest = &s[len..];
        if let Some(inner) = rest.strip_prefix('{') {
            let Some(end) = expression_end(inner) else {
                return len;
            };
            len += end + 2;
            continue;
        }
        if len > 0 && is_name_continuation(&Parser::new(0, rest)) && rest.starts_with('/') {
            len += 1;
            continue;
        }
        let word = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
        if word == 0 {
            return len;
        }
        len += word;
    }
}

/// Returns whether the parser is at a name followed by `: `, as in an entry of a dict or an environment variable of
/// `run`.
pub(super) fn is_named_entry(parser: &Parser) -> bool {
    let rest = parser.as_str();
    let len = name_len(rest);
    len > 0 && rest[len..].starts_with(": ")
}

/// Returns whether the parser is at the `.` of an attribute of what is before it.
pub(super) fn is_attribute_access(parser: &Parser) -> bool {
    let mut chars = parser.as_str().chars();
    chars.next() == Some('.') && chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// Returns the first character of a name which cannot be part of it, with its index, where a name is identifiers joined
/// by `/`, which may have a `.` where they are computed.
pub(super) fn invalid_name_char(name: &str) -> Option<(usize, char)> {
    let mut segment_start = 0;
    for (i, c) in name.char_indices() {
        if c == '/' {
            segment_start = i + 1;
            continue;
        }
        // a `.` can only be in a part computed in `{}`, as one written in a name takes an attribute, as some targets
        // have one, such as `thumbv8m.main-none-eabihf`
        if c == '.' {
            continue;
        }
        if invalid_ident_char(&(i - segment_start, c)) {
            return Some((i, c));
        }
    }
    None
}

/// Returns whether a character, at index `i`, cannot be part of an identifier.
pub(super) fn invalid_ident_char(&(i, c): &(usize, char)) -> bool {
    match c {
        'a'..='z' | 'A'..='Z' | '-' | '_' => false,
        // a leading digit would be an integer
        '0'..='9' => i == 0,
        _ => true,
    }
}
