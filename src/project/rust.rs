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
    /// The edition rustc compiles it with, from its `edition` or the property where it is declared.
    pub edition: String,
    pub builtin: bool,
    /// Whether the crate is built by `jig build` without targets.
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
    /// The environment variables rustc inherits, which `env!` reads, whose values are inputs of the crate.
    pub pass_env: Vec<String>,
    /// How rig detects that the crate's files have changed, in place of `change-detection`, if it is given.
    pub change_detection: Option<String>,
    /// Whether rig caches what is built of the crate, when it caches, which `cache false` turns off.
    pub cached: bool,
    /// The codegen options passed with `-C`, as `<option>=<value>`, from the properties set where it was declared.
    pub codegen: Vec<String>,
    /// The flags which give clippy's lints their levels, as `-W clippy::<lint>`, from the properties set where it was
    /// declared.
    pub lints: Vec<String>,
    /// The linker script passed to the linker with `-T`, which replaces that of the target.
    pub link_script: Option<PathBuf>,
    /// Where `jig install` installs the output, if it does.
    pub install: Install,
    /// The tags a worker must have for rig to send it the crate's commands.
    pub worker_tags: Vec<String>,
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
    /// A static library for C, `lib<name>.a`, with the crates it uses and the standard library in it.
    StaticLib,
    /// A shared library for C, as a shared library of C is named.
    CDylib,
}

/// The editions of Rust which a crate may have.
pub const EDITIONS: [&str; 4] = ["2015", "2018", "2021", "2024"];

/// Returns the edition a value gives, as a string or a number, if it is one of [`EDITIONS`].
pub fn edition(data: &super::value::Data) -> Option<String> {
    use super::value::Data;
    let edition = match data {
        Data::Str(edition) => edition.clone(),
        Data::Unsigned(edition) => edition.to_string(),
        _ => return None,
    };
    EDITIONS.contains(&edition.as_str()).then_some(edition)
}

impl Kind {
    /// The `--crate-type` of a crate of the kind.
    pub fn crate_type(self) -> &'static str {
        match self {
            Kind::Lib => "lib",
            Kind::Bin => "bin",
            Kind::ProcMacro => "proc-macro",
            Kind::StaticLib => "staticlib",
            Kind::CDylib => "cdylib",
        }
    }
    /// Whether it is a library for C, which C links, rather than a crate which other crates use.
    pub fn is_for_c(self) -> bool {
        matches!(self, Kind::StaticLib | Kind::CDylib)
    }
}

impl Crate {
    /// The library or binary built from the crate.
    pub fn output(&self) -> PathBuf {
        match self.kind {
            Kind::Lib => self.dir.join(format!("lib{}.rlib", self.name)),
            Kind::Bin => self.dir.join(format!("{}{}", self.name, self.exe_suffix())),
            // a dynamic library for the host, which configure runs on
            Kind::ProcMacro => self.dir.join(format!(
                "{}{}{}",
                std::env::consts::DLL_PREFIX,
                self.name,
                std::env::consts::DLL_SUFFIX,
            )),
            Kind::StaticLib => self.dir.join(format!("lib{}.a", self.name)),
            // as the platform's linker finds it by its name
            Kind::CDylib if self.target.contains("-windows") => self.dir.join(format!("{}.dll", self.name)),
            Kind::CDylib if self.target.contains("-apple-") => self.dir.join(format!("lib{}.dylib", self.name)),
            Kind::CDylib => self.dir.join(format!("lib{}.so", self.name)),
        }
    }
    /// The file a static library for C is written with the libraries of the system it needs, which rustc writes by
    /// `--print=native-static-libs`, as arguments of the C compiler, which a link reads by `@<file>`.
    pub fn native_static_libs(&self) -> PathBuf {
        self.dir.join(format!("lib{}.a.libs", self.name))
    }
    /// The name of the crate's test and documentation, which is the name of the crate, with its target if that is not
    /// the host target.
    pub fn short_name(&self, host: &str) -> String {
        if self.target == host { self.name.clone() } else { format!("{}@{}", self.name, self.target) }
    }
    /// The metadata of a library, which rustc writes beside it before it has compiled it, which the libraries which use
    /// it are compiled against, so that they start once it is written.
    pub fn metadata(&self) -> PathBuf {
        self.dir.join(format!("lib{}.rmeta", self.name))
    }
    /// The library built again without `panic`, for the test harnesses and procedural macros which link it, which
    /// unwind.
    pub fn unwind_output(&self) -> PathBuf {
        self.dir.join("unwind").join(format!("lib{}.rlib", self.name))
    }
    /// The test harness of the crate, built with `--test`.
    pub fn test_harness(&self) -> PathBuf {
        self.dir.join("test").join(format!("{}{}", self.name, self.exe_suffix()))
    }
    /// The suffix of a program for the crate's target: `.exe` for Windows, as rustc writes one, and none otherwise.
    fn exe_suffix(&self) -> &'static str {
        if self.target.contains("-windows") { ".exe" } else { "" }
    }
}
