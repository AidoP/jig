//! Statements, and the parts of them which report what is wrong when they cannot be taken.

use std::path::PathBuf;

use super::diagnostic::{Diagnostic, Message, Severity};
use super::meta::Meta;
use super::syntax::{Parser, Span, invalid_ident_char, invalid_name_char, is_name_continuation, is_word_char};
use super::value::{Coercion, Data, Operand};
use super::{File, Project};

pub(super) struct Attribute<'a> {
    pub(super) name: &'a str,
    pub(super) name_span: Span,
    pub(super) data: Data,
    pub(super) data_span: Span,
    /// Where the value was set, if the data is the value of a property.
    pub(super) origin: Option<Span>,
    /// How far the value may be converted, which is further for an expression in `{}`.
    pub(super) coerce: Coercion,
}
/// The name of a property or crate, as taken by [`Project::parse_name`].
pub(super) struct Name {
    pub(super) name: String,
    pub(super) span: Span,
    /// Whether the name was written as an identifier, so that its characters are at known offsets.
    pub(super) literal: bool,
}

impl Attribute<'_> {
    /// The value of the attribute as an operand, which may be converted as far as its expression permits.
    pub(super) fn operand(self) -> Operand {
        Operand { data: self.data, span: self.data_span, coerce: self.coerce, origin: self.origin }
    }
}

