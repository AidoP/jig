//! Reading the JSON messages rustc writes with `--error-format=json`: its diagnostics, whose `rendered` field is what it
//! would have printed, and the notices of `--json=artifacts`, which say that an output, such as the metadata of a
//! crate, has been written, before rustc has finished.

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Returns the value of a field of an object, if it is one and has the field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Self::Object(fields) => fields.iter().find(|(field, _)| field == name).map(|(_, value)| value),
            _ => None,
        }
    }
    /// Returns the string, if it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(string) => Some(string),
            _ => None,
        }
    }
}

/// Parse a JSON value, which is the whole of the text but for the whitespace around it, or `None` if it is not one.
#[must_use]
pub fn parse(text: &str) -> Option<Value> {
    let mut parser = Parser { bytes: text.as_bytes(), pos: 0, depth: 0 };
    let value = parser.value()?;
    parser.whitespace();
    (parser.pos == parser.bytes.len()).then_some(value)
}

/// The deepest arrays and objects are nested, beyond which the text is not taken as JSON, so that it cannot exhaust the
/// stack.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while self.bytes.get(self.pos).is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }
    fn eat(&mut self, byte: u8) -> bool {
        self.whitespace();
        let ate = self.bytes.get(self.pos) == Some(&byte);
        if ate {
            self.pos += 1;
        }
        ate
    }
    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        let rest = self.bytes.get(self.pos..)?;
        rest.starts_with(word.as_bytes()).then(|| {
            self.pos += word.len();
            value
        })
    }
    fn value(&mut self) -> Option<Value> {
        self.whitespace();
        match *self.bytes.get(self.pos)? {
            b'n' => self.literal("null", Value::Null),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'"' => self.string().map(Value::String),
            b'[' | b'{' => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return None;
                }
                let value = if self.bytes[self.pos] == b'[' { self.array() } else { self.object() };
                self.depth -= 1;
                value
            },
            _ => self.number(),
        }
    }
    fn array(&mut self) -> Option<Value> {
        self.pos += 1;
        let mut items = Vec::new();
        if self.eat(b']') {
            return Some(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            if self.eat(b']') {
                return Some(Value::Array(items));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }
    fn object(&mut self) -> Option<Value> {
        self.pos += 1;
        let mut fields = Vec::new();
        if self.eat(b'}') {
            return Some(Value::Object(fields));
        }
        loop {
            self.whitespace();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return None;
            }
            let name = self.string()?;
            if !self.eat(b':') {
                return None;
            }
            fields.push((name, self.value()?));
            if self.eat(b'}') {
                return Some(Value::Object(fields));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }
    fn number(&mut self) -> Option<Value> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(|byte| matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        text.parse().ok().map(Value::Number)
    }
    /// Read a string, from its opening quote to its closing one, with its escapes, of which `\u` gives a UTF-16 code
    /// unit, which may be one of a surrogate pair.
    fn string(&mut self) -> Option<String> {
        self.pos += 1;
        let mut string = String::new();
        loop {
            let start = self.pos;
            while self.bytes.get(self.pos).is_some_and(|&byte| byte != b'"' && byte != b'\\') {
                self.pos += 1;
            }
            string.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).ok()?);
            if *self.bytes.get(self.pos)? == b'"' {
                self.pos += 1;
                return Some(string);
            }
            // a backslash, and what it escapes
            self.pos += 1;
            let escaped = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match escaped {
                b'"' => string.push('"'),
                b'\\' => string.push('\\'),
                b'/' => string.push('/'),
                b'b' => string.push('\u{8}'),
                b'f' => string.push('\u{c}'),
                b'n' => string.push('\n'),
                b'r' => string.push('\r'),
                b't' => string.push('\t'),
                b'u' => {
                    let unit = self.unit()?;
                    let code = if (0xd800..0xdc00).contains(&unit) {
                        // the second of the pair follows
                        if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                            return None;
                        }
                        self.pos += 2;
                        let low = self.unit()?;
                        if !(0xdc00..0xe000).contains(&low) {
                            return None;
                        }
                        0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        unit
                    };
                    string.push(char::from_u32(code)?);
                },
                _ => return None,
            }
        }
    }
    fn unit(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?).ok()?;
        self.pos += 4;
        u32::from_str_radix(digits, 16).ok()
    }
}

