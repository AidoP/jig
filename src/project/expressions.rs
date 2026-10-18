//! Expressions and string templates, which are evaluated as they are parsed.

use std::collections::BTreeMap;

use super::diagnostic::{Diagnostic, Message, Severity};
use super::statements::Name;
use super::syntax::{Operator, Parser, Span, expression_end, is_attribute_access, is_name_continuation, is_named_entry};
use super::value::{Coercion, Data, Operand};
use super::Project;

/// An entry of a dict, as it is written in an expression.
struct DictEntry {
    name: Name,
    value: Data,
}

impl Project {
    /// Take the next expression from the parser and evaluate it.
    ///
    /// An expression is an operand, or operands joined by operators.
    /// An operator must have a single space on either side of it.
    pub(super) fn parse_expression(&mut self, parser: &mut Parser) -> Option<Operand> {
        self.parse_binary(parser, 0)
    }
    /// Take operands joined by operators with a precedence of at least `min_precedence`.
    pub(super) fn parse_binary(&mut self, parser: &mut Parser, min_precedence: u8) -> Option<Operand> {
        let mut lhs = self.parse_operand(parser)?;
        while let Some(operator) = parser.operator(min_precedence) {
            let rhs = self.parse_binary(parser, operator.precedence() + 1)?;
            lhs = self.evaluate_binary(operator, lhs, rhs)?;
        }
        Some(lhs)
    }
    /// Take the next operand from the parser.
    ///
    /// An operand is a literal, a property name, an expression in `{}` to permit coercion, or an expression in `()`.
    /// It may be preceded by `!` to negate a boolean.
    pub(super) fn parse_operand(&mut self, parser: &mut Parser) -> Option<Operand> {
        let start = parser.offset;
        match parser.peek() {
            Some('\'') => {
                let (string, span) = self.parse_string(parser)?;
                Some(Operand { data: Data::Str(string), span, coerce: Coercion::Strict, origin: None })
            },
            Some('0'..='9' | '-' | '+') => {
                let span = parser.span_until(|c| c == ' ' || c == '\n');
                let Some((data, span)) = parser.integer() else {
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::InvalidInteger,
                        severity: Severity::Error,
                    });
                    return None;
                };
                Some(Operand { data, span, coerce: Coercion::Sign, origin: None })
            },
            // the complement of an integer, or the negation of a boolean, as which `{}` is taken
            Some('!') => {
                parser.expect('!');
                let operand = self.parse_operand(parser)?;
                let data = match operand.data {
                    Data::Unsigned(i) if operand.coerce != Coercion::Any => Data::Unsigned(!i),
                    Data::Signed(i) if operand.coerce != Coercion::Any => Data::Signed(!i),
                    _ => Data::Bool(!self.bool_operand(operand)?),
                };
                Some(Operand { data, span: parser.span_from(start), coerce: Coercion::Strict, origin: None })
            },
            Some('(') => {
                parser.expect('(');
                let inner = self.parse_binary(parser, 0)?;
                if !parser.expect(')') {
                    self.diagnostics.push(Diagnostic {
                        span: Some(parser.span_char()),
                        message: Message::ExpectedExact(")"),
                        severity: Severity::Error,
                    });
                    return None;
                }
                Some(Operand { span: parser.span_from(start), ..inner })
            },
            Some('[') => {
                parser.expect('[');
                // an empty dict, which has no names to tell it from a list
                if parser.as_str().starts_with(":]") {
                    parser.offset += 2;
                    let data = Data::Dict(BTreeMap::new());
                    return Some(Operand { data, span: parser.span_from(start), coerce: Coercion::Strict, origin: None });
                }
                if is_named_entry(parser) {
                    return self.parse_dict(parser, start);
                }
                let items = self.parse_arguments(parser, ']', |this, parser| this.parse_binary(parser, 0))?;
                let data = Data::List(items.into_iter().map(|item| item.data).collect());
                Some(Operand { data, span: parser.span_from(start), coerce: Coercion::Strict, origin: None })
            },
            Some('{') => {
                let operand = self.parse_braced(parser)?;
                // part of a name, which may be computed, as in `{target}/core` or the namespace `{target}/`, or the
                // whole of one whose attribute is taken, as in `{target}.has-std`
                if !is_name_continuation(parser) && parser.peek() != Some('/') && !is_attribute_access(parser) {
                    return Some(operand);
                }
                let name = self.parse_name_from(parser, start, operand.data.coerce_string(), false)?;
                self.named_operand(parser, name)
            },
            // any word is parsed as a name, to report its invalid characters
            Some(c) if c.is_alphabetic() || c == '_' => {
                let name = self.parse_name(parser)?;
                if parser.peek() == Some('(') && name.literal && !name.name.contains('/') {
                    return self.parse_call(parser, &name.name, name.span);
                }
                self.named_operand(parser, name)
            },
            _ => {
                self.diagnostics.push(Diagnostic {
                    span: Some(parser.span_until(|c| c == ' ' || c == '\n')),
                    message: Message::ExpectedExpression,
                    severity: Severity::Error,
                });
                None
            },
        }
    }
    /// Take an expression in `{}`, which may be converted to any type.
    pub(super) fn parse_braced(&mut self, parser: &mut Parser) -> Option<Operand> {
        let start = parser.offset;
        parser.expect('{');
        let inner = self.parse_binary(parser, 0)?;
        if !parser.expect('}') {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_char()),
                message: Message::ExpectedExact("}"),
                severity: Severity::Error,
            });
            return None;
        }
        Some(Operand { span: parser.span_from(start), coerce: Coercion::Any, ..inner })
    }
    /// Returns the value of what a name refers to, or of its attribute after `.`, or the dict of a namespace, when it is
    /// followed by a `/` which does not continue it.
    fn named_operand(&mut self, parser: &mut Parser, name: Name) -> Option<Operand> {
        if parser.peek() == Some('/') {
            parser.expect('/');
            let span = Span { len: name.span.len + 1, ..name.span };
            let data = self.namespace(&name.name, span)?;
            return Some(Operand { data, span, coerce: Coercion::Strict, origin: None });
        }
        if is_attribute_access(parser) {
            return self.parse_named_attribute(parser, &name.name, name.span, name.span.offset);
        }
        let data = match name.name.as_str() {
            "true" if name.literal => Data::Bool(true),
            "false" if name.literal => Data::Bool(false),
            _ => return self.reference(&name.name, name.span),
        };
        Some(Operand { data, span: name.span, coerce: Coercion::Strict, origin: None })
    }
    /// Apply an operator.
    ///
    /// Operands must have the type required by the operator, and the same type as each other.
    /// An operand is only converted to satisfy this as far as its [`Coercion`] permits.
    fn evaluate_binary(&mut self, operator: Operator, lhs: Operand, rhs: Operand) -> Option<Operand> {
        let span = Span {
            file: lhs.span.file,
            offset: lhs.span.offset,
            len: rhs.span.offset + rhs.span.len - lhs.span.offset,
        };
        let rhs_span = rhs.span;
        let mut coerce = Coercion::Strict;
        let data = match operator {
            Operator::Or | Operator::And => {
                let lhs = self.bool_operand(lhs)?;
                let rhs = self.bool_operand(rhs)?;
                Data::Bool(match operator {
                    Operator::Or => lhs || rhs,
                    _ => lhs && rhs,
                })
            },
            Operator::Equal | Operator::NotEqual => {
                let (lhs, rhs) = self.comparable_operands(lhs, rhs, span)?;
                Data::Bool((lhs == rhs) == matches!(operator, Operator::Equal))
            },
            Operator::Less | Operator::LessEqual | Operator::Greater | Operator::GreaterEqual => {
                let ordering = match self.integer_operands(lhs, rhs, span)? {
                    (Data::Unsigned(lhs), Data::Unsigned(rhs)) => lhs.cmp(&rhs),
                    (Data::Signed(lhs), Data::Signed(rhs)) => lhs.cmp(&rhs),
                    _ => unreachable!(),
                };
                Data::Bool(match operator {
                    Operator::Less => ordering.is_lt(),
                    Operator::LessEqual => ordering.is_le(),
                    Operator::Greater => ordering.is_gt(),
                    _ => ordering.is_ge(),
                })
            },
            // lists are concatenated and dicts are merged, with the entries on the right replacing those on the left, but
            // only with others of the same type
            Operator::Add if is_collection(&lhs.data) || is_collection(&rhs.data) => {
                match (lhs.data.clone(), rhs.data.clone()) {
                    (Data::List(mut lhs_items), Data::List(rhs_items)) => {
                        lhs_items.extend(rhs_items);
                        Data::List(lhs_items)
                    },
                    (Data::Dict(mut lhs_entries), Data::Dict(rhs_entries)) => {
                        lhs_entries.extend(rhs_entries);
                        Data::Dict(lhs_entries)
                    },
                    _ => {
                        self.diagnostics.push(Diagnostic {
                            span: Some(span),
                            message: Message::MismatchedOperands {
                                lhs: lhs.data.type_name(),
                                rhs: rhs.data.type_name(),
                                lhs_origin: lhs.origin,
                                rhs_origin: rhs.origin,
                            },
                            severity: Severity::Error,
                        });
                        return None;
                    },
                }
            },
            Operator::Add
            | Operator::Subtract
            | Operator::Multiply
            | Operator::Divide
            | Operator::Remainder
            | Operator::BitOr
            | Operator::BitXor
            | Operator::BitAnd
            | Operator::ShiftLeft
            | Operator::ShiftRight => {
                coerce = lhs.coerce.min(rhs.coerce);
                let (lhs, rhs) = self.integer_operands(lhs, rhs, span)?;
                if matches!(operator, Operator::Divide | Operator::Remainder) && matches!(rhs, Data::Unsigned(0) | Data::Signed(0)) {
                    self.diagnostics.push(Diagnostic {
                        span: Some(rhs_span),
                        message: Message::DivisionByZero,
                        severity: Severity::Error,
                    });
                    return None;
                }
                macro_rules! checked {
                    ($lhs:expr, $rhs:expr) => {
                        match operator {
                            Operator::Add => $lhs.checked_add($rhs),
                            Operator::Subtract => $lhs.checked_sub($rhs),
                            Operator::Multiply => $lhs.checked_mul($rhs),
                            Operator::Divide => $lhs.checked_div($rhs),
                            Operator::Remainder => $lhs.checked_rem($rhs),
                            Operator::BitOr => Some($lhs | $rhs),
                            Operator::BitXor => Some($lhs ^ $rhs),
                            Operator::BitAnd => Some($lhs & $rhs),
                            // the amount must be less than the number of bits, while the bits shifted out are lost
                            Operator::ShiftLeft => u32::try_from($rhs).ok().and_then(|rhs| $lhs.checked_shl(rhs)),
                            _ => u32::try_from($rhs).ok().and_then(|rhs| $lhs.checked_shr(rhs)),
                        }
                    };
                }
                let data = match (lhs, rhs) {
                    (Data::Unsigned(lhs), Data::Unsigned(rhs)) => checked!(lhs, rhs).map(Data::Unsigned),
                    (Data::Signed(lhs), Data::Signed(rhs)) => checked!(lhs, rhs).map(Data::Signed),
                    _ => unreachable!(),
                };
                let Some(data) = data else {
                    let shift = matches!(operator, Operator::ShiftLeft | Operator::ShiftRight);
                    self.diagnostics.push(Diagnostic {
                        span: Some(if shift { rhs_span } else { span }),
                        message: if shift { Message::ShiftAmount } else { Message::IntegerOverflow },
                        severity: Severity::Error,
                    });
                    return None;
                };
                data
            },
        };
        Some(Operand { data, span, coerce, origin: None })
    }
    /// Ensure an operand is a boolean, converting it if it permits coercion.
    pub(super) fn bool_operand(&mut self, operand: Operand) -> Option<bool> {
        let value = match &operand.data {
            Data::Bool(value) => Some(*value),
            data if operand.coerce == Coercion::Any => data.coerce_bool(),
            _ => None,
        };
        if value.is_none() {
            self.invalid_operand(&operand, "bool", operand.coerce == Coercion::Any);
        }
        value
    }
    /// Ensure an operand is an integer, converting it if it permits coercion.
    pub(super) fn integer_operand(&mut self, operand: Operand) -> Option<Operand> {
        let data = match &operand.data {
            Data::Unsigned(_) | Data::Signed(_) => return Some(operand),
            data if operand.coerce == Coercion::Any => data.coerce_integer(),
            _ => None,
        };
        let Some(data) = data else {
            self.invalid_operand(&operand, "integer", operand.coerce == Coercion::Any);
            return None;
        };
        Some(Operand { data, ..operand })
    }
    /// Ensure both operands are integers of the same type.
    fn integer_operands(&mut self, lhs: Operand, rhs: Operand, span: Span) -> Option<(Data, Data)> {
        let lhs = self.integer_operand(lhs)?;
        let rhs = self.integer_operand(rhs)?;
        self.unify_sign(lhs, rhs, span)
    }
    /// Ensure two integer operands are either both signed or both unsigned.
    pub(super) fn unify_sign(&mut self, lhs: Operand, rhs: Operand, span: Span) -> Option<(Data, Data)> {
        if lhs.data.type_name() == rhs.data.type_name() {
            return Some((lhs.data, rhs.data));
        }
        if let Some(data) = rhs.data.convert_sign() && rhs.coerce >= Coercion::Sign {
            return Some((lhs.data, data));
        }
        if let Some(data) = lhs.data.convert_sign() && lhs.coerce >= Coercion::Sign {
            return Some((data, rhs.data));
        }
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::MismatchedOperands {
                lhs: lhs.data.type_name(),
                rhs: rhs.data.type_name(),
                lhs_origin: lhs.origin,
                rhs_origin: rhs.origin,
            },
            severity: Severity::Error,
        });
        None
    }
    /// Ensure both operands have the same type, of any kind.
    fn comparable_operands(&mut self, lhs: Operand, rhs: Operand, span: Span) -> Option<(Data, Data)> {
        if lhs.data.type_name() == rhs.data.type_name() {
            return Some((lhs.data, rhs.data));
        }
        if lhs.data.is_integer() && rhs.data.is_integer() {
            return self.unify_sign(lhs, rhs, span);
        }
        let (from, to, swap) = if rhs.coerce == Coercion::Any {
            (rhs, lhs, false)
        } else if lhs.coerce == Coercion::Any {
            (lhs, rhs, true)
        } else {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::MismatchedOperands {
                    lhs: lhs.data.type_name(),
                    rhs: rhs.data.type_name(),
                    lhs_origin: lhs.origin,
                    rhs_origin: rhs.origin,
                },
                severity: Severity::Error,
            });
            return None;
        };
        let Some(data) = from.data.coerce_like(&to.data) else {
            self.invalid_operand(&from, to.data.type_name(), true);
            return None;
        };
        Some(if swap { (data, to.data) } else { (to.data, data) })
    }
    /// Ensure an operand is a list.
    pub(super) fn list_operand(&mut self, operand: Operand) -> Option<Vec<Data>> {
        if let Data::List(items) = operand.data {
            return Some(items);
        }
        self.invalid_operand(&operand, "list", false);
        None
    }
    /// Ensure an operand is a string, converting it if it permits coercion, or if it is a reference, which is its paths.
    pub(super) fn str_operand(&mut self, operand: Operand) -> Option<String> {
        let coerce = operand.coerce == Coercion::Any;
        if let Data::Str(string) = operand.data {
            return Some(string);
        }
        if let Data::Ref(_) = operand.data {
            return Some(operand.data.coerce_string());
        }
        if coerce && operand.data != Data::None {
            return Some(operand.data.coerce_string());
        }
        self.invalid_operand(&operand, "str", coerce);
        None
    }
    /// Take the entries of a dict, as `name: value` separated by `, `, after the opening bracket.
    fn parse_dict(&mut self, parser: &mut Parser, start: usize) -> Option<Operand> {
        let entries = self.parse_arguments(parser, ']', |this, parser| this.parse_dict_entry(parser))?;
        let mut dict = BTreeMap::new();
        for entry in entries {
            if dict.contains_key(&entry.name.name) {
                self.diagnostics.push(Diagnostic {
                    span: Some(entry.name.span),
                    message: Message::RepeatedKey(entry.name.name),
                    severity: Severity::Error,
                });
                return None;
            }
            dict.insert(entry.name.name, entry.value);
        }
        Some(Operand { data: Data::Dict(dict), span: parser.span_from(start), coerce: Coercion::Strict, origin: None })
    }
    /// Take an entry of a dict, which every item of a dict must be.
    fn parse_dict_entry(&mut self, parser: &mut Parser) -> Option<DictEntry> {
        if !is_named_entry(parser) {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_until(|c| c == ',' || c == ']' || c == '\n')),
                message: Message::ExpectedDictEntry,
                severity: Severity::Error,
            });
            return None;
        }
        let name = self.parse_name(parser)?;
        parser.offset += 2;
        let value = self.parse_binary(parser, 0)?.data;
        Some(DictEntry { name, value })
    }
    /// Report an operand which does not have the type that is `expected`, or which cannot be converted to it if
    /// `coerce`.
    pub(super) fn invalid_operand(&mut self, operand: &Operand, expected: &'static str, coerce: bool) {
        self.diagnostics.push(Diagnostic {
            span: Some(operand.span),
            message: Message::InvalidOperand { ty: operand.data.type_name(), expected, coerce, origin: operand.origin },
            severity: Severity::Error,
        });
    }
    /// Take the next string from the parser.
    fn parse_string(&mut self, parser: &mut Parser) -> Option<(String, Span)> {
        if parser.as_str().starts_with("'''\n") {
            return self.parse_multiline_string(parser);
        }
        let Some((string, span)) = parser.string() else {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_until(|c| c == ' ' || c == '\n')),
                message: Message::ExpectedString,
                severity: Severity::Error,
            });
            return None;
        };
        Some((self.evaluate_str(string, span.file, span.offset + 1), span))
    }
    /// Take a multi-line string, which starts with `'''` at the end of a line, and ends with `'''` at the
    /// indentation of that line.
    ///
    /// Each line between must be indented by 4 spaces more than the line it starts on, which is removed, or be blank.
    /// A comment, with `#` in the first column, is skipped.
    /// The lines are templates, like other strings, and are joined by newlines, without one at the end.
    fn parse_multiline_string(&mut self, parser: &mut Parser) -> Option<(String, Span)> {
        let start = parser.offset;
        let line_start = parser.data[..start].rfind('\n').map_or(0, |i| i + 1);
        let indent = parser.data[line_start..start].len() - parser.data[line_start..start].trim_start_matches(' ').len();
        let content_indent = " ".repeat(indent + 4);
        let closing = format!("{}'''", " ".repeat(indent));
        let span = Span { file: parser.file, offset: start, len: 3 };
        parser.offset += 4;
        let mut lines = Vec::new();
        let mut valid = true;
        loop {
            let rest = parser.as_str();
            let len = rest.find('\n').unwrap_or(rest.len());
            let line = &rest[..len];
            if line.starts_with(&closing) {
                parser.offset += closing.len();
                break;
            }
            // a comment in the first column is not part of the string
            if line.starts_with('#') {
                parser.offset += len + 1;
                continue;
            }
            let spaces = line.len() - line.trim_start_matches(' ').len();
            // a line which is not indented beyond the start is not part of the string, which was not ended
            if rest.is_empty() || !line.trim().is_empty() && spaces <= indent {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::UnterminatedString,
                    severity: Severity::Error,
                });
                // the end of the previous line, so that skipping the rest of the statement stops at this one
                if !rest.is_empty() {
                    parser.offset -= 1;
                }
                return None;
            }
            if line.trim().is_empty() {
                lines.push(("", parser.offset));
            } else if let Some(content) = line.strip_prefix(&content_indent) {
                lines.push((content, parser.offset + content_indent.len()));
            } else {
                self.diagnostics.push(Diagnostic {
                    span: Some(Span { file: parser.file, offset: parser.offset, len: spaces.max(1) }),
                    message: Message::StringIndentation { expected: indent + 4 },
                    severity: Severity::Error,
                });
                valid = false;
            }
            parser.offset += (len + 1).min(rest.len());
        }
        if !valid {
            return None;
        }
        let lines: Vec<_> = lines.iter().map(|(line, offset)| self.evaluate_str(line, parser.file, *offset)).collect();
        Some((lines.join("\n"), span))
    }
    /// Evaluate string templates.
    ///
    /// An expression in `{}` is replaced with its value, while `{{` and `}}` are replaced with a single brace.
    /// `offset` is the offset of `s` within the file.
    pub(super) fn evaluate_str(&mut self, s: &str, file: usize, offset: usize) -> String {
        let mut string = String::new();
        let mut rest = s;
        while let Some(i) = rest.find(['{', '}']) {
            string.push_str(&rest[..i]);
            let brace = &rest[i..i + 1];
            let after = &rest[i + 1..];
            if after.starts_with(brace) {
                string.push_str(brace);
                rest = &after[1..];
                continue;
            }
            // the offset of `after` within the file
            let offset = offset + s.len() - after.len();
            if brace == "}" {
                self.diagnostics.push(Diagnostic {
                    span: Some(Span { file, offset: offset - 1, len: 1 }),
                    message: Message::InvalidStringTemplate,
                    severity: Severity::Error,
                });
                rest = after;
                continue;
            }
            let Some(end) = expression_end(after) else {
                self.diagnostics.push(Diagnostic {
                    span: Some(Span { file, offset: offset - 1, len: after.len() + 1 }),
                    message: Message::InvalidStringTemplate,
                    severity: Severity::Error,
                });
                return string;
            };
            let first_diagnostic = self.diagnostics.len();
            let mut parser = Parser::new(file, &after[..end]);
            match self.parse_expression(&mut parser) {
                Some(operand) if parser.is_empty() => string.push_str(&operand.data.coerce_string()),
                Some(_) => self.diagnostics.push(Diagnostic {
                    span: Some(parser.span_until(|_| false)),
                    message: Message::InvalidStringTemplate,
                    severity: Severity::Error,
                }),
                None => (),
            }
            // the parser's spans are relative to the expression
            for diagnostic in &mut self.diagnostics[first_diagnostic..] {
                if let Some(span) = &mut diagnostic.span {
                    span.offset += offset;
                }
            }
            rest = &after[end + 1..];
        }
        string.push_str(rest);
        string
    }
}

/// Returns whether data is a list or a dict, which `+` joins.
fn is_collection(data: &Data) -> bool {
    matches!(data, Data::List(_) | Data::Dict(_))
}
