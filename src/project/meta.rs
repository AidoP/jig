//! Meta-statements, which decide whether, and how often, the statement after them is processed.

use super::diagnostic::{Diagnostic, Message, Severity};
use super::syntax::{Parser, Span};
use super::value::{Data, Operand};
use super::Project;

/// What remains of a statement after its meta-statements.
pub(super) enum Meta {
    /// The statement is skipped.
    Skip,
    /// The statement is processed.
    Process,
    /// The statement has been processed already, by `@for` or `@with`.
    Done,
}

/// What the meta-statements of a statement have decided so far.
struct MetaState {
    /// Whether the statement is processed, as far as the conditions so far.
    active: bool,
    /// Whether the statement is conditional, so that it starts or continues a chain.
    conditional: bool,
    /// Whether the chain had already processed a statement, if this statement continues it with `@else`.
    continues: Option<bool>,
    /// Whether a meta-statement is invalid, which skips the statement, and any `@else` after it.
    failed: bool,
}
impl MetaState {
    /// The chain after the statement, given whether it is processed: whether any statement in the chain has been
    /// processed, or [`None`] if the statement is unconditional.
    fn chain(&self, processed: bool) -> Option<bool> {
        match self.continues {
            Some(previous) => Some(previous || processed),
            None if self.conditional => Some(processed),
            None => None,
        }
    }
}

/// The header of a `@with`, whose property is given its value while the rest of the chain is processed.
struct With {
    name: String,
    span: Span,
    value: Operand,
}

/// The header of a `@for`.
struct ForLoop {
    /// The property which takes each value in turn, as a local.
    name: String,
    /// The values, where each item of a list is a value.
    values: Vec<Data>,
}