impl Project {
    pub(super) fn parse_file(&mut self, idx: usize) {
        let data = std::mem::take(&mut self.files[idx].data);
        let mut parser = Parser::new(idx, &data);
        let mut chain = None;
        // the statement which used the file is still being processed after it
        let meta_insertion = self.meta_insertion.take();
        loop {
            parser.skip_empty();
            if parser.is_empty() {
                break;
            }
            self.parse_statement(&mut parser, &mut chain);
        }
        self.files[idx].data = data;
        self.meta_insertion = meta_insertion;
    }
    /// Take a statement, with the meta-statements before it, and the statements chained after it with `@then`, which
    /// are processed or skipped with it.
    pub(super) fn parse_statement(&mut self, parser: &mut Parser, chain: &mut Option<bool>) {
        let process = match self.parse_meta(parser, chain) {
            Meta::Skip => false,
            // `@for` processes the chained statements too
            Meta::Done => return,
            Meta::Process => true,
        };
        if process {
            self.parse_single_statement(parser);
        } else {
            parser.skip_statement();
        }
        // the diagnostics of the statement have the severities set where it is
        self.override_severities();
        while self.parse_then(parser) {
            if process {
                self.parse_statement(parser, &mut None);
            } else {
                Self::skip_chained(parser);
            }
        }
    }
    /// Take a `@then` directly after a statement, returning whether there is one.
    fn parse_then(&mut self, parser: &mut Parser) -> bool {
        let Some(rest) = parser.as_str().strip_prefix("@then") else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with(['\n', ' ']) {
            return false;
        }
        let span = Span { file: parser.file, offset: parser.offset, len: 5 };
        parser.offset += 5;
        if !self.parse_line_end(parser) {
            parser.skip_line();
        }
        if matches!(parser.peek(), None | Some('\n' | '#')) {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::ExpectedStatementAfterMeta,
                severity: Severity::Error,
            });
            return false;
        }
        true
    }
    /// Skip a statement chained with `@then` to a skipped statement, with its meta-statements.
    fn skip_chained(parser: &mut Parser) {
        while parser.peek() == Some('@') {
            parser.skip_line();
        }
        parser.skip_statement();
    }
    /// Take a statement, after its meta-statements.
    fn parse_single_statement(&mut self, parser: &mut Parser) {
        let start = parser.offset;
        self.meta_insertion = Some(Span { file: parser.file, offset: start, len: 0 });
        let Some((keyword, span)) = self.parse_ident(parser) else {
            parser.skip_statement();
            return;
        };
        match keyword {
            "declare" => self.parse_declare(parser),
            "use" => self.parse_use(parser),
            "set" => self.parse_set(parser),
            "log" => self.parse_log(parser),
            "crate" => self.parse_crate(parser),
            "custom" => self.parse_custom(parser, false),
            "test" => self.parse_custom(parser, true),
            "target" => self.parse_target(parser),
            _ => {
                parser.offset = start;
                let _ = parser.skip_statement();
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::UnknownStatement(keyword.into()),
                    severity: Severity::Error,
                });
            },
        }
    }
    fn parse_use(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some((mut path, span)) = self.parse_path(parser) else {
            parser.skip_statement();
            return;
        };
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        // a directory is used by its root file, unless the path is the file
        let mut file = false;
        while let Some(attribute) = self.next_attribute(parser, &[]) {
            match attribute.name {
                "file" => file = true,
                name => self.unknown_attribute("use", name, attribute.name_span),
            }
        }
        if !file {
            path = path.join("Jig");
        }
        match File::read(path.clone()) {
            Ok(mut file) => {
                file.used = Some(span);
                // using a file again is fine unless it is still being processed, which is a cycle
                if let Some(idx) = self.parse(file) && self.files[idx].active {
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::RepeatedUse { path, file: idx },
                        severity: Severity::Warning,
                    });
                }
            },
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::ReadFailure(path, error),
                    severity: Severity::Error,
                });
            },
        };
    }
    fn parse_log(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some((severity, span)) = self.parse_ident(parser) else {
            parser.skip_statement();
            return;
        };
        let severity = match severity {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            "note" => Severity::Note,
            "suggestion" => Severity::Suggestion,
            _ => {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::UnknownSeverity(severity.into()),
                    severity: Severity::Error,
                });
                Severity::Error
            },
        };
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(operand) = self.parse_expression(parser) else {
            parser.skip_statement();
            return;
        };
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        // a quiet message is shown without where it was logged
        let mut quiet = false;
        // the identifier of the diagnostic, which is shown with it and whose property overrides its severity
        let mut id = None;
        while let Some(attribute) = self.next_attribute(parser, &[]) {
            match attribute.name {
                "quiet" => {
                    if let Some(value) = self.flag(attribute.operand()) {
                        quiet = value;
                    }
                },
                "id" => {
                    let span = attribute.data_span;
                    if let Some(name) = self.str_operand(attribute.operand()) && self.diagnostic_id(&name, span) {
                        self.declare_diagnostic_override(&name);
                        id = Some(name);
                    }
                },
                name => self.unknown_attribute("log", name, attribute.name_span),
            }
        }
        // any value can be logged, as it would be in a string template
        let message = operand.data.coerce_string();
        let message = match id {
            Some(id) => Message::Logged { id, message },
            None => Message::User(message),
        };
        self.diagnostics.push(Diagnostic { span: (!quiet).then_some(operand.span), message, severity });
    }
    /// Returns whether a diagnostic's identifier is valid, which is an identifier, as it is part of the name of its
    /// property, `diagnostic/<id>`, or reports why not.
    fn diagnostic_id(&mut self, id: &str, span: Span) -> bool {
        let message = match id.char_indices().find(invalid_ident_char) {
            _ if id.is_empty() => Message::ExpectedIdentifier,
            Some((_, character)) => Message::InvalidIdentifier { name: id.into(), character },
            None => return true,
        };
        self.diagnostics.push(Diagnostic { span: Some(span), message, severity: Severity::Error });
        false
    }
    fn unknown_attribute(&mut self, keyword: &'static str, name: &str, span: Span) {
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::UnknownAttribute { keyword, name: name.into() },
            severity: Severity::Error,
        });
    }
    /// Take the next identifier from the parser.
    ///
    /// If the parser is not pointing at an identifier, a diagnostic is created, naming the first invalid character if
    /// the parser is pointing at a word.
    pub(super) fn parse_ident<'a>(&mut self, parser: &mut Parser<'a>) -> Option<(&'a str, Span)> {
        let rest = parser.as_str();
        let word = &rest[..rest.find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-').unwrap_or(rest.len())];
        let start = parser.offset;
        if let Some((ident, span)) = parser.ident() && ident.len() == word.len() {
            return Some((ident, span));
        }
        parser.offset = start;
        let (span, message) = match word.char_indices().find(invalid_ident_char) {
            Some((i, character)) => (
                Span { file: parser.file, offset: start + i, len: character.len_utf8() },
                Message::InvalidIdentifier { name: word.into(), character },
            ),
            None => (parser.span_until(|c| c == ' ' || c == '\n'), Message::ExpectedIdentifier),
        };
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message,
            severity: Severity::Error,
        });
        None
    }
    /// Take the name of a property or crate, which is identifiers joined by `/`, any part of which may be computed by
    /// an expression in `{}`, as in `{target}/core` or `a-{b}`.
    pub(super) fn parse_name(&mut self, parser: &mut Parser) -> Option<Name> {
        let start = parser.offset;
        self.parse_name_from(parser, start, String::new(), true)
    }
    /// Take the rest of a name which starts at `start`, whose value so far is `name`, and which is `literal` if none of
    /// it has been computed.
    pub(super) fn parse_name_from(&mut self, parser: &mut Parser, start: usize, mut name: String, mut literal: bool) -> Option<Name> {
        loop {
            if parser.peek() == Some('{') {
                let operand = self.parse_braced(parser)?;
                name.push_str(&operand.data.coerce_string());
                literal = false;
                continue;
            }
            if parser.offset > start && parser.peek() == Some('/') && is_name_continuation(parser) {
                parser.expect('/');
                name.push('/');
                continue;
            }
            let rest = parser.as_str();
            let len = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
            if len == 0 {
                break;
            }
            name.push_str(&rest[..len]);
            parser.offset += len;
        }
        let span = parser.span_from(start);
        let (span, message) = match invalid_name_char(&name) {
            // point at the character if the name was written as it is, otherwise its position is unknown
            Some((i, character)) if literal => (
                Span { file: parser.file, offset: start + i, len: character.len_utf8() },
                Message::InvalidIdentifier { name, character },
            ),
            Some((_, character)) => (span, Message::InvalidIdentifier { name, character }),
            None if parser.offset == start => (parser.span_until(|c| c == ' ' || c == '\n'), Message::ExpectedIdentifier),
            None if name.split('/').any(str::is_empty) => (span, Message::ExpectedIdentifier),
            None => return Some(Name { name, span, literal }),
        };
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message,
            severity: Severity::Error,
        });
        None
    }
    /// Takes the next space from the parser and returns true.
    /// If the parser is not pointing at a space, a diagnostic is created and false is returned.
    pub(super) fn parse_space(&mut self, parser: &mut Parser) -> bool {
        if parser.expect(' ') {
            true
        } else {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_char()),
                message: Message::ExpectedExact(" "),
                severity: Severity::Error,
            });
            false
        }
    }
    /// Takes the next new line character from the parser and returns true.
    /// If the parser is not pointing at a new line, a diagnostic is created and false is returned.
    pub(super) fn parse_line_end(&mut self, parser: &mut Parser) -> bool {
        if parser.expect('\n') {
            true
        } else {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_char()),
                message: Message::ExpectedExact("\n"),
                severity: Severity::Error,
            });
            false
        }
    }
    /// Take the next path from the parser.
    ///
    /// A path is a string literal which resolves to a path relative to the project root.
    pub(super) fn parse_path(&mut self, parser: &mut Parser) -> Option<(PathBuf, Span)> {
        let Some((path, span)) = parser.string() else {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_until(|c| c == ' ' || c == '\n')),
                message: Message::ExpectedPath,
                severity: Severity::Error,
            });
            return None;
        };
        let path_buf;
        if path.starts_with('/') {
            let path = self.evaluate_str(&path[1..], span.file, span.offset + 2);
            path_buf = self.root.join(path);
        } else {
            let path = self.evaluate_str(path, span.file, span.offset + 1);
            path_buf = self.file(parser.file).relative_path(path)
        }
        Some((path_buf, span))
    }
    /// Take the optional value of an attribute after its name, which is at the parser, as an operand which is `none`
    /// without a value.
    ///
    /// Returns [`None`] if the value could not be taken, which has been reported.
    pub(super) fn parse_attribute_value(&mut self, parser: &mut Parser, name_span: Span) -> Option<Operand> {
        if !parser.expect(' ') {
            return Some(Operand { data: Data::None, span: name_span, coerce: Coercion::Strict, origin: None });
        }
        self.parse_expression(parser)
    }
    /// Returns the value of a flag attribute, such as `default`, which is `true` without a value, or is a `bool`.
    pub(super) fn flag(&mut self, operand: Operand) -> Option<bool> {
        if operand.data == Data::None {
            return Some(true);
        }
        self.bool_operand(operand)
    }
    /// Take the next indented attribute of a statement, skipping those which are invalid, which have been reported.
    pub(super) fn next_attribute<'a>(&mut self, parser: &mut Parser<'a>, names: &[&str]) -> Option<Attribute<'a>> {
        while parser.indent().is_some() {
            if let Some(attribute) = self.parse_attribute(parser, names) {
                return Some(attribute);
            }
            parser.skip_line();
        }
        None
    }
    /// Take an indented attribute, with an optional value.
    ///
    /// The value of an attribute in `names` is a name, as taken by [`Self::parse_name`], and is required. Otherwise it
    /// is an expression.
    ///
    /// The name of an attribute is identifiers joined by `/`, as for one named after a property, `codegen/<option>`.
    fn parse_attribute<'a>(&mut self, parser: &mut Parser<'a>, names: &[&str]) -> Option<Attribute<'a>> {
        let start = parser.offset;
        self.parse_ident(parser)?;
        while parser.peek() == Some('/') && is_name_continuation(parser) {
            parser.expect('/');
            self.parse_ident(parser)?;
        }
        let name = &parser.data[start..parser.offset];
        let name_span = parser.span_from(start);
        let (data, data_span, origin, coerce) = if names.contains(&name) {
            if !self.parse_space(parser) {
                return None;
            }
            let Name { name: mut value, span, .. } = self.parse_name(parser)?;
            // a crate can only use crates for the same target, so it is not given, unlike in a reference to a crate
            if let Some((target, crate_name)) = value.split_once('/') {
                let declared = self.crates.get(&(crate_name.into(), target.into())).map(|c| c.name_span);
                value = crate_name.into();
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::UseWithTarget { declared },
                    severity: Severity::Error,
                });
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::Replacement { description: "use the crate by its name", replacement: value.clone() },
                    severity: Severity::Suggestion,
                });
            }
            (Data::Str(value), span, None, Coercion::Strict)
        } else if parser.expect(' ') {
            let operand = self.parse_expression(parser)?;
            (operand.data, operand.span, operand.origin, operand.coerce)
        } else {
            (Data::None, name_span, None, Coercion::Strict)
        };
        if !self.parse_line_end(parser) {
            parser.skip_line();
        }
        Some(Attribute {
            name,
            name_span,
            data,
            data_span,
            origin,
            coerce,
        })
    }
}
