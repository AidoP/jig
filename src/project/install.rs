//! The `install`, `install-dir` and `install-mode` attributes of crates, custom commands and generated files, which
//! `jig install` copies the outputs of.

use std::path::{Path, PathBuf};

use super::{Project, rust};
use super::customs::CustomKind;
use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::syntax::Span;
use super::value::{Data, Operand};

/// Where and how the outputs of a crate or custom command are installed, with defaults depending on what it is.
#[derive(Clone)]
pub struct Install {
    /// Whether `jig install` installs the outputs.
    pub enabled: bool,
    /// The directory the outputs are installed in, relative to the prefix unless it is absolute.
    pub dir: String,
    /// The permissions of the installed files.
    pub mode: u32,
}
impl Install {
    /// The defaults for an executable, which is installed in `bin`.
    pub fn executable() -> Self {
        Self { enabled: false, dir: "bin".into(), mode: 0o755 }
    }
    /// The defaults for a library, which is installed in `lib`, with `mode`.
    pub fn library(mode: u32) -> Self {
        Self { enabled: false, dir: "lib".into(), mode }
    }
    /// The defaults for data, such as the outputs of a custom command, which is installed in `share`.
    pub fn data() -> Self {
        Self { enabled: false, dir: "share".into(), mode: 0o644 }
    }
}

/// A file which `jig install` installs, which is an output of a crate or custom command.
pub struct Installed {
    /// The output, relative to the project root.
    pub path: PathBuf,
    /// The directory it is installed in, relative to the prefix unless it is absolute.
    pub dir: String,
    pub mode: u32,
    /// Whether the file is a program, which `jig install --proxy` installs a wrapper for, rather than a link.
    pub program: bool,
    item: Item,
    /// The name of the crate, with its target, or the custom command.
    name: String,
    span: Span,
}
impl Installed {
    /// Where the file is installed, relative to the prefix unless it is absolute.
    fn destination(&self) -> PathBuf {
        Path::new(&self.dir).join(self.path.file_name().unwrap_or_default())
    }
}

/// The largest permissions of an installed file, including the setuid, setgid and sticky bits.
const MAX_MODE: u64 = 0o7777;

/// Returns the `rwx` bits of written permissions, before they are shifted to the position of a class, where `X` is `x`
/// if anyone can execute a file with `mode`, or [`None`] if one is not a permission.
fn class_bits(permissions: &str, mode: u32) -> Option<u32> {
    let mut rwx = 0;
    for c in permissions.chars() {
        match c {
            'r' => rwx |= 0o4,
            'w' => rwx |= 0o2,
            'x' => rwx |= 0o1,
            'X' if mode & 0o111 != 0 => rwx |= 0o1,
            'X' | 's' | 't' => (),
            _ => return None,
        }
    }
    Some(rwx)
}

/// Returns the special bit of a class if written permissions have it: setuid and setgid, `s`, are of the owner and the
/// group, while the sticky bit, `t`, is of others.
fn special_bits(permissions: &str, class: char, special: u32) -> u32 {
    let letter = if class == 'o' { 't' } else { 's' };
    if permissions.contains(letter) { special } else { 0 }
}

/// Returns `base` changed by a symbolic mode, as taken by chmod, or [`None`] if it is not one.
///
/// It is clauses separated by `,`, each of which is who it applies to, of `u`, `g`, `o` and `a`, or everyone without
/// any, followed by operations: `+`, `-` or `=`, and the permissions which are added, removed or set, of `r`, `w`, `x`,
/// `X`, which is `x` if anyone can execute it, `s` and `t`, or one of `u`, `g` and `o`, whose permissions are copied.
/// As there is no umask, `+x` is `a+x`.
fn apply_symbolic_mode(symbolic: &str, base: u32) -> Option<u32> {
    // the permissions of each class, in its position for the owner, and the special bit of each class
    const CLASSES: [(char, u32, u32); 3] = [('u', 6, 0o4000), ('g', 3, 0o2000), ('o', 0, 0o1000)];
    let mut mode = base;
    for clause in symbolic.split(',') {
        let operations = clause.trim_start_matches(['u', 'g', 'o', 'a']);
        let who = &clause[..clause.len() - operations.len()];
        let classes: Vec<_> = CLASSES.iter().filter(|(class, ..)| who.is_empty() || who.contains(['a', *class])).collect();
        let mut chars = operations.chars().peekable();
        // a clause needs an operation
        chars.peek()?;
        while let Some(operator) = chars.next() {
            if !matches!(operator, '+' | '-' | '=') {
                return None;
            }
            let mut permissions = String::new();
            while let Some(&c) = chars.peek() && !matches!(c, '+' | '-' | '=') {
                permissions.push(c);
                chars.next();
            }
            // the permissions of another class, as `rwx`, or those which are written
            let copied = match permissions.as_str() {
                "u" | "g" | "o" => {
                    let shift = CLASSES.iter().find(|(class, ..)| permissions.starts_with(*class)).unwrap().1;
                    Some((mode >> shift) & 0o7)
                },
                _ => None,
            };
            let mut bits = 0;
            for &&(class, shift, special) in &classes {
                bits |= match copied {
                    Some(rwx) => rwx << shift,
                    None => class_bits(&permissions, mode)? << shift | special_bits(&permissions, class, special),
                };
            }
            match operator {
                '+' => mode |= bits,
                '-' => mode &= !bits,
                _ => {
                    // the permissions of the classes are replaced, with their special bits
                    let cleared = classes.iter().fold(0, |cleared, &&(_, shift, special)| cleared | (0o7 << shift) | special);
                    mode = (mode & !cleared) | bits;
                },
            }
        }
    }
    Some(mode)
}

