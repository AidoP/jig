//! Depfiles, in the form gcc and rustc write, of the inputs a command discovered.
//!
//! A depfile is rules of the form `<target> ...: <input> ...`, where a `\` before a line break continues the line, a
//! `\` escapes a space, `#` or `\`, and `$$` is `$`. The inputs of every rule are taken, as ninja takes them. Lines
//! which begin with `#` are comments, except for the `# env-dep:<name>=<value>` or `# env-dep:<name>` lines rustc
//! writes for the environment variables `env!` and `option_env!` read, whose names are taken.

/// What a depfile lists: the inputs a command discovered, and the environment variables it read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub inputs: Vec<String>,
    pub env: Vec<String>,
}

/// Returns what a depfile lists, each input and variable once, in the order they are first listed.
///
/// # Errors
///
/// If a rule has no `:`, or has a second one.
pub fn parse(text: &str) -> Result<Discovered, String> {
    let mut inputs: Vec<String> = Vec::new();
    let mut env: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in logical_lines(text) {
        if let Some(comment) = line.trim_start().strip_prefix('#') {
            if let Some(variable) = comment.trim_start().strip_prefix("env-dep:") {
                let name = variable.split_once('=').map_or(variable, |(name, _)| name).trim_end();
                if !name.is_empty() && !env.iter().any(|known| known == name) {
                    env.push(name.into());
                }
            }
            continue;
        }
        let words = words(&line);
        if words.is_empty() {
            continue;
        }
        // the targets end with the first word which ends with a `:` which is not escaped
        let Some(end) = words.iter().position(|(_, colon)| *colon) else {
            return Err(format!("the rule {:?} has no `:`", line.trim()));
        };
        for (input, colon) in words.into_iter().skip(end + 1) {
            if colon {
                return Err(format!("the rule {:?} has more than one `:`", line.trim()));
            }
            if seen.insert(input.clone()) {
                inputs.push(input);
            }
        }
    }
    Ok(Discovered { inputs, env })
}

/// Returns the lines of a depfile, where a `\` before a line break continues the line.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('\n' | '\r')) => {
                chars.next_if_eq(&'\r');
                chars.next_if_eq(&'\n');
                line.push(' ');
            },
            // an escaped character, which is kept escaped for `words`
            '\\' => {
                line.push('\\');
                line.extend(chars.next_if(|&c| c != '\n'));
            },
            '\n' => lines.push(std::mem::take(&mut line)),
            c => line.push(c),
        }
    }
    lines.push(line);
    lines
}

/// Returns the words of a line, unescaped, each with whether it ends with a `:` which is not escaped, which is left
/// out. A `\` escapes a space, `#` or `\`, and `$$` is `$`. A `:` within a word is part of it.
fn words(line: &str) -> Vec<(String, bool)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' if !word.is_empty() => words.push((std::mem::take(&mut word), false)),
            ' ' | '\t' | '\r' => (),
            '\\' => match chars.next_if(|&c| matches!(c, ' ' | '#' | '\\')) {
                Some(escaped) => word.push(escaped),
                None => word.push('\\'),
            },
            '$' if chars.next_if_eq(&'$').is_some() => word.push('$'),
            ':' if chars.peek().is_none_or(|&c| matches!(c, ' ' | '\t' | '\r')) => {
                words.push((std::mem::take(&mut word), true));
            },
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push((word, false));
    }
    words
}

#[cfg(test)]
mod tests {
    use super::{Discovered, parse};

    #[test]
    fn takes_the_inputs() {
        let text = "out.o: a.c b.h \\\n  c.h\nout.o: d.h a.c\n";
        assert_eq!(parse(text).unwrap().inputs, ["a.c", "b.h", "c.h", "d.h"]);
        assert_eq!(parse("out: \n").unwrap(), Discovered::default());
        assert_eq!(parse("").unwrap(), Discovered::default());
        // several targets, and no line break at the end
        assert_eq!(parse("a b: c").unwrap().inputs, ["c"]);
    }

    #[test]
    fn unescapes_paths() {
        let text = "build.ninja: Jig sub/data\\ dir/VERSION a\\#b c$$d e\\\\ f\r\n";
        assert_eq!(parse(text).unwrap().inputs, ["Jig", "sub/data dir/VERSION", "a#b", "c$d", "e\\", "f"]);
    }

    #[test]
    fn takes_the_variables_rustc_lists() {
        // as rustc writes, with the variables `env!` and `option_env!` read, which are set or not
        let text = "lib.rlib: src/lib.rs\n\nsrc/lib.rs:\n\n\
                    # env-dep:FOO=bar=baz\n# env-dep:UNSET\n# a comment\n# env-dep:FOO\n";
        let discovered = parse(text).unwrap();
        assert_eq!(discovered.inputs, ["src/lib.rs"]);
        assert_eq!(discovered.env, ["FOO", "UNSET"]);
    }

    #[test]
    fn refuses_rules_without_targets() {
        assert!(parse("a.c b.c\n").is_err());
        assert!(parse("out: a : b\n").is_err());
        assert!(parse("out: a\nb\n").is_err());
    }
}