impl Project {
    /// Take the meta-statements which precede a statement, returning whether the statement should be processed.
    ///
    /// Each meta-statement is a line starting with `@`, and must be followed directly by another meta-statement or by
    /// the statement it applies to. Once the statement is known to be skipped, later conditions are not evaluated.
    ///
    /// `chain` is whether a statement has been processed in the current chain of conditional statements, which `@else`
    /// continues, or [`None`] if the previous statement was unconditional.
    ///
    /// `@for` processes the meta-statements and statement after it for each value, and `@with` processes them with a
    /// property set, leaving nothing more to process.
    pub(super) fn parse_meta(&mut self, parser: &mut Parser, chain: &mut Option<bool>) -> Meta {
        let mut state = MetaState { active: true, conditional: false, continues: None, failed: false };
        let mut first = true;
        while parser.peek() == Some('@') {
            let start = parser.offset;
            parser.expect('@');
            let Some((name, span)) = self.parse_ident(parser) else {
                parser.skip_line();
                state.failed = true;
                continue;
            };
            let meta_span = Span { file: span.file, offset: start, len: span.offset + span.len - start };
            self.meta_insertion = (!matches!(name, "else" | "else-if")).then_some(Span { len: 0, ..meta_span });
            match name {
                "if" => {
                    state.conditional = true;
                    if !state.active {
                        parser.skip_line();
                    } else if let Some(condition) = self.parse_condition(parser) {
                        state.active = condition;
                    } else {
                        state.failed = true;
                    }
                },
                // a skipped statement is not repeated
                "for" if !state.active || state.failed => {
                    parser.skip_line();
                },
                "for" => {
                    let Some(for_loop) = self.parse_for(parser) else {
                        state.failed = true;
                        continue;
                    };
                    if matches!(parser.peek(), None | Some('\n' | '#')) {
                        self.diagnostics.push(Diagnostic {
                            span: Some(meta_span),
                            message: Message::ExpectedStatementAfterMeta,
                            severity: Severity::Error,
                        });
                        *chain = None;
                        return Meta::Skip;
                    }
                    // a statement with `@for` counts as processed in its chain, even without any values
                    *chain = state.chain(true);
                    return self.repeat(parser, for_loop);
                },
                // a skipped statement does not set the property
                "with" if !state.active || state.failed => {
                    parser.skip_line();
                },
                "with" => {
                    let Some(with) = self.parse_with(parser) else {
                        state.failed = true;
                        continue;
                    };
                    if matches!(parser.peek(), None | Some('\n' | '#')) {
                        self.diagnostics.push(Diagnostic {
                            span: Some(meta_span),
                            message: Message::ExpectedStatementAfterMeta,
                            severity: Severity::Error,
                        });
                        *chain = None;
                        return Meta::Skip;
                    }
                    let Ok(inner) = self.process_with(parser, with) else {
                        state.failed = true;
                        continue;
                    };
                    // the conditions after `@with` continue the chain, unless there were some before it
                    *chain = if state.conditional { state.chain(inner.unwrap_or(true)) } else { inner };
                    return Meta::Done;
                },
                // `@then` is taken with the statement before it
                "then" => {
                    parser.skip_line();
                    self.diagnostics.push(Diagnostic {
                        span: Some(meta_span),
                        message: Message::ThenWithoutStatement,
                        severity: Severity::Error,
                    });
                    state.failed = true;
                },
                "else" | "else-if" => {
                    state.conditional = true;
                    match *chain {
                        Some(processed) if first => {
                            state.continues = Some(processed);
                            state.active &= !processed;
                        },
                        _ => {
                            let message = if first {
                                Message::ElseWithoutIf(name.into())
                            } else {
                                Message::ElseNotFirst(name.into())
                            };
                            self.diagnostics.push(Diagnostic {
                                span: Some(meta_span),
                                message,
                                severity: Severity::Error,
                            });
                            state.failed = true;
                        },
                    }
                    if name == "else" {
                        if !self.parse_line_end(parser) {
                            parser.skip_line();
                            state.failed = true;
                        }
                    } else if !state.active || state.failed {
                        parser.skip_line();
                    } else if let Some(condition) = self.parse_condition(parser) {
                        state.active = condition;
                    } else {
                        state.failed = true;
                    }
                },
                _ => {
                    parser.skip_line();
                    self.diagnostics.push(Diagnostic {
                        span: Some(meta_span),
                        message: Message::UnknownMetaStatement(name.into()),
                        severity: Severity::Error,
                    });
                    state.failed = true;
                },
            }
            first = false;
            if matches!(parser.peek(), None | Some('\n' | '#')) {
                self.diagnostics.push(Diagnostic {
                    span: Some(meta_span),
                    message: Message::ExpectedStatementAfterMeta,
                    severity: Severity::Error,
                });
                *chain = None;
                return Meta::Skip;
            }
        }
        if state.failed {
            *chain = Some(true);
            return Meta::Skip;
        }
        *chain = state.chain(state.active);
        if state.active { Meta::Process } else { Meta::Skip }
    }
    /// Process the meta-statements and statement after a `@for` for each of its values, with the property as a local.
    fn repeat(&mut self, parser: &mut Parser, for_loop: ForLoop) -> Meta {
        let body = parser.offset;
        let previous = self.files[parser.file].locals.get(&for_loop.name).copied();
        for value in &for_loop.values {
            // a fatal diagnostic stops the loop too
            if self.fatal {
                break;
            }
            parser.offset = body;
            self.set_local(&for_loop.name, value.clone(), parser.file);
            self.parse_statement(parser, &mut None);
        }
        match previous {
            Some(previous) => self.files[parser.file].locals.insert(for_loop.name, previous),
            None => self.files[parser.file].locals.remove(&for_loop.name),
        };
        if !for_loop.values.is_empty() {
            return Meta::Done;
        }
        // without any values, the rest is skipped
        while parser.peek() == Some('@') {
            parser.skip_line();
        }
        Meta::Skip
    }
    /// Process the meta-statements and statement after a `@with`, and the statements chained to it, with its property
    /// set, then restore the property's value, unless they set it.
    ///
    /// Returns the chain of the conditions after it, or an error if the property cannot be set, which is reported.
    ///
    /// As with `set`, the property may be declared later, which checks the value it was given.
    fn process_with(&mut self, parser: &mut Parser, with: With) -> Result<Option<bool>, ()> {
        let previous = self.values_idx.get(&with.name).copied();
        let previous_local = self.files[parser.file].locals.get(&with.name).copied();
        if !self.set_property(&with.name, with.span, with.value) {
            return Err(());
        }
        let assigned = self.values_idx[&with.name];
        let mut inner = None;
        self.parse_statement(parser, &mut inner);
        // a value set by the statements remains, as with `set` anywhere else
        if self.values_idx.get(&with.name) != Some(&assigned) {
            return Ok(inner);
        }
        self.reverted.push(assigned);
        match previous {
            Some(previous) => self.values_idx.insert(with.name.clone(), previous),
            None => self.values_idx.remove(&with.name),
        };
        if let Some(local) = previous_local {
            self.files[parser.file].locals.insert(with.name, local);
        }
        Ok(inner)
    }
    /// Take the rest of a `@with`, a property and its value, as `<name> as <value>`, skipping the line if it is invalid.
    fn parse_with(&mut self, parser: &mut Parser) -> Option<With> {
        let with = self.parse_with_header(parser);
        if with.is_none() {
            parser.skip_line();
        }
        with
    }
    fn parse_with_header(&mut self, parser: &mut Parser) -> Option<With> {
        if !self.parse_space(parser) {
            return None;
        }
        let name = self.parse_name(parser)?;
        if !self.parse_space(parser) || !self.parse_keyword(parser, "as") || !self.parse_space(parser) {
            return None;
        }
        let value = self.parse_expression(parser)?;
        if !self.parse_line_end(parser) {
            return None;
        }
        Some(With { name: name.text, span: name.span, value })
    }
    /// Take a keyword, such as the `in` of `@for`, reporting it if it is missing.
    fn parse_keyword(&mut self, parser: &mut Parser, keyword: &'static str) -> bool {
        if matches!(parser.as_str().strip_prefix(keyword), Some(rest) if rest.starts_with([' ', '\n'])) {
            parser.offset += keyword.len();
            return true;
        }
        self.diagnostics.push(Diagnostic {
            span: Some(parser.span_until(|c| c == ' ' || c == '\n')),
            message: Message::ExpectedExact(keyword),
            severity: Severity::Error,
        });
        false
    }
    /// Take the rest of a `@for`, skipping the line if it is invalid.
    fn parse_for(&mut self, parser: &mut Parser) -> Option<ForLoop> {
        let for_loop = self.parse_for_header(parser);
        if for_loop.is_none() {
            parser.skip_line();
        }
        for_loop
    }
    /// Take the rest of a `@for`, a property and the values it takes, separated by spaces.
    fn parse_for_header(&mut self, parser: &mut Parser) -> Option<ForLoop> {
        if !self.parse_space(parser) {
            return None;
        }
        let name = self.parse_name(parser)?.text;
        if !self.parse_space(parser) || !self.parse_keyword(parser, "in") {
            return None;
        }
        let mut values = Vec::new();
        loop {
            if !self.parse_space(parser) {
                return None;
            }
            match self.parse_expression(parser)?.data {
                Data::List(items) => values.extend(items),
                data => values.push(data),
            }
            if parser.peek() != Some(' ') {
                break;
            }
        }
        if !self.parse_line_end(parser) {
            return None;
        }
        Some(ForLoop { name, values })
    }
    /// Take the condition of an `@if` to the end of the line, returning [`None`] if it is invalid.
    fn parse_condition(&mut self, parser: &mut Parser) -> Option<bool> {
        if !self.parse_space(parser) {
            parser.skip_line();
            return None;
        }
        let Some(operand) = self.parse_binary(parser, 0) else {
            parser.skip_line();
            return None;
        };
        if !self.parse_line_end(parser) {
            parser.skip_line();
            return None;
        }
        self.bool_operand(&operand)
    }
}
