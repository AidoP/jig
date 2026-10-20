use std::path::PathBuf;
use super::install::Install;
use super::syntax::Span;

pub struct Crate {
    pub name: String,
    pub name_span: Span,
    pub path: PathBuf,
    /// The target the crate is compiled for, which is a builtin target or one declared by a `target` statement.
    pub target: String,
    pub target_span: Span,
    /// The directory of the outputs for the target, `<build-dir>/<target>`.
    pub dir: PathBuf,
    pub kind: Kind,
    pub builtin: bool,
    /// Whether the crate is built by `ninja` without targets.
    pub default: bool,
    /// Whether the crate's tests are built as a test harness, and registered as a test named after the crate.
    pub test: bool,
    /// Whether the crate is documented by rustdoc, in `<build-dir>/<target>/doc/<name>`.
    pub doc: bool,
    /// The crates for the same target which are passed to this crate with `--extern`.
    pub uses: Vec<Use>,
    /// What is built before the crate and its test harness, and before its test runs, but is not an input.
    pub depends: Vec<PathBuf>,
    /// The configuration options passed with `--cfg`, as `<name>` or `<name>="<value>"`.
    pub cfg: Vec<String>,
    /// The environment variables rustc is run with, which `env!` reads.
    pub env: Vec<(String, String)>,
    /// The codegen options passed with `-C`, as `<option>=<value>`, from the properties set where it was declared.
    pub codegen: Vec<String>,
    /// The flags which give clippy's lints their levels, as `-W clippy::<lint>`, from the properties set where it was
    /// declared.
    pub lints: Vec<String>,
    /// The linker script passed to the linker with `-T`, which replaces that of the target.
    pub link_script: Option<PathBuf>,
    /// Where `jig install` installs the output, if it does.
    pub install: Install,
}

/// A crate used by another, from a `use` attribute.
#[derive(Clone)]
pub struct Use {
    /// The name of the crate, which is for the same target as the crate using it.
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Lib,
    Bin,
    /// A procedural macro, which is a library for the host target, which rustc loads.
    ProcMacro,
}

impl Kind {
    /// The `--crate-type` of a crate of the kind.
    pub fn crate_type(self) -> &'static str {
        match self {
            Kind::Lib => "lib",
            Kind::Bin => "bin",
            Kind::ProcMacro => "proc-macro",
        }
    }
}

impl Crate {
    /// The library or binary built from the crate.
    pub fn output(&self) -> PathBuf {
        match self.kind {
            Kind::Lib => self.dir.join(format!("lib{}.rlib", self.name)),
            Kind::Bin => self.dir.join(&self.name),
            // a dynamic library for the host, which configure runs on
            Kind::ProcMacro => self.dir.join(format!(
                "{}{}{}",
                std::env::consts::DLL_PREFIX,
                self.name,
                std::env::consts::DLL_SUFFIX,
            )),
        }
    }
    /// The name of the crate's test and documentation, which is the name of the crate, with its target if that is not
    /// the host target.
    pub fn short_name(&self, host: &str) -> String {
        if self.target == host { self.name.clone() } else { format!("{}@{}", self.name, self.target) }
    }
    /// The test harness of the crate, built with `--test`.
    pub fn test_harness(&self) -> PathBuf {
        self.dir.join("test").join(&self.name)
    }
}