impl Project {
    /// Take the value of an install attribute, `install`, `install-dir` or `install-mode`, which is `none` for an
    /// attribute without a value.
    pub(super) fn install_attribute(&mut self, install: &mut Install, name: &str, operand: Operand) {
        match name {
            "install" => {
                if let Some(enabled) = self.flag(&operand) {
                    install.enabled = enabled;
                }
            },
            "install-dir" => {
                if let Some(dir) = self.str_operand(operand) {
                    install.dir = dir;
                }
            },
            // a symbolic mode changes the default, or the mode it was given before
            "install-mode" => {
                if let Some(mode) = self.mode_operand(operand, install.mode) {
                    install.mode = mode;
                }
            },
            _ => unreachable!("not an install attribute: {name}"),
        }
    }
    /// Returns the permissions of a file from an operand, which is an integer, such as `0o755`, or a symbolic mode, as
    /// taken by chmod, such as `'u+x'`, which changes `base`, reporting a value which cannot be one.
    pub(super) fn mode_operand(&mut self, operand: Operand, base: u32) -> Option<u32> {
        let span = operand.span;
        if let Data::Str(symbolic) = &operand.data {
            let mode = apply_symbolic_mode(symbolic, base);
            if mode.is_none() {
                self.invalid_mode(format!("{symbolic:?}"), span);
            }
            return mode;
        }
        let mode = match self.integer_operand(operand)?.data {
            Data::Unsigned(mode) => mode,
            Data::Signed(mode) if mode >= 0 => mode.unsigned_abs(),
            data => {
                self.invalid_mode(data.coerce_string(), span);
                return None;
            },
        };
        if mode > MAX_MODE {
            self.invalid_mode(format!("0o{mode:o}"), span);
            return None;
        }
        Some(mode as u32)
    }
    /// Returns the files which are installed, those of crates in the order they were declared, then those of custom
    /// commands, then generated files.
    pub fn installed(&self) -> Vec<Installed> {
        let mut installed = Vec::new();
        for key in self.crate_keys() {
            let c = &self.crates[&key];
            if c.install.enabled {
                installed.push(Installed {
                    path: c.output(),
                    dir: c.install.dir.clone(),
                    mode: c.install.mode,
                    program: c.kind == rust::Kind::Bin,
                    item: Item::Crate,
                    name: format!("{}/{}", c.target, c.name),
                    span: c.name_span,
                });
            }
        }
        for custom in &self.customs {
            let CustomKind::Command { outputs, install, .. } = &custom.kind else {
                continue;
            };
            if !install.enabled {
                continue;
            }
            for path in outputs {
                installed.push(Installed {
                    path: path.clone(),
                    dir: install.dir.clone(),
                    mode: install.mode,
                    // the output of a custom command is taken to be a program if it is installed as one
                    program: install.mode & 0o111 != 0,
                    item: Item::CustomCommand,
                    name: custom.name.clone(),
                    span: custom.name_span,
                });
            }
        }
        for file in &self.generated {
            if file.install.enabled {
                installed.push(Installed {
                    path: file.output.clone(),
                    dir: file.install.dir.clone(),
                    mode: file.install.mode,
                    program: file.install.mode & 0o111 != 0,
                    item: Item::File,
                    name: file.name.clone(),
                    span: file.name_span,
                });
            }
        }
        installed
    }
    /// Report files which are installed at the same destination as another, such as a binary crate for several
    /// targets, which would replace each other.
    pub(super) fn check_installs(&mut self) {
        let installed = self.installed();
        for (i, file) in installed.iter().enumerate() {
            let destination = file.destination();
            let Some(other) = installed[..i].iter().find(|other| other.destination() == destination) else {
                continue;
            };
            self.diagnostics.push(Diagnostic {
                span: Some(file.span),
                message: Message::InstallConflict {
                    path: destination.display().to_string(),
                    item: file.item,
                    name: file.name.clone(),
                    other_item: other.item,
                    other_name: other.name.clone(),
                    declared: other.span,
                },
                severity: Severity::Error,
            });
        }
    }
    fn invalid_mode(&mut self, mode: String, span: Span) {
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::InvalidMode(mode),
            severity: Severity::Error,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::apply_symbolic_mode;

    #[test]
    fn symbolic_modes() {
        let mode = |symbolic: &str, base: u32| apply_symbolic_mode(symbolic, base);
        // without who it applies to everyone, as there is no umask
        assert_eq!(mode("+x", 0o644), Some(0o755));
        assert_eq!(mode("u+x", 0o644), Some(0o744));
        assert_eq!(mode("go-w", 0o666), Some(0o644));
        assert_eq!(mode("a=r,u+w", 0o777), Some(0o644));
        assert_eq!(mode("u=rwx,g=rx,o=", 0), Some(0o750));
        // several operations in a clause, and the permissions of another class
        assert_eq!(mode("u+x-w", 0o644), Some(0o544));
        assert_eq!(mode("g=u", 0o640), Some(0o660));
        assert_eq!(mode("+r=o", 0o640), Some(0o444));
        // `X` is `x` only if someone can execute it
        assert_eq!(mode("+X", 0o644), Some(0o644));
        assert_eq!(mode("+X", 0o744), Some(0o755));
        // the special bits, which `=` replaces with the rest
        assert_eq!(mode("u+s", 0o755), Some(0o4755));
        assert_eq!(mode("+t", 0o755), Some(0o1755));
        assert_eq!(mode("u=rw", 0o4755), Some(0o655));
        for invalid in ["", "x", "u", "u+q", "u+x,", "g=uw", "+ro"] {
            assert_eq!(mode(invalid, 0o644), None, "{invalid:?}");
        }
    }
}
