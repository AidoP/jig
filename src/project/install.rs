//! The `install`, `install-dir` and `install-mode` attributes of crates and custom commands, which `jig install` copies
//! the outputs of.

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

impl Project {
    /// Take the value of an install attribute, `install`, `install-dir` or `install-mode`, which is `none` for an
    /// attribute without a value.
    pub(super) fn install_attribute(&mut self, install: &mut Install, name: &str, operand: Operand) {
        match name {
            "install" => {
                if let Some(enabled) = self.flag(operand) {
                    install.enabled = enabled;
                }
            },
            "install-dir" => {
                if let Some(dir) = self.str_operand(operand) {
                    install.dir = dir;
                }
            },
            "install-mode" => {
                let span = operand.span;
                let mode = match self.integer_operand(operand).map(|operand| operand.data) {
                    Some(Data::Unsigned(mode)) => mode,
                    Some(Data::Signed(mode)) if mode >= 0 => mode as u64,
                    Some(data) => return self.invalid_install_mode(data.coerce_string(), span),
                    None => return,
                };
                if mode > MAX_MODE {
                    return self.invalid_install_mode(format!("0o{mode:o}"), span);
                }
                install.mode = mode as u32;
            },
            _ => unreachable!("not an install attribute: {name}"),
        }
    }
    /// Returns the files which are installed, those of crates in the order they were declared, then those of custom
    /// commands.
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
    fn invalid_install_mode(&mut self, mode: String, span: Span) {
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::InvalidInstallMode(mode),
            severity: Severity::Error,
        });
    }
}
