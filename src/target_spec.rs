//! Generation of the JSON specifications of custom targets, which rustc uses in place of a builtin target.

use std::path::{Path, PathBuf};

use crate::project::{Data, Field};
use crate::rust_analyzer::string;

/// The specification of a custom target, in the build directory.
pub fn path(build_dir: &Path, name: &str) -> PathBuf {
    build_dir.join("targets").join(format!("{name}.json"))
}

/// A field of a JSON object, whose value is kept as JSON so that it is written as it was read.
#[derive(Debug, PartialEq)]
struct JsonField {
    name: String,
    /// The JSON of the value.
    json: String,
}

/// Returns the specification of a custom target, from the specification of the builtin target it inherits, if any, with
/// its fields replaced or added.
pub fn spec(target_fields: &[Field], inherited: Option<&str>) -> Result<String, &'static str> {
    let mut fields = Vec::new();
    if let Some(inherited) = inherited {
        let Some(inherited) = object_fields(inherited) else {
            return Err("invalid target specification from rustc");
        };
        fields = inherited;
    }
    for field in target_fields {
        let json = json(&field.value);
        // a field replaces an inherited one in place, so that the order of the inherited fields is kept
        match fields.iter_mut().find(|inherited| inherited.name == field.name) {
            Some(inherited) => inherited.json = json,
            None => fields.push(JsonField { name: field.name.clone(), json }),
        }
    }
    let mut spec = String::from("{\n");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            spec.push_str(",\n");
        }
        spec.push_str(&format!("  {}: {}", string(&field.name), field.json));
    }
    spec.push_str("\n}\n");
    Ok(spec)
}

/// Returns a value as JSON.
fn json(data: &Data) -> String {
    match data {
        Data::None => "null".into(),
        Data::Str(s) => string(s),
        Data::Bool(b) => b.to_string(),
        Data::Unsigned(i) => i.to_string(),
        Data::Signed(i) => i.to_string(),
        Data::List(items) => {
            let mut json = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    json.push_str(", ");
                }
                json.push_str(&self::json(item));
            }
            json.push(']');
            json
        },
        // the paths it refers to
        Data::Ref(_) => string(&data.coerce_string()),
        Data::Dict(entries) => {
            let mut json = String::from("{");
            for (name, value) in entries {
                if json.len() > 1 {
                    json.push_str(", ");
                }
                json.push_str(&format!("{}: {}", string(name), self::json(value)));
            }
            json.push('}');
            json
        },
    }
}

/// Split a JSON object into its fields, or return [`None`] if it is not an object.
fn object_fields(json: &str) -> Option<Vec<JsonField>> {
    let mut rest = json.trim().strip_prefix('{')?.trim_start();
    let mut fields = Vec::new();
    if let Some(after) = rest.strip_prefix('}') {
        if !after.trim().is_empty() {
            return None;
        }
        return Some(fields);
    }
    loop {
        // the names of specification fields do not need escapes
        let quoted = rest.strip_prefix('"')?;
        let name_end = quoted.find('"')?;
        let name = quoted[..name_end].to_string();
        rest = quoted[name_end + 1..].trim_start().strip_prefix(':')?.trim_start();
        let len = value_len(rest)?;
        fields.push(JsonField { name, json: rest[..len].trim_end().to_string() });
        rest = rest[len..].trim_start();
        match rest.as_bytes().first()? {
            b',' => rest = rest[1..].trim_start(),
            b'}' => {
                if !rest[1..].trim().is_empty() {
                    return None;
                }
                return Some(fields);
            },
            _ => return None,
        }
    }
}

/// Returns the length of the JSON value at the start of `s`.
fn value_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            _ if in_string => (),
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return Some(i),
            b'}' | b']' => depth -= 1,
            b',' if depth == 0 => return Some(i),
            _ => (),
        }
        i += 1;
    }
    // the value is the rest of `s`, which must be complete
    if depth != 0 || in_string {
        return None;
    }
    Some(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let json = r#"{
  "arch": "riscv64",
  "metadata": { "description": "a, \"b\" {", "tier": 2 },
  "list": [1, [2, 3]],
  "flag": true
}"#;
        let field = |name: &str, json: &str| JsonField { name: name.into(), json: json.into() };
        assert_eq!(object_fields(json).unwrap(), [
            field("arch", "\"riscv64\""),
            field("metadata", r#"{ "description": "a, \"b\" {", "tier": 2 }"#),
            field("list", "[1, [2, 3]]"),
            field("flag", "true"),
        ]);
        assert_eq!(object_fields("{}").unwrap(), []);
        assert!(object_fields("{\"a\": 1").is_none());
        assert!(object_fields("[]").is_none());
    }
}