/// What a message of rustc says: an output it has written, by its path, or text it would have printed.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Artifact(String),
    Printed(String),
}

/// Returns what a line rustc wrote with `--error-format=json` says: the path of an artifact it notes, the rendered
/// text of a diagnostic, or the line itself, as one which is not JSON, such as from a linker, or a panic.
#[must_use]
pub fn message(line: &str) -> Message {
    let printed = || Message::Printed(format!("{line}\n"));
    if !line.trim_start().starts_with('{') {
        return printed();
    }
    let Some(value) = parse(line) else {
        return printed();
    };
    if let Some(artifact) = value.get("artifact").and_then(Value::as_str) {
        return Message::Artifact(artifact.into());
    }
    match value.get("rendered") {
        Some(Value::String(rendered)) => Message::Printed(rendered.clone()),
        // a diagnostic without a rendering, as a note of what was emitted, prints nothing
        Some(_) => Message::Printed(String::new()),
        None if value.get("$message_type").is_some() => Message::Printed(String::new()),
        None => printed(),
    }
}

/// Returns what rustc would have printed, from what it wrote with `--error-format=json`, calling `artifact` with the
/// path of each artifact it notes.
pub fn rendered(output: &[u8], artifact: &mut dyn FnMut(&str)) -> Vec<u8> {
    let text = String::from_utf8_lossy(output);
    let mut printed = String::new();
    for line in text.lines() {
        match message(line) {
            Message::Artifact(path) => artifact(&path),
            Message::Printed(text) => printed.push_str(&text),
        }
    }
    printed.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values() {
        let value = parse(r#" {"a": [1, -2.5e1, true, false, null], "b": {"c": "d"}, "": ""} "#).unwrap();
        let items = vec![Value::Number(1.0), Value::Number(-25.0), Value::Bool(true), Value::Bool(false), Value::Null];
        assert_eq!(value.get("a"), Some(&Value::Array(items)));
        assert_eq!(value.get("b").and_then(|b| b.get("c")).and_then(Value::as_str), Some("d"));
        assert_eq!(parse("[]"), Some(Value::Array(Vec::new())));
        assert_eq!(parse("{}"), Some(Value::Object(Vec::new())));
        for invalid in ["", "{", "[1,]", "{\"a\" 1}", "{\"a\": 1} x", "nul", "\"open", "[1 2]", "{1: 2}"] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
        // too deeply nested
        assert_eq!(parse(&"[".repeat(1000)), None);
    }

    #[test]
    fn parses_escapes() {
        let value = parse(r#""q\" s\\ /\/ \b\f\n\r\t é 🦀""#).unwrap();
        assert_eq!(value.as_str(), Some("q\" s\\ // \u{8}\u{c}\n\r\t é 🦀"));
        for invalid in [r#""\x""#, r#""\ud83e""#, r#""\ud83eA""#, r#""\u12""#] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn reads_what_rustc_writes() {
        let artifact = r#"{"$message_type":"artifact","artifact":"x86_64/liba.rmeta","emit":"metadata"}"#;
        assert_eq!(message(artifact), Message::Artifact("x86_64/liba.rmeta".into()));
        let diagnostic = r#"{"$message_type":"diagnostic","message":"unused","children":[],"rendered":"warning: unused\n"}"#;
        assert_eq!(message(diagnostic), Message::Printed("warning: unused\n".into()));
        assert_eq!(message(r#"{"$message_type":"diagnostic","rendered":null}"#), Message::Printed(String::new()));
        assert_eq!(message("error: linking with `cc` failed"), Message::Printed("error: linking with `cc` failed\n".into()));
        assert_eq!(message("{ not json"), Message::Printed("{ not json\n".into()));
        let output = format!("{artifact}\n{diagnostic}\nnote: from the linker\n");
        let mut artifacts = Vec::new();
        let printed = rendered(output.as_bytes(), &mut |path| artifacts.push(path.to_string()));
        assert_eq!(String::from_utf8(printed).unwrap(), "warning: unused\nnote: from the linker\n");
        assert_eq!(artifacts, ["x86_64/liba.rmeta"]);
    }
}
