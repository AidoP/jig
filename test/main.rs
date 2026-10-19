//! Integration tests for `jig`, the configure tool.
//!
//! Each test writes a project to a temporary directory, runs `jig` on its `Jig` file from that directory, and checks
//! the diagnostics and the generated `build/build.ninja`.
//!
//! The `jig` binary is found with the `CONFIGURE` environment variable, or as the `jig` crate's output, in the target
//! directory which contains the directory of this test harness.

use std::path::PathBuf;
use std::process::Command;

/// A project in a temporary directory, removed when dropped.
struct Project {
    dir: PathBuf,
}
impl Project {
    /// Create a project from `(path, contents)` pairs. The first file should be the root `Jig`.
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("jig-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, contents) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        // the paths outside the build directory are absolute, as `jig` sees them
        let dir = dir.canonicalize().unwrap();
        Self { dir }
    }
    fn configure(&self) -> Output {
        self.configure_with(&[])
    }
    /// Run `configure` with environment variables.
    ///
    /// rustup finds its toolchains in `HOME` unless `RUSTUP_HOME` is set, so it is kept where `HOME` is replaced.
    fn configure_with(&self, env: &[(&str, &str)]) -> Output {
        let configure = configure_path();
        let mut command = Command::new(&configure);
        if env.iter().any(|&(name, _)| name == "HOME")
            && std::env::var_os("RUSTUP_HOME").is_none()
            && let Some(home) = std::env::var_os("HOME")
        {
            command.env("RUSTUP_HOME", PathBuf::from(home).join(".rustup"));
        }
        let output = command
            .args(["generate", "Jig"])
            .envs(env.iter().copied())
            .current_dir(&self.dir)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {configure:?}: {error}"));
        Output {
            success: output.status.success(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            ninja: self.ninja(),
        }
    }
    /// Run `configure check` for a label, returning stdout and stderr.
    #[track_caller]
    fn check(&self, label: &str) -> (String, String) {
        let output = Command::new(configure_path()).args(["check", label]).current_dir(&self.dir).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        // a failed check is still a successful flycheck
        assert!(output.status.success(), "check failed:\n{stdout}\n{stderr}");
        (stdout, stderr)
    }
    /// Returns `build/build.ninja`, where the paths outside the build directory, which are absolute, are made relative
    /// to the project again, so that they are as the configuration gives them.
    fn ninja(&self) -> String {
        self.ninja_in("build")
    }
    /// Returns `build.ninja` in a build directory, as [`Self::ninja`] does.
    fn ninja_in(&self, build_dir: &str) -> String {
        let ninja = std::fs::read_to_string(self.dir.join(build_dir).join("build.ninja")).unwrap_or_default();
        ninja.replace(&format!("{}/", self.dir.display()), "")
    }
    /// Returns `build/build.ninja.d`, with its paths made relative to the project as by [`Self::ninja`].
    fn depfile(&self) -> String {
        let depfile = std::fs::read_to_string(self.dir.join("build/build.ninja.d")).unwrap();
        depfile.replace(&format!("{}/", self.dir.display()), "")
    }
    /// The host triple, as seen by `rustc` from the project directory.
    fn host(&self) -> String {
        let output = Command::new("rustc").arg("--print=host-tuple").current_dir(&self.dir).output().unwrap();
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
}
fn configure_path() -> PathBuf {
    match std::env::var_os("CONFIGURE") {
        Some(path) => std::path::absolute(path).unwrap(),
        None => std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jig"),
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Output {
    success: bool,
    stderr: String,
    ninja: String,
}
impl Output {
    /// Assert that configuration succeeded, returning the output for further checks.
    #[track_caller]
    fn ok(self) -> Self {
        assert!(self.success, "configure failed:\n{}", self.stderr);
        self
    }
    /// Assert that configuration failed, returning the output for further checks.
    #[track_caller]
    fn failed(self) -> Self {
        assert!(!self.success, "configure succeeded:\n{}", self.stderr);
        self
    }
    /// Assert that stderr contains `expected`.
    #[track_caller]
    fn stderr(&self, expected: &str) -> &Self {
        assert!(self.stderr.contains(expected), "stderr does not contain {expected:?}:\n{}", self.stderr);
        self
    }
    /// Returns the `build` statement for an output, including its variables.
    #[track_caller]
    fn build(&self, output: &str) -> &str {
        let prefix = format!("build {output}:");
        let Some(start) = self.ninja.find(&prefix) else {
            panic!("no build statement for {output:?}:\n{}", self.ninja);
        };
        let rest = &self.ninja[start..];
        &rest[..rest.find("\n\n").unwrap_or(rest.len())]
    }
    /// Returns the outputs of the crates' `build` statements, rather than their checks.
    fn outputs(&self) -> Vec<&str> {
        let outputs = self.ninja.lines().filter_map(|line| line.strip_prefix("build ")?.split(':').next());
        outputs.filter(|output| output.ends_with(".rlib")).collect()
    }
}

/// Make an executable at `destination` from `source`, which may be the same file, so that it can be run.
///
/// The executable is a new file written by `cp` rather than by the tests' process, as a file it has open for writing is
/// held open by a process which another test forks until that process executes its program, and running the file in
/// the meantime fails with `ExecutableFileBusy`.
#[track_caller]
fn make_executable(source: &std::path::Path, destination: &std::path::Path) {
    let status = Command::new("sh")
        .args(["-c", "cp \"$1\" \"$2.tmp\" && chmod 755 \"$2.tmp\" && mv \"$2.tmp\" \"$2\"", "sh"])
        .args([source, destination])
        .status()
        .unwrap();
    assert!(status.success(), "failed to make {destination:?} executable");
}

#[track_caller]
fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "{needle:?} not found in:\n{haystack}");
}

/// Returns the output of `jig test` without how long the tests took, which varies, asserting that each result has it.
#[track_caller]
fn without_durations(stdout: &str) -> String {
    let mut lines = Vec::new();
    for line in stdout.lines() {
        let line = if line.starts_with("test result: ") {
            let Some((result, elapsed)) = line.split_once("; finished in ") else {
                panic!("no duration in {line:?}");
            };
            assert!(elapsed.ends_with('s'), "{line:?}");
            result
        } else if line.starts_with("test ") && line.ends_with("s)") && let Some(start) = line.rfind(" (") {
            &line[..start]
        } else {
            line
        };
        lines.push(line);
    }
    lines.join("\n") + "\n"
}

#[test]
fn log_evaluates_expressions() {
    let project = Project::new("log", &[("Jig", "\
declare pages
    type 'unsigned'
    default 4
log note 'size {pages * 0x1000} {{literal}}'
log warning 'careful'
")]);
    project.configure().ok().stderr("note: size 16384 {literal}").stderr("warning: careful");
}

#[test]
fn target_defaults_to_host() {
    let project = Project::new("target-default", &[("Jig", "crate app 'app.rs'\n")]);
    let host = project.host();
    let output = project.configure().ok();
    let build = output.build(&format!("{host}/libapp.rlib"));
    assert_contains(build, "crate-name = app");
    assert_contains(build, &format!("--target={host}"));
    assert_contains(build, &format!("-L dependency={host}"));
}

#[test]
fn target_is_resolved_at_declaration() {
    let project = Project::new("target-resolve", &[("Jig", "\
crate before 'x.rs'
set target 'riscv64gc-unknown-none-elf'
crate after 'x.rs'
crate attribute 'x.rs'
    target 'aarch64-unknown-none'
crate host 'x.rs'
    target host-target
log note 'host-target is {host-target}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr(&format!("note: host-target is {host}"));
    output.build(&format!("{host}/libbefore.rlib"));
    output.build("riscv64gc-unknown-none-elf/libafter.rlib");
    output.build("aarch64-unknown-none/libattribute.rlib");
    output.build(&format!("{host}/libhost.rlib"));
}

#[test]
fn crate_default_path() {
    let project = Project::new("crate-path", &[
        ("Jig", "use 'lib'\n"),
        ("lib/Jig", "declare suffix\n    type 'str'\n    default '2'\ncrate {'util{suffix}'}\n"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    assert_contains(output.build(&format!("{host}/libutil2.rlib")), ": rustc lib/util2/lib.rs");
}

#[test]
fn crates_are_keyed_by_target() {
    let project = Project::new("crate-key", &[("Jig", "\
crate core 'core.rs'
crate core 'core.rs'
    target 'riscv64gc-unknown-none-elf'
")]);
    let host = project.host();
    let output = project.configure().ok();
    let mut outputs = output.outputs();
    outputs.sort();
    // in the order of their names, as the host target may come before the other
    let mut expected = ["riscv64gc-unknown-none-elf/libcore.rlib".to_string(), format!("{host}/libcore.rlib")];
    expected.sort();
    assert_eq!(outputs, expected);
}

#[test]
fn crate_redeclaration() {
    let project = Project::new("crate-redeclare", &[("Jig", "\
crate dup 'x.rs'
crate dup 'y.rs'
")]);
    project.configure().failed()
        .stderr("error[redeclare-crate]: redeclaration of crate \"dup\"")
        .stderr(" --> Jig:2:7")
        .stderr("note: previously declared here\n --> Jig:1:7");
}

#[test]
fn builtin_crates_are_extern_by_default() {
    let project = Project::new("builtin", &[("Jig", "\
crate core 'core.rs'
    builtin
crate compiler_builtins 'cb.rs'
    builtin
    use core
crate app 'app.rs'
crate kernel 'kernel.rs'
    target 'riscv64gc-unknown-none-elf'
")]);
    let host = project.host();
    let output = project.configure().ok();
    let core = format!("{host}/libcore.rlib");
    let compiler_builtins = format!("{host}/libcompiler_builtins.rlib");

    let app = output.build(&format!("{host}/libapp.rlib"));
    assert_contains(app, &format!("| {compiler_builtins} {core}\n"));
    assert_contains(app, &format!("--extern compiler_builtins={compiler_builtins} --extern core={core}"));

    // builtins only receive the crates they use
    let core_build = output.build(&core);
    assert!(!core_build.contains("--extern"), "{core_build}");
    assert_contains(output.build(&compiler_builtins), &format!("--extern core={core}"));

    // builtins are only passed to crates for the same target
    let kernel = output.build("riscv64gc-unknown-none-elf/libkernel.rlib");
    assert!(!kernel.contains("--extern"), "{kernel}");
}

#[test]
fn use_adds_dependency() {
    let project = Project::new("use", &[
        ("Jig", "declare name\n    default 'util'\ncrate app 'app.rs'\n    use {name}\nuse 'util'\n"),
        ("util/Jig", "crate util\n"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    let util = format!("{host}/libutil.rlib");
    let app = output.build(&format!("{host}/libapp.rlib"));
    assert_contains(app, &format!("| {util}\n"));
    assert_contains(app, &format!("--extern util={util}"));
}

#[test]
fn use_unknown_crate() {
    let project = Project::new("use-unknown", &[("Jig", "\
crate util 'u.rs'
crate kernel 'k.rs'
    target 'riscv64gc-unknown-none-elf'
    use util
crate bad 'b.rs'
    use 5
    use 'quoted'
    use
    use {'a b'}
")]);
    project.configure().failed()
        .stderr("error[unknown-crate]: no crate \"util\" for target \"riscv64gc-unknown-none-elf\"\n --> Jig:4:9")
        .stderr("error[invalid-identifier]: invalid character '5' in identifier \"5\"\n --> Jig:6:9")
        .stderr("error[expected-identifier]: expected identifier\n --> Jig:7:9")
        .stderr("error[expected-exact]: expected \" \"\n --> Jig:8:8")
        .stderr("error[invalid-identifier]: invalid character ' ' in identifier \"a b\"\n --> Jig:9:9")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn check_errors_in_declaration_order() {
    let project = Project::new("check-order", &[("Jig", "\
crate a 'a.rs'
    use x1
    use x2
crate b 'b.rs'
    use x3
crate c 'c.rs'
    use x4
crate d 'd.rs'
    target ['nope-c', 'nope-a', 'nope-b']
set undeclared-a 1
set undeclared-b 2
set undeclared-c 3
")]);
    let output = project.configure().failed();
    // the crates and properties are kept in hash maps, so the errors are sorted
    let expected = [
        "unknown target \"nope-a\"",
        "unknown target \"nope-b\"",
        "unknown target \"nope-c\"",
        "property \"undeclared-a\" is undeclared",
        "property \"undeclared-b\" is undeclared",
        "property \"undeclared-c\" is undeclared",
        "no crate \"x1\"",
        "no crate \"x2\"",
        "no crate \"x3\"",
        "no crate \"x4\"",
    ];
    let mut position = 0;
    for message in expected {
        let Some(found) = output.stderr[position..].find(message) else {
            panic!("{message:?} not found in order in:\n{}", output.stderr);
        };
        position += found + message.len();
    }
}

#[test]
fn unknown_target() {
    let project = Project::new("target-unknown", &[("Jig", "\
crate a 'x.rs'
    target 'riscv64gc-unknown-none'
crate b 'x.rs'
    target 'nope'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[unknown-target]: unknown target \"riscv64gc-unknown-none\", did you mean \"riscv64gc-unknown-none-elf\"?\n --> Jig:2:12")
        .stderr("error[unknown-target]: unknown target \"nope\"\n --> Jig:4:12")
        .stderr("note: available targets are:\n      aarch64-")
        .stderr("\n      riscv64gc-unknown-none-elf\n");
    assert_eq!(output.stderr.matches("available targets are").count(), 1, "{}", output.stderr);
}

#[test]
fn invalid_crate_name() {
    let project = Project::new("crate-name", &[("Jig", "\
declare suffix
    type 'str'
    default 'x'
crate host-tool 'x.rs'
crate {'a-{suffix}'} 'x.rs'
crate café 'x.rs'
crate 2fast 'x.rs'
crate 'quoted' 'x.rs'
crate {'a b'} 'x.rs'
crate {''} 'x.rs'
crate compiler_builtins2 'x.rs'
crate Upper 'x.rs'
")]);
    project.configure().failed()
        // an identifier points at the character, an expression at the whole name
        .stderr("error[invalid-crate-name]: invalid character '-' in crate name \"host-tool\"\n --> Jig:4:11")
        .stderr("error[invalid-crate-name]: invalid character '-' in crate name \"a-x\"\n --> Jig:5:7")
        .stderr("error[invalid-identifier]: invalid character 'é' in identifier \"café\"\n --> Jig:6:10")
        .stderr("error[invalid-identifier]: invalid character '2' in identifier \"2fast\"\n --> Jig:7:7")
        .stderr("error[expected-identifier]: expected identifier\n --> Jig:8:7")
        .stderr("error[invalid-identifier]: invalid character ' ' in identifier \"a b\"\n --> Jig:9:7")
        .stderr("error[expected-identifier]: expected identifier\n  --> Jig:10:7")
        .stderr("configuration failed with 7 errors");
}

#[test]
fn invalid_identifier() {
    let project = Project::new("ident", &[("Jig", "\
declare my.prop
declare prop2
    type 'str'
    default 'v'
    défaut 'x'
Sét prop2 'y'
log note '{Prøp}'
declare 2go
set {5} 1
")]);
    project.configure().failed()
        .stderr("error[expected-exact]: expected \"\\n\"\n --> Jig:1:11")
        .stderr("error[invalid-identifier]: invalid character 'é' in identifier \"défaut\"\n --> Jig:5:6")
        .stderr("error[invalid-identifier]: invalid character 'é' in identifier \"Sét\"\n --> Jig:6:2")
        .stderr("error[invalid-identifier]: invalid character 'ø' in identifier \"Prøp\"\n --> Jig:7:14")
        .stderr("error[invalid-identifier]: invalid character '2' in identifier \"2go\"\n --> Jig:8:9")
        .stderr("error[invalid-identifier]: invalid character '5' in identifier \"5\"\n --> Jig:9:5")
        .stderr("configuration failed with 6 errors");
}

#[test]
fn computed_names() {
    let project = Project::new("names", &[("Jig", "\
declare my_prop
    default 'core'
declare suffix
    default 2
declare other_2
set {'other_{suffix}'} 1
log note 'other_2 is {other_2}'
crate compiler_builtins 'x.rs'
crate {'{my_prop}_ext'} 'y.rs'
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr("note: other_2 is 1");
    output.build(&format!("{host}/libcompiler_builtins.rlib"));
    output.build(&format!("{host}/libcore_ext.rlib"));
}

#[test]
fn writes_depfile() {
    let project = Project::new("depfile", &[
        ("Jig", "use 'a'\n"),
        ("a/Jig", "crate a 'a.rs'\n"),
    ]);
    project.configure().ok();
    let depfile = project.depfile();
    assert_eq!(depfile, "build.ninja: Jig a/Jig reconfigure.stamp\n");
}

#[test]
fn paths_through_symbolic_links() {
    let project = Project::new("symbolic-links", &[
        ("Jig", "use 'link/../sib'\nuse 'plain/../other'\n"),
        ("real/inner/.keep", ""),
        ("real/sib/Jig", "log note 'beside the target of the link'\n"),
        ("sib/Jig", "log note 'beside the link'\n"),
        ("other/Jig", ""),
        ("plain/.keep", ""),
    ]);
    std::os::unix::fs::symlink("real/inner", project.dir.join("link")).unwrap();
    let output = project.configure().ok();
    output.stderr("note: beside the target of the link\n");
    // ninja removes `..` with the component before it, so a symbolic link before `..` is resolved for it
    assert_eq!(project.depfile(), "build.ninja: Jig real/sib/Jig other/Jig reconfigure.stamp\n");
}

#[test]
fn read_only_property() {
    let project = Project::new("read-only", &[("Jig", "\
declare version
    type 'unsigned'
    default 3
    read-only
declare free
    default 1
set free 2
declare after
    read-only
set after 'a'
set before 'b'
declare before
    read-only
log note '{version} {free} {after} {before}'
")]);
    // a read-only property may be assigned once, by a default or by a `set` either side of the declaration
    project.configure().ok().stderr("note: 3 2 a b");
}

#[test]
fn set_read_only_property() {
    let project = Project::new("read-only-set", &[("Jig", "\
set host-target 'x'
declare version
    default 3
    read-only
set version 4
set early 'a'
set early 'b'
declare early
    read-only
set defaulted 'a'
declare defaulted
    default 'd'
    read-only
log note 'early is {early}'
log note 'defaulted is {defaulted}, by default {defaulted.default}'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[read-only-property]: cannot set read-only builtin property \"host-target\"\n --> Jig:1:5")
        .stderr("note: host-target: the target triple of the machine running the build\nerror")
        .stderr("error[read-only-property]: cannot set read-only property \"version\"\n --> Jig:5:5")
        .stderr("note: value assigned here\n --> Jig:3:5")
        .stderr("note: declared read-only here\n --> Jig:2:9")
        // only the first `set` before the declaration is permitted, and its value is kept
        .stderr("error[read-only-property]: cannot set read-only property \"early\"\n --> Jig:7:5")
        .stderr("note: value assigned here\n --> Jig:6:5")
        .stderr("note: early is a")
        // a `set` before the declaration replaces its default, even for a read-only property
        .stderr("note: defaulted is a, by default d")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn duplicate_declare() {
    let project = Project::new("declare-duplicate", &[("Jig", "\
set later 1
declare later
declare size
    type 'unsigned'
    default 1
declare size
    type 'str'
    default 'x'
declare host-target
    default 'y'
log note 'size is {size}'
")]);
    project.configure().failed()
        .stderr("error[redeclare-property]: redeclaration of property \"size\"\n --> Jig:6:9")
        .stderr("note: previously declared here\n --> Jig:3:9")
        .stderr("error[redeclare-property]: redeclaration of builtin property \"host-target\"\n --> Jig:9:9")
        .stderr("note: host-target: the target triple of the machine running the build\nnote: size is 1")
        // the attributes of a duplicate are ignored
        .stderr("note: size is 1")
        .stderr("configuration failed with 2 errors");
}

#[test]
fn meta_if() {
    let project = Project::new("meta-if", &[("Jig", "\
declare debug
    default false
declare level
    default 2
@if debug
set level 9
@if !debug
log note 'release'
@if level > 1
@if {'true'}
crate kept 'x.rs'
    target host-target
@if false
crate skipped 'x.rs'
    target host-target
@if false
@if missing
log note 'not evaluated'
log note 'level is {level}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr("note: release").stderr("note: level is 2");
    assert!(!output.stderr.contains("not evaluated"), "{}", output.stderr);
    // the attributes of a skipped statement are skipped with it
    assert_eq!(output.outputs(), [format!("{host}/libkept.rlib")]);
}

#[test]
fn meta_if_errors() {
    let project = Project::new("meta-if-errors", &[("Jig", "\
@if 1
log note 'unsigned condition'
@if true

log note 'after blank line'
@if true
# comment
log note 'after comment'
@elif true
log note 'unknown'
@if true
")]);
    let output = project.configure().failed();
    output
        .stderr("error[invalid-operand]: unsigned value used where bool is required\n --> Jig:1:5")
        .stderr("error[expected-statement-after-meta]: expected statement on the line after a meta-statement\n --> Jig:3:1")
        .stderr("error[expected-statement-after-meta]: expected statement on the line after a meta-statement\n --> Jig:6:1")
        .stderr("error[unknown-meta-statement]: unknown meta-statement \"@elif\"\n --> Jig:9:1")
        .stderr("error[expected-statement-after-meta]: expected statement on the line after a meta-statement\n  --> Jig:11:1")
        .stderr("configuration failed with 5 errors");
    // an invalid condition skips the statement
    assert!(!output.stderr.contains("note: unsigned condition"), "{}", output.stderr);
    assert!(!output.stderr.contains("note: unknown"), "{}", output.stderr);
}

#[test]
fn meta_else() {
    let project = Project::new("meta-else", &[("Jig", "\
declare level
    default 2
@if level == 1
log note 'one'
@else
@if level == 2
log note 'two'
@else
log note 'other'

# blank lines and comments may separate the statements of a chain
@if level == 2
log note 'if taken'

@else
log note 'else not taken'
@if false
log note 'x'
@else
@if false
log note 'y'
@else
log note 'final else'
@if false
@if missing
log note 'skipped'
@else
crate fallback 'x.rs'
    target host-target
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr("note: two").stderr("note: if taken").stderr("note: final else");
    for skipped in ["one", "other", "else not taken", "note: x", "note: y", "skipped"] {
        assert!(!output.stderr.contains(skipped), "{skipped:?} was not skipped:\n{}", output.stderr);
    }
    assert_eq!(output.outputs(), [format!("{host}/libfallback.rlib")]);
}

#[test]
fn meta_else_errors() {
    let project = Project::new("meta-else-errors", &[("Jig", "\
@else
log note 'no if'
@if true
log note 'a'
log note 'unconditional'
@else
log note 'chain ended'
@if true
@else
log note 'not first'
@if 1
log note 'bad condition'
@else
log note 'else after bad condition'
@else x
log note 'trailing'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[else-without-if]: \"@else\" must follow a conditional statement\n --> Jig:1:1")
        // an unconditional statement ends the chain
        .stderr("error[else-without-if]: \"@else\" must follow a conditional statement\n --> Jig:6:1")
        .stderr("error[else-not-first]: \"@else\" must be the first meta-statement of a statement\n --> Jig:9:1")
        .stderr("error[invalid-operand]: unsigned value used where bool is required\n  --> Jig:11:5")
        .stderr("error[expected-exact]: expected \"\\n\"\n  --> Jig:15:6")
        .stderr("configuration failed with 5 errors");
    // an invalid chain processes none of its statements
    for skipped in ["no if", "chain ended", "not first", "bad condition", "else after bad condition", "trailing"] {
        assert!(!output.stderr.contains(skipped), "{skipped:?} was not skipped:\n{}", output.stderr);
    }
}

#[test]
fn meta_else_if() {
    let project = Project::new("meta-else-if", &[("Jig", "\
declare level
    default 2
@if level == 1
log note 'one'
@else-if level == 2
log note 'two'
@else-if missing
log note 'not evaluated'
@else
log note 'other'
@if false
log note 'x'
@else-if {'false'}
log note 'y'
@else-if true
@if level > 1
log note 'nested condition'
")]);
    let output = project.configure().ok();
    output.stderr("note: two").stderr("note: nested condition");
    for skipped in ["one", "not evaluated", "other", "note: x", "note: y"] {
        assert!(!output.stderr.contains(skipped), "{skipped:?} was not skipped:\n{}", output.stderr);
    }
}

#[test]
fn meta_else_if_errors() {
    let project = Project::new("meta-else-if-errors", &[("Jig", "\
@else-if true
log note 'no if'
@if true
@else-if true
log note 'not first'
@if false
log note 'a'
@else-if 1
log note 'bad condition'
@else
log note 'else after bad condition'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[else-without-if]: \"@else-if\" must follow a conditional statement\n --> Jig:1:1")
        .stderr("error[else-not-first]: \"@else-if\" must be the first meta-statement of a statement\n --> Jig:4:1")
        .stderr("error[invalid-operand]: unsigned value used where bool is required\n --> Jig:8:10")
        .stderr("configuration failed with 3 errors");
    for skipped in ["no if", "not first", "bad condition", "else after bad condition"] {
        assert!(!output.stderr.contains(skipped), "{skipped:?} was not skipped:\n{}", output.stderr);
    }
}

#[test]
fn property_info() {
    let project = Project::new("property-info", &[("Jig", "\
declare pages
    type 'unsigned'
    info 'the number of pages in the heap'
declare pages
set pages 'many'
declare later
    info 'declared without a value'
log note '{later}'
set host-target 'x'
declare bad
    info 5
declare plain
declare plain
")]);
    let output = project.configure().failed();
    output
        .stderr("error[redeclare-property]: redeclaration of property \"pages\"\n --> Jig:4:9")
        .stderr("note: previously declared here\n --> Jig:1:9\n  |\n1 | declare pages\n  |         ^^^^^\nnote: pages: the number of pages in the heap\n")
        .stderr("error[mismatched-type]: invalid data type for property \"pages\"")
        .stderr("error[unset-property]: no value for property \"later\"\n --> Jig:8:12\n  |\n8 | log note '{later}'\n  |            ^^^^^\nnote: later: declared without a value\n")
        .stderr("note: host-target: the target triple of the machine running the build")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n  --> Jig:11:10")
        .stderr("error[redeclare-property]: redeclaration of property \"plain\"\n  --> Jig:13:9")
        .stderr("configuration failed with 6 errors");
    // once for each diagnostic about "pages"
    assert_eq!(output.stderr.matches("note: pages:").count(), 2, "{}", output.stderr);
    // no note for a property without info
    assert!(!output.stderr.contains("note: plain:"), "{}", output.stderr);
}

#[test]
fn type_error_origin() {
    let project = Project::new("type-origin", &[("Jig", "\
declare level
    type 'unsigned'
    default 2
declare name
    default 'x'
declare copy
    default name
declare count
    type 'unsigned'
set count copy
@if level
log note 'a'
@if level == name
log note 'b'
crate app 'x.rs'
    target level
@if {target} == 1
log note 'c'
")]);
    let output = project.configure().failed();
    let default_level = "note: unsigned value set here\n --> Jig:3:13\n  |\n3 |     default 2\n  |             ^\n";
    // a reference to a reference is followed to where the data was written
    let default_name = "note: str value set here\n --> Jig:5:13\n  |\n5 |     default 'x'\n  |             ^^^\n";
    output
        .stderr(&format!("error[mismatched-type]: invalid data type for property \"count\"\n  --> Jig:10:11\n   |\n10 | set count copy\n   |           ^^^^\n{default_name}"))
        .stderr(&format!("error[invalid-operand]: unsigned value used where bool is required\n  --> Jig:11:5\n   |\n11 | @if level\n   |     ^^^^^\n{default_level}"))
        .stderr(&format!("^^^^^^^^^^^^^\n{default_level}{default_name}"))
        .stderr(&format!("error[invalid-operand]: unsigned value used where str is required\n  --> Jig:16:12\n   |\n16 |     target level\n   |            ^^^^^\n{default_level}"))
        .stderr("configuration failed with 5 errors");
    // a builtin value has no location
    let builtin = "str value cannot be coerced to unsigned\n  --> Jig:17:5\n   |\n17 | @if {target} == 1\n   |     ^^^^^^^^\n";
    let after = &output.stderr[output.stderr.find(builtin).expect(&output.stderr) + builtin.len()..];
    assert!(!after.starts_with("note"), "{}", output.stderr);
}

#[test]
fn type_error_does_not_assign() {
    let project = Project::new("type-assign", &[("Jig", "\
declare count
    type 'unsigned'
    default 3
set count 'many'
log note 'count is {count}'
set early 'a'
set early 5
declare early
    type 'unsigned'
log note 'early is {early}'
declare backwards
    default 'x'
    type 'unsigned'
declare offset
    type 'signed'
    default 4
@if offset > -1
log note 'offset is signed'
declare locked
    default 'bad'
    type 'bool'
    read-only
set locked true
log note 'locked is {locked}'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[mismatched-type]: invalid data type for property \"count\"\n --> Jig:4:11")
        .stderr("note: declared here\n --> Jig:1:9")
        .stderr("note: count is 3")
        // a value set before the declaration is checked by it
        .stderr("error[mismatched-type]: invalid data type for property \"early\"\n --> Jig:6:11")
        .stderr("note: declared here\n --> Jig:8:9")
        .stderr("note: early is 5")
        // a default is checked against a type declared after it
        .stderr("error[mismatched-type]: invalid data type for property \"backwards\"\n  --> Jig:12:13")
        // an unsigned value is converted for a signed property
        .stderr("note: offset is signed")
        // an invalid default is not an assignment of a read-only property
        .stderr("error[mismatched-type]: invalid data type for property \"locked\"\n  --> Jig:20:13")
        .stderr("note: locked is true")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn default_is_visible_to_later_attributes() {
    let project = Project::new("default-visible", &[("Jig", "\
declare pages
    default 4
    info 'the number of pages, {pages} by default'
declare pages
")]);
    project.configure().failed().stderr("note: pages: the number of pages, 4 by default");
}

#[test]
fn functions() {
    let project = Project::new("functions", &[("Jig", "\
set diagnostic/impure 'allow'
declare arch
    default 'RiscV'
declare board
declare fallback
    default 'virt'
log note '{upper(arch)} {lower(arch)} {lower({5})}'
log note 'board is {select(board, fallback)}'
set board 'qemu'
log note 'board is now {select(board, fallback)}'
@if starts-with(lower(arch), 'risc') && ends-with(arch, 'V') && !ends-with(arch, 'x')
log note 'nested calls'
declare path
set path env('PATH')
log note 'path is {path}'
")]);
    let output = project.configure().ok();
    output
        .stderr("note: RISCV riscv 5")
        // unset properties are skipped
        .stderr("note: board is virt")
        .stderr("note: board is now qemu")
        .stderr("note: nested calls")
        .stderr(&format!("note: path is {}\n", std::env::var("PATH").unwrap()));
}

#[test]
fn function_errors() {
    let project = Project::new("function-errors", &[("Jig", "\
set diagnostic/impure 'allow'
declare a
set a select(a, typo)
set a select(a)
set a select()
set a upper('a', 'b')
set a upper(5)
set a nope(1)
set a upper('a',' b')
set a env('BLUEMETAL_CONFIGURE_TEST_UNSET')
@if upper('a')
log note 'x'
")]);
    project.configure().failed()
        // a misspelt property is not ignored
        .stderr("error[undeclared-property]: property \"typo\" is undeclared\n --> Jig:3:17")
        .stderr("error[none-selected]: no value for any of the properties \"a\"\n --> Jig:4:7")
        .stderr("error[argument-count]: function \"select\" takes at least 1 argument, but 0 were given\n --> Jig:5:7")
        .stderr("error[argument-count]: function \"upper\" takes 1 argument, but 2 were given\n --> Jig:6:7")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:7:13")
        .stderr("error[unknown-function]: unknown function \"nope\"\n --> Jig:8:7")
        .stderr("error[expected-separator]: expected \", \" or \")\"\n --> Jig:9:16")
        .stderr("error[environment-variable]: environment variable \"BLUEMETAL_CONFIGURE_TEST_UNSET\" is not set\n  --> Jig:10:11")
        .stderr("error[invalid-operand]: str value used where bool is required\n  --> Jig:11:5")
        .stderr("configuration failed with 9 errors");
}

#[test]
fn strings_in_templates() {
    let project = Project::new("template-strings", &[("Jig", "\
set diagnostic/impure 'allow'
declare arch
    default 'riscv'
log note 'path is {env('PATH')}'
log note '{upper('x')} and {lower('{upper(arch)}')}'
log note 'nested {'a {'b {upper('c')}'}'}'
log note 'braces {'{{'}{'}}'} {{x}}'
@if starts-with('{arch}64', 'riscv6')
log note 'condition'
")]);
    let output = project.configure().ok();
    output
        .stderr(&format!("note: path is {}\n", std::env::var("PATH").unwrap()))
        .stderr("note: X and riscv\n")
        .stderr("note: nested a b C\n")
        .stderr("note: braces {} {x}\n")
        .stderr("note: condition\n");
}

#[test]
fn strings_in_templates_errors() {
    let project = Project::new("template-strings-errors", &[("Jig", "\
log note 'bad {upper(5)} here'
log note 'inner {'{upper(5)}'}'
log note 'unclosed {'a'
")]);
    project.configure().failed()
        // spans within nested strings are relative to the file
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:1:22")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:2:26")
        .stderr("error[invalid-string-template]: invalid string template\n --> Jig:3:20");
}

#[test]
fn run_function() {
    let project = Project::new("run", &[("Jig", "\
set diagnostic/impure 'allow'
declare name
    default 'world'
log note '{run('sh', '-c', 'echo \"hello $WHO\"; echo', WHO: name)}|'
log note '{run('printf', '%s-%s', 'a', {5})}'
@if starts-with(run('sh', '-c', 'pwd'), '/')
log note 'condition'
")]);
    let output = project.configure().ok();
    // trailing newlines are removed
    output.stderr("note: hello world|\n").stderr("note: a-5\n").stderr("note: condition\n");
}

#[test]
fn run_errors() {
    let project = Project::new("run-errors", &[("Jig", "\
set diagnostic/impure 'allow'
declare a
set a run('sh', '-c', 'echo oops >&2; echo second >&2; exit 3')
set a run('bluemetal-no-such-program')
set a run('echo', X: 'a', 'late')
set a run('echo', 5)
set a run(X: 'a')
set a run('sh', '-c', 'printf \"\\377\"')
")]);
    project.configure().failed()
        .stderr("error[run-failed]: \"sh\" failed with exit status: 3\n --> Jig:3:7")
        // shown like a location
        .stderr("^\n --> stderr\n  |\n  | oops\n  | second\n  |\nerror")
        .stderr("error[run-spawn]: failed to run \"bluemetal-no-such-program\": ")
        .stderr("error[positional-after-env]: program arguments must come before environment variables\n --> Jig:5:27")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:6:19")
        .stderr("error[argument-count]: function \"run\" takes at least 1 argument, but 0 were given\n --> Jig:7:7")
        .stderr("error[run-output]: output of \"sh\" is not valid unicode\n --> Jig:8:7")
        .stderr("configuration failed with 6 errors");
}

#[test]
fn path_function() {
    let project = Project::new("path", &[
        ("Jig", "log note 'root {path('x.rs')} {path('./a/../../b')} {path('a/..')} {path('/x/../../y')}'\nuse 'sub'\n"),
        ("sub/Jig", "\
declare abs
    default '/abs/file'
log note 'sub {path('x.rs')} {path('../y')} {path(abs)} {'{path('t')}'}'
log note '{path({5})}'
"),
    ]);
    project.configure().ok()
        // `..` is resolved, but kept if it leaves a relative path's start, and removed at the root of an absolute one
        .stderr("note: root x.rs ../b . /y\n")
        // relative to the file's directory, and an absolute path is unchanged
        .stderr("note: sub sub/x.rs y /abs/file sub/t\n")
        .stderr("note: sub/5\n");
}

#[test]
fn uppercase_identifiers() {
    let project = Project::new("uppercase", &[("Jig", "\
declare Board_Name
    default 'virt'
set {'Arch_{Board_Name}'} 'riscv'
declare Arch_virt
log note '{Board_Name} {Arch_virt}'
crate Upper 'x.rs'
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr("note: virt riscv\n");
    output.build(&format!("{host}/libUpper.rlib"));
}

#[test]
fn run_environment_names() {
    let project = Project::new("run-env-names", &[("Jig", "\
set diagnostic/impure 'allow'
declare name
    default 'world'
log note '{run('sh', '-c', 'echo $NAME_world', {'NAME_{name}'}: 'computed')}'
log note '{run('sh', '-c', 'echo $CC $X-Y', CC: 'clang', name: name)}'
log note '{run('printenv', 'X-Y', X-Y: 'hyphen')}'
log note '{run('printf', {5})}|{run('printf', '%s', {'{5}'})}'
set name run('printenv', {'a b'}: 'x')
")]);
    project.configure().failed()
        // a name in `{}` is computed, like the name of a property
        .stderr("note: computed\n")
        .stderr("note: clang -Y\n")
        .stderr("note: hyphen\n")
        // an expression in `{}` is still an argument unless it is followed by `: `
        .stderr("note: 5|5\n")
        .stderr("error[invalid-identifier]: invalid character ' ' in identifier \"a b\"\n --> Jig:8:26")
        .stderr("configuration failed with 1 error");
}

#[test]
fn read_function() {
    let project = Project::new("read", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
log note 'version {read('data dir/VERSION')}|'
log note '{read('plain')} {read('../sub/plain')}'
"),
        ("sub/data dir/VERSION", "1.2.3\n\n"),
        ("sub/plain", "abc"),
    ]);
    // trailing newlines are removed
    project.configure().ok().stderr("note: version 1.2.3|\n").stderr("note: abc abc\n");
    // the configuration is regenerated when a file it read changes, with special characters escaped
    let depfile = project.depfile();
    assert_eq!(depfile, "build.ninja: Jig sub/Jig sub/data\\ dir/VERSION sub/plain reconfigure.stamp\n");
}

#[test]
fn read_errors() {
    let project = Project::new("read-errors", &[
        ("Jig", "declare a\nset a read('missing')\nset a read('binary')\nset a read(5)\n"),
        ("binary", "\u{0}\u{0}"),
    ]);
    std::fs::write(project.dir.join("binary"), b"\xff").unwrap();
    project.configure().failed()
        .stderr("error[read-failure]: failed to read \"missing\": ")
        .stderr("\n --> Jig:2:12")
        .stderr("error[read-failure]: failed to read \"binary\": stream did not contain valid UTF-8\n --> Jig:3:12")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:4:12")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn exists_function() {
    let project = Project::new("exists", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
@if exists('present')
log note 'present'
@if !exists('absent')
log note 'absent'
@if exists('inner')
log note 'directory'
@if !exists('no/such/file')
log note 'deep'
@if exists('../sub/present')
log note 'normalized'
"),
        ("sub/present", ""),
        ("sub/inner/file", ""),
    ]);
    project.configure().ok()
        .stderr("note: present\n")
        .stderr("note: absent\n")
        .stderr("note: directory\n")
        .stderr("note: deep\n")
        .stderr("note: normalized\n");
    // only paths which exist are dependencies, as a missing dependency would always be out of date
    let depfile = project.depfile();
    assert_eq!(depfile, "build.ninja: Jig sub/Jig sub/present sub/inner reconfigure.stamp\n");
}

#[test]
fn run_relative_program() {
    let project = Project::new("run-relative", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
set diagnostic/impure 'allow'
log note '{run('../tool.sh', 'arg')}'
log note '{run('./local.sh')}'
log note '{run('sh', '-c', 'echo from path')}'
"),
        ("tool.sh", "#!/bin/sh\necho \"tool $1\"\n"),
        ("sub/local.sh", "#!/bin/sh\necho local\n"),
    ]);
    for script in ["tool.sh", "sub/local.sh"] {
        make_executable(&project.dir.join(script), &project.dir.join(script));
    }
    // a program with a `/` is relative to the file, and the configuration depends on it
    project.configure().ok().stderr("note: tool arg\n").stderr("note: local\n").stderr("note: from path\n");
    // a program without a `/` is found in `PATH`, and is not a dependency
    let depfile = project.depfile();
    assert_eq!(depfile, "build.ninja: Jig sub/Jig tool.sh sub/local.sh reconfigure.stamp\n");
}

#[test]
fn which_function() {
    let project = Project::new("which", &[("Jig", "\
log note 'sh is {which('sh')}'
declare a
set a which('bluemetal-no-such-program')
set a which('a/b')
set a which('')
")]);
    let output = project.configure().failed();
    output
        .stderr("error[program-not-found]: program \"bluemetal-no-such-program\" not found in PATH\n --> Jig:3:13")
        .stderr("error[invalid-program-name]: invalid program name \"a/b\", which cannot contain \"/\"\n --> Jig:4:13")
        .stderr("error[invalid-program-name]: invalid program name \"\"")
        .stderr("configuration failed with 3 errors");
    // the first executable `sh` in `PATH`
    let path = std::env::var_os("PATH").unwrap();
    let sh = std::env::split_paths(&path).map(|dir| dir.join("sh")).find(|path| path.is_file()).unwrap();
    output.stderr(&format!("note: sh is {}\n", sh.display()));
}

#[test]
fn try_function() {
    let project = Project::new("try", &[("Jig", "\
set diagnostic/impure 'allow'
declare a
    default 'x'
log note try(env('BLUEMETAL_CONFIGURE_TEST_UNSET'), 'fallback')
@if try(env('BLUEMETAL_CONFIGURE_TEST_UNSET') == 'a', false)
log note 'not taken'
log note '{try(upper(a), 'never')} {try(upper(5), 1)} {try(missing, 2)} {try(1 + , 3)} {try(a b, 4)}'
log note try(get(['a', ')'], 5), [1, 2])
log note try(missing, try(also_missing, 'nested'))
log note try(upper(','), 'x')
")]);
    let output = project.configure().ok();
    output
        // errors are reported as warnings, and the default is the value
        .stderr("warning[environment-variable]: environment variable \"BLUEMETAL_CONFIGURE_TEST_UNSET\" is not set\n --> Jig:4:18")
        .stderr("note: fallback\n")
        .stderr("warning[invalid-operand]: unsigned value used where str is required\n --> Jig:7:47")
        .stderr("warning[unset-property]: no value for property \"missing\"")
        .stderr("warning[expected-expression]: expected expression")
        .stderr("warning[expected-separator]: expected \", \" or \")\"")
        // the rest of the argument is skipped after an error, even if it is in a string or brackets
        .stderr("note: X 1 2 3 4\n")
        .stderr("note: 1 2\n")
        .stderr("note: nested\n")
        .stderr("note: ,\n")
        .stderr("configuration raised 9 warnings");
    assert!(!output.stderr.contains("not taken"), "{}", output.stderr);
}

#[test]
fn try_errors() {
    let project = Project::new("try-errors", &[("Jig", "\
set diagnostic/impure 'allow'
declare a
set a try()
set a try(env('X'))
set a try(env('X'), nope)
set a try(upper('unclosed'
")]);
    // the default is required, and its errors, or those which leave the end of the argument unknown, are not suppressed
    project.configure().failed()
        .stderr("error[argument-count]: function \"try\" takes 2 arguments, but 1 was given\n --> Jig:3:7")
        .stderr("error[argument-count]: function \"try\" takes 2 arguments, but 1 was given\n --> Jig:4:7")
        .stderr("error[unset-property]: no value for property \"nope\"\n --> Jig:5:21")
        .stderr("error[expected-separator]: expected \", \" or \")\"\n --> Jig:6:27")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn impure_functions() {
    let project = Project::new("impure", &[("Jig", "\
declare a
set a env('PATH')
@if run('true') == ''
custom c
    command 'echo' run('echo', 'x')
log note try(env('PATH'), 'fallback')
@if false
log note 'skipped'
@else-if env('PATH') != ''
log note 'else'
")]);
    project.configure().failed()
        // the call is reported, with a suggestion to allow it for the statement
        .stderr("\
error[impure]: function \"env\" is impure, as the build is not reconfigured when its value changes
 --> Jig:2:7
  |
2 | set a env('PATH')
  |       ^^^^^^^^^^^
suggestion: allow it for the statement with `@with`
 --> Jig:2:1
  |
2 + @with diagnostic/impure as 'allow'
2 | set a env('PATH')
")
        // in a meta-statement, the suggestion is before the meta-statement
        .stderr("error[impure]: function \"run\" is impure, as the build is not reconfigured when its value changes\n --> Jig:3:5")
        .stderr("suggestion: allow it for the statement with `@with`\n --> Jig:3:1")
        // in an attribute, the suggestion is before the statement
        .stderr("error[impure]: function \"run\" is impure, as the build is not reconfigured when its value changes\n --> Jig:5:20")
        .stderr("suggestion: allow it for the statement with `@with`\n --> Jig:4:1")
        // `try` does not lower it, as evaluating it did not fail
        .stderr("error[impure]: function \"env\" is impure, as the build is not reconfigured when its value changes\n --> Jig:6:14")
        .stderr("note: /")
        // `@with` cannot be before `@else-if`, so there is no suggestion
        .stderr("error[impure]: function \"env\" is impure, as the build is not reconfigured when its value changes\n --> Jig:9:10\n  |\n9 | @else-if env('PATH') != ''\n  |          ^^^^^^^^^^^\nnote: else")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn impure_functions_allowed() {
    let project = Project::new("impure-allowed", &[
        ("Jig", "\
@with diagnostic/impure as 'allow'
log note 'home is {env('HOME')}'
@with diagnostic/impure as 'warning'
use 'sub'
log note run('echo', 'again')
"),
        ("sub/Jig", "log note run('echo', 'sub')\n"),
    ]);
    let output = project.configure_with(&[("HOME", "/home/test")]).failed();
    output
        .stderr("note: home is /home/test\n")
        // the property applies within files used by the statement
        .stderr("warning[impure]: function \"run\" is impure, as the build is not reconfigured when its value changes\n --> sub/Jig:1:10")
        .stderr("suggestion: allow it for the statement with `@with`\n --> sub/Jig:1:1")
        .stderr("note: sub\n")
        // and is restored after it
        .stderr("error[impure]: function \"run\" is impure, as the build is not reconfigured when its value changes\n --> Jig:5:10")
        .stderr("configuration failed with 1 error, 1 warning");
    // only the calls outside the first `@with` are reported
    assert_eq!(output.stderr.matches("[impure]").count(), 2, "{}", output.stderr);
}

#[test]
fn missing_paths_are_not_dependencies() {
    let project = Project::new("missing-dependencies", &[("Jig", "\
set diagnostic/impure 'allow'
@if try(run('./missing-program') == '', false)
log note 'ran'
@if try(read('missing-file') == '', false)
log note 'read'
")]);
    project.configure().ok().stderr("warning[run-spawn]: failed to run \"./missing-program\"");
    let depfile = project.depfile();
    assert_eq!(depfile, "build.ninja: Jig reconfigure.stamp\n");
}

#[test]
fn missing_paths_reconfigure() {
    // a path found missing is recorded, and once it exists, building regenerates `build.ninja` first
    let project = Project::new("missing-reconfigure", &[("Jig", "\
@if exists('extra')
log note 'extra exists'
@if try(read('data') == '', false)
log note 'data read'
")]);
    project.configure().ok();
    let build = || {
        let output = Command::new(configure_path()).arg("build").current_dir(&project.dir).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        String::from_utf8(output.stdout).unwrap() + &String::from_utf8(output.stderr).unwrap()
    };
    // ninja regenerates it the first time, as it has not recorded how it was made
    build();
    assert!(!build().contains("CONFIGURE"));
    std::fs::write(project.dir.join("extra"), "").unwrap();
    let output = build();
    assert_contains(&output, "CONFIGURE build.ninja");
    assert_contains(&output, "note: extra exists");
    // ninja keeps the dependencies in its log, rather than the depfile
    let deps = Command::new("ninja").args(["-t", "deps", "build.ninja"]).current_dir(project.dir.join("build")).output().unwrap();
    assert_contains(&String::from_utf8(deps.stdout).unwrap(), &format!("\n    {}/extra\n", project.dir.display()));
    // only once, as it is now a dependency
    assert!(!build().contains("CONFIGURE"));
    // a file which could not be read is missing too
    std::fs::write(project.dir.join("data"), "").unwrap();
    assert_contains(&build(), "note: data read");
    assert!(!build().contains("CONFIGURE"));
}

#[test]
fn log_quiet() {
    let project = Project::new("log-quiet", &[("Jig", "\
log note 'loud'
log note 'hushed'
    quiet
log note 'odd'
    loud
")]);
    let output = project.configure().failed();
    output
        .stderr("note: loud\n --> Jig:1:10\n")
        // a quiet message is shown without where it was logged
        .stderr("note: hushed\nerror[unknown-attribute]: unknown log attribute \"loud\"\n --> Jig:5:5\n")
        .stderr("note: odd\n --> Jig:4:10\n");
}

#[test]
fn use_file() {
    let project = Project::new("use-file", &[
        ("Jig", "use 'extra.jig'\n    file\nuse 'sub'\n"),
        ("extra.jig", "log note 'in extra'\n"),
        ("sub/Jig", "log note 'in sub'\n"),
    ]);
    // the file is used itself, while a directory is used by its root file
    project.configure().ok()
        .stderr("note: in extra\n --> extra.jig:1:10")
        .stderr("note: in sub\n --> sub/Jig:1:10");
    assert_eq!(project.depfile(), "build.ninja: Jig extra.jig sub/Jig reconfigure.stamp\n");

    let project = Project::new("use-file-errors", &[("Jig", "use 'odd'\n    odd\n"), ("odd/Jig", "")]);
    project.configure().failed()
        .stderr("error[unknown-attribute]: unknown use attribute \"odd\"\n --> Jig:2:5");
}

#[test]
fn use_paths() {
    let mut files = vec![
        ("Jig".to_string(), "use 'a'\nuse 'b'\nuse 'alias'\nuse 'link/../sib'\nuse 'cycle'\nuse 'f0'\n".to_string()),
        ("a/Jig".into(), "use '../shared'\n".into()),
        ("b/Jig".into(), "use '../b/./../shared'\n".into()),
        ("shared/Jig".into(), "log note 'shared'\n".into()),
        ("real/inner/.keep".into(), String::new()),
        ("real/sib/Jig".into(), "log note 'beside the target of the link'\n".into()),
        ("sib/Jig".into(), "log note 'beside the link'\n".into()),
        ("cycle/Jig".into(), "use '../cycle'\n".into()),
    ];
    // a chain of paths through `..` does not grow
    for i in 0..1000 {
        let next = if i < 999 { format!("use '../f{}'\n", i + 1) } else { "log note 'end of the chain'\n".into() };
        files.push((format!("f{i}/Jig"), next));
    }
    let files: Vec<_> = files.iter().map(|(path, contents)| (path.as_str(), contents.as_str())).collect();
    let project = Project::new("use-paths", &files);
    std::os::unix::fs::symlink("shared", project.dir.join("alias")).unwrap();
    std::os::unix::fs::symlink("real/inner", project.dir.join("link")).unwrap();
    let output = project.configure();
    output
        // `..` after a symbolic link leaves the directory it points to
        .stderr("note: beside the target of the link\n --> link/../sib/Jig:1:10")
        .stderr("warning[repeated-use]: file \"cycle/Jig\" is already used\n --> cycle/Jig:1:5")
        .stderr("note: end of the chain\n --> f999/Jig:1:10");
    assert!(output.success, "{}", output.stderr);
    assert!(!output.stderr.contains("beside the link\n"), "{}", output.stderr);
    // the file is used once, however its path is written, even through a symbolic link
    assert_eq!(output.stderr.matches("note: shared").count(), 1, "{}", output.stderr);
    let depfile = project.depfile();
    assert!(depfile.starts_with("build.ninja: Jig a/Jig shared/Jig b/Jig "), "{depfile}");
    assert!(depfile.contains(" f998/Jig f999/Jig reconfigure.stamp"), "{depfile}");
}

#[test]
fn log_expression() {
    let project = Project::new("log-expression", &[("Jig", "\
declare level
    default 2
log note level * 3
log note upper('loud')
log warning level > 1
log note (level)
log suggestion 'try {level}'
log note
")]);
    project.configure().failed()
        .stderr("note: 6\n --> Jig:3:10\n  |\n3 | log note level * 3\n  |          ^^^^^^^^^\n")
        .stderr("note: LOUD\n")
        .stderr("warning: true\n")
        .stderr("note: 2\n")
        // a suggestion is not counted
        .stderr("suggestion: try 2\n --> Jig:7:16")
        .stderr("error[expected-exact]: expected \" \"\n --> Jig:8:9")
        .stderr("configuration failed with 1 error, 1 warning");
}

#[test]
fn multi_line_message() {
    let project = Project::new("multi-line", &[("Jig", "\
set diagnostic/impure 'allow'
log warning run('printf', '%s\\n%s\\n\\n%s', 'first', 'second', 'after blank')
")]);
    // continuation lines are aligned with the first, except for blank lines
    project.configure().ok().stderr("\
warning: first
         second

         after blank
 --> Jig:2:13
");
}

#[test]
fn rust_project() {
    let project = Project::new("rust-project", &[("Jig", "\
crate core 'core.rs'
    builtin
crate util 'util.rs'
crate app 'app\"s.rs'
    use util
")]);
    let host = project.host();
    project.configure().ok();
    let json = std::fs::read_to_string(project.dir.join(".rust-project.json")).unwrap();
    let dir = std::path::absolute(&project.dir).unwrap();
    // crates are in the same order as their indices in `deps`, which match the `--extern` flags
    assert_contains(&json, &format!("\
        {{
            \"display_name\": \"app\",
            \"root_module\": \"{}/app\\\"s.rs\",
            \"edition\": \"2024\",
            \"target\": \"{host}\",
            \"is_workspace_member\": true,
            \"cfg\": [],
            \"build\": {{ \"label\": \"app@{host}\", \"build_file\": \"{}/Jig\", \"target_kind\": \"lib\" }},
            \"deps\": [{{ \"crate\": 1, \"name\": \"core\" }}, {{ \"crate\": 2, \"name\": \"util\" }}]
        }},", dir.display(), dir.display()));
    assert_contains(&json, "\"display_name\": \"core\",");
    assert_contains(&json, &format!("\"root_module\": \"{}/util.rs\"", dir.display()));
    // the configure tool is only part of a project with its source
    assert!(!json.contains("\"display_name\": \"configure\""), "{json}");
    assert_contains(&json, "\"sysroot_src\": \"");
    // the flycheck for a label replays its saved diagnostics
    assert_contains(&json, &format!("\
    \"runnables\": [
        {{
            \"kind\": \"flycheck\",
            \"program\": \"{}\",
            \"args\": [\"check\", \"{{label}}\"],
            \"cwd\": \"{}\"
        }}
    ],", configure_path().display(), dir.display()));
}

#[test]
fn flycheck_replays_diagnostics() {
    let project = Project::new("flycheck", &[
        ("Jig", "crate util 'util.rs'\ncrate app 'app.rs'\n    use util\n"),
        ("util.rs", "pub fn f() -> u32 { let unused = 1; 2 }\n"),
        ("app.rs", "pub fn g() -> u32 { util::f() }\n"),
    ]);
    let host = project.host();
    project.configure().ok();
    let ninja = project.configure().ninja;
    // checks are ordinary edges, which save their diagnostics
    assert_contains(&ninja, &format!("build check/app@{host}: phony {host}/check/libapp.rmeta\n"));
    assert_contains(&ninja, &format!("--extern util={host}/check/libutil.rmeta"));

    // the diagnostics of the crates it depends on are replayed too
    let (first, _) = project.check(&format!("app@{host}"));
    assert_contains(&first, "\"message\":\"unused variable: `unused`\"");
    let util = project.dir.join(format!("build/{host}/check/util.json"));
    let modified = std::fs::metadata(&util).unwrap().modified().unwrap();

    // an up to date check is not run again, but its diagnostics are still replayed
    let (second, stderr) = project.check(&format!("app@{host}"));
    assert_eq!(second, first);
    assert!(!stderr.contains("CHECK"), "{stderr}");
    assert_eq!(std::fs::metadata(&util).unwrap().modified().unwrap(), modified);

    // a failed check is reported, and runs again next time
    std::fs::write(project.dir.join("app.rs"), "pub fn g() -> u32 { util::f() + \"x\" }\n").unwrap();
    let (failed, _) = project.check(&format!("app@{host}"));
    assert_contains(&failed, "\"message\":\"cannot add `&str` to `u32`\"");
    assert_contains(&failed, "\"message\":\"unused variable: `unused`\"");
    let (again, stderr) = project.check(&format!("app@{host}"));
    assert_eq!(again, failed);
    assert_contains(&stderr, "FAILED");
}

#[test]
fn flycheck_problems() {
    let project = Project::new("flycheck-problems", &[("Jig", "crate app 'app.rs'\n"), ("app.rs", "")]);
    // without a configuration, which it generates first, without writing to stdout, which is for rust-analyzer
    let (stdout, stderr) = project.check(&format!("app@{}", project.host()));
    assert_eq!(stdout, "");
    assert!(!stderr.contains("error"), "{stderr}");
    assert!(project.dir.join(".jig").is_file());
    // with an unknown label
    let (stdout, stderr) = project.check("nope");
    assert_eq!(stdout, "");
    assert_contains(&stderr, "no crate with the label \"nope\"");
}

#[test]
fn custom_command() {
    let project = Project::new("custom", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
custom copy
    input 'in.txt'
    output '../out/copy.txt'
    command 'sh' '-c' 'cat \"$1\" > \"$2\"; echo \"a  b\" >> \"$2\"' 'sh' input output
custom tool
    output '../out/tool.txt'
    command './tool.sh' output 'a b' 4 * 2
"),
        ("sub/in.txt", "hello\n"),
        ("sub/tool.sh", "#!/bin/sh\necho \"tool: $*\" > \"$1\"\n"),
    ]);
    make_executable(&project.dir.join("sub/tool.sh"), &project.dir.join("sub/tool.sh"));
    let output = project.configure().ok();
    // `input` and `output` are the paths, and the arguments are quoted for `sh`, with `$` escaped for ninja
    assert_contains(output.build("out/copy.txt"), "\
build out/copy.txt: custom sub/in.txt
    command-line = 'sh' '-c' 'cat \"$$1\" > \"$$2\"; echo \"a  b\" >> \"$$2\"' 'sh' 'sub/in.txt' 'out/copy.txt'
    name = copy");
    // a program which is a path is relative to the file, and a dependency
    assert_contains(output.build("out/tool.txt"), "\
build out/tool.txt: custom | sub/tool.sh
    command-line = './sub/tool.sh' 'out/tool.txt' 'a b' '8'");
    // a command's outputs can be built by its name
    assert_contains(&output.ninja, "\nbuild custom/copy: phony out/copy.txt\n");
    let ninja = Command::new(configure_path()).args(["build", "custom/copy", "custom/tool"]).current_dir(&project.dir).output().unwrap();
    assert!(ninja.status.success(), "{}", String::from_utf8_lossy(&ninja.stdout));
    assert_eq!(std::fs::read_to_string(project.dir.join("out/copy.txt")).unwrap(), "hello\na  b\n");
    assert_eq!(std::fs::read_to_string(project.dir.join("out/tool.txt")).unwrap(), "tool: out/tool.txt a b 8\n");
}

#[test]
fn custom_errors() {
    let project = Project::new("custom-errors", &[("Jig", "\
custom a
    output 'x'
custom b
    command 'true'
custom c
    output 'y'
    command 'true'
    command 'false'
    bogus 1
custom c
    output 'z'
    command 'true'
custom d
    command 'echo' input
    input 'late'
    output 'w'
log note '{try(input, false)}'
")]);
    project.configure().failed()
        .stderr("error[missing-attribute]: missing required attribute \"command\"\n --> Jig:1:8")
        .stderr("error[repeated-attribute]: attribute \"command\" cannot be repeated\n --> Jig:8:5")
        .stderr("error[unknown-attribute]: unknown custom attribute \"bogus\"\n --> Jig:9:5")
        .stderr("error[redeclare-custom]: redeclaration of custom command \"c\"\n  --> Jig:10:8")
        .stderr("note: previously declared here\n --> Jig:5:8")
        // `input` is only set by the attributes before it, and only within the statement
        .stderr("error[unset-property]: no value for property \"input\"\n  --> Jig:14:20")
        .stderr("note: false")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn default_targets() {
    let project = Project::new("default", &[("Jig", "\
crate kept 'kept.rs'
    default
crate other 'other.rs'
custom made
    output 'made.txt'
    command 'touch' output
    default
custom skipped
    output 'skipped.txt'
    command 'touch' output
"), ("kept.rs", ""), ("other.rs", "")]);
    let host = project.host();
    let output = project.configure().ok();
    assert_contains(&output.ninja, &format!("\ndefault custom/made {host}/libkept.rlib\n"));
    // `ninja` without targets only builds the defaults, once it has recorded how `build.ninja` was generated
    Command::new("ninja").arg("build.ninja").current_dir(project.dir.join("build")).output().unwrap();
    let ninja = Command::new("ninja").arg("-n").current_dir(project.dir.join("build")).output().unwrap();
    let ninja = String::from_utf8(ninja.stdout).unwrap();
    assert_contains(&ninja, "CUSTOM made");
    assert_contains(&ninja, &format!("RUSTC {host}/libkept.rlib"));
    assert!(!ninja.contains("skipped") && !ninja.contains("other"), "{ninja}");

    // without defaults, everything is built but the checks, which are for rust-analyzer
    let project = Project::new("no-default", &[
        ("Jig", "crate other 'other.rs'\n    test\ncustom data\n    command 'true'\n"),
        ("other.rs", ""),
    ]);
    let output = project.configure().ok();
    assert_contains(&output.ninja, &format!("\ndefault custom/data {host}/libother.rlib {host}/test/other\n"));
    let build = Command::new(configure_path()).arg("build").current_dir(&project.dir).output().unwrap();
    let build = String::from_utf8(build.stdout).unwrap();
    assert_contains(&build, &format!("RUSTC {host}/libother.rlib"));
    assert!(!build.contains("CHECK"), "{build}");
}

#[test]
fn lists() {
    let project = Project::new("lists", &[("Jig", "\
set diagnostic/impure 'allow'
declare flags
    type 'list'
    default ['-O', 'dtb']
declare more
    default flags + ['-o', 1 + 1, true]
log note '{more} | {flags == ['-O', 'dtb']} | {[] == []} | {[[1], 'x']}'
log note run('printf', '%s,', flags, ['a b', []])
custom pack
    input ['a.dts', 'b.dtsi']
    input 'c.dtsi'
    output 'out.bin'
    command 'tool' flags '-o' output input
")]);
    let output = project.configure().ok();
    // as a string, the items of a list are separated by spaces
    output.stderr("note: -O dtb -o 2 true | true | true | 1 x\n");
    // each item of a list is a separate argument
    output.stderr("note: -O,dtb,a b,\n");
    assert_contains(output.build("out.bin"), "\
build out.bin: custom a.dts b.dtsi c.dtsi
    command-line = 'tool' '-O' 'dtb' '-o' 'build/out.bin' 'a.dts' 'b.dtsi' 'c.dtsi'");
}

#[test]
fn list_errors() {
    let project = Project::new("list-errors", &[("Jig", "\
declare flags
    type 'list'
    default ['a']
declare bad
    type 'list'
    default 'x'
log note [1, 2
log note [1,2]
log note flags + 'x'
@if flags
log note 'x'
")]);
    project.configure().failed()
        .stderr("error[mismatched-type]: invalid data type for property \"bad\"\n --> Jig:6:13")
        .stderr("error[expected-separator]: expected \", \" or \"]\"\n --> Jig:7:15")
        .stderr("error[expected-separator]: expected \", \" or \"]\"\n --> Jig:8:12")
        .stderr("error[mismatched-operands]: mismatched operand types list and str\n --> Jig:9:10")
        .stderr("note: list value set here\n --> Jig:3:13")
        .stderr("error[invalid-operand]: list value used where bool is required\n  --> Jig:10:5")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn list_functions() {
    let project = Project::new("list-functions", &[("Jig", "\
declare flags
    default ['-O', 'dtb', 2, -1]
log note '{len(flags)} {len([])} {len([[1, 2]])} {len('abc')} {len('')} {len('é')}'
log note '{contains(flags, 'dtb')} {contains(flags, 'x')} {contains(flags, 2)} {contains(flags, -1)} {contains([[1]], [1])}'
@if len(flags) > 3 && contains(flags, '-O')
log note 'condition'
declare a
set a len(5)
set a contains('abc', 'a')
set a contains(flags)
")]);
    project.configure().failed()
        // the length of a string is in bytes
        .stderr("note: 4 0 1 3 0 2\n")
        .stderr("note: true false true true true\n")
        .stderr("note: condition\n")
        .stderr("error[invalid-operand]: unsigned value used where list, dict or str is required\n --> Jig:8:11")
        .stderr("error[invalid-operand]: str value used where list or dict is required\n --> Jig:9:16")
        .stderr("error[argument-count]: function \"contains\" takes 2 arguments, but 1 was given")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn tests() {
    let project = Project::new("tests", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
custom gen
    output 'gen.txt'
    command 'sh' '-c' 'echo hello > \"$1\"' 'sh' output
test generated
    input gen
    command './check.sh' input
test fails
    command 'sh' '-c' 'echo some output; echo an error >&2; exit 3'
test passes
    command 'true'
test arguments
    command 'sh' '-c' 'echo \"arguments:$*\"' 'sh'
"),
        ("sub/check.sh", "#!/bin/sh\ngrep -q hello \"$1\"\n"),
    ]);
    make_executable(&project.dir.join("sub/check.sh"), &project.dir.join("sub/check.sh"));
    let output = project.configure().ok();
    // a test's target builds its inputs and program, and `test` builds those of every test
    assert_contains(&output.ninja, "\nbuild test/generated: phony gen.txt sub/check.sh\n");
    assert_contains(&output.ninja, "\nbuild test: phony test/generated test/fails test/passes test/arguments\n");

    let run = |names: &[&str]| {
        let output = Command::new(configure_path()).arg("test").args(names).current_dir(&project.dir).output().unwrap();
        (output.status.success(), String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap())
    };
    // the inputs are built before the tests run, and the output of a failed test is shown
    let (success, stdout, stderr) = run(&["generated", "fails", "passes"]);
    assert!(!success);
    // the results line up, with how long each test took
    assert_contains(&without_durations(&stdout), "
running 3 tests
test generated ... ok
test fails     ... FAILED
test passes    ... ok

---- fails ----
exit status: 3
some output
an error

failures:
    fails

test result: FAILED. 2 passed; 1 failed
");
    // without colours, as stdout is not a terminal
    assert!(!stdout.contains('\x1b'), "{stdout}");
    assert_contains(&stdout, "\ntest passes    ... ok (0.");
    assert_contains(&stderr, "1 test failed");
    // tests can be chosen by name
    let (success, stdout, _) = run(&["passes", "generated"]);
    assert!(success);
    assert_contains(
        &without_durations(&stdout),
        "\nrunning 2 tests\ntest passes    ... ok\ntest generated ... ok\n\ntest result: ok. 2 passed; 0 failed\n",
    );
    // with `-v`, the output of each test is shown below its result, rather than that of the failures at the end
    let (success, stdout, _) = run(&["-v", "fails", "passes"]);
    assert!(!success);
    assert_contains(&without_durations(&stdout), "\
test fails  ... FAILED
    exit status: 3
    some output
    an error
test passes ... ok

failures:
    fails
");
    assert!(!stdout.contains("---- fails ----"), "{stdout}");
    // only a crate's test harness is given libtest's `--color`
    let (success, stdout, _) = run(&["-v", "arguments"]);
    assert!(success);
    assert_contains(&stdout, "\n    arguments:\n");
    let (success, _, stderr) = run(&["nope"]);
    assert!(!success);
    assert_contains(&stderr, "no test named \"nope\"");
}

#[test]
fn test_errors() {
    let project = Project::new("test-errors", &[("Jig", "\
test bad
    output 'x'
    default
    command 'true'
test a
    command 'true'
test a
    command 'false'
custom a
    output 'y'
    command 'true'
test empty
    input 'x'
")]);
    project.configure().failed()
        .stderr("error[unknown-attribute]: unknown test attribute \"output\"\n --> Jig:2:5")
        .stderr("error[unknown-attribute]: unknown test attribute \"default\"\n --> Jig:3:5")
        // tests and custom commands have separate names
        .stderr("error[redeclare-custom]: redeclaration of test \"a\"\n --> Jig:7:6")
        .stderr("error[missing-attribute]: missing required attribute \"command\"\n  --> Jig:12:6")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn crate_tests() {
    let project = Project::new("crate-tests", &[
        ("Jig", "crate util 'util.rs'\ncrate app 'app.rs'\n    use util\n    test\n"),
        ("util.rs", "pub fn two() -> u32 { 2 }\n"),
        ("app.rs", "\
pub fn four() -> u32 { util::two() * 2 }
#[cfg(test)]
mod tests {
    #[test]
    fn works() { assert_eq!(super::four(), 4); }
    #[test]
    fn broken() { assert_eq!(super::four(), 5); }
}
"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    // the test harness is built like the crate, with `--test`, and is registered as a test named after the crate
    assert_contains(output.build(&format!("{host}/test/app")), &format!(
        "--extern util={host}/libutil.rlib --test",
    ));
    assert_contains(&output.ninja, &format!("\nbuild test/app: phony {host}/test/app\n"));
    let run = Command::new(configure_path()).arg("test").current_dir(&project.dir).output().unwrap();
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert!(!run.status.success());
    let stdout = without_durations(&stdout);
    assert_contains(&stdout, "\nrunning 1 test\ntest app ... FAILED\n");
    // the harness's own output is shown, where its tests run in any order
    assert_contains(&stdout, "test tests::works ... ok\n");
    assert_contains(&stdout, "test tests::broken ... FAILED\n");
    assert_contains(&stdout, "\ntest result: FAILED. 0 passed; 1 failed\n");
    // the results of the harness's tests are shown below its own with `-v`
    let run = Command::new(configure_path()).args(["test", "--verbose"]).current_dir(&project.dir).output().unwrap();
    let stdout = without_durations(&String::from_utf8(run.stdout).unwrap());
    assert_contains(&stdout, "\ntest app ... FAILED\n    exit status: 101\n\n    running 2 tests\n");
    assert_contains(&stdout, "\n    test tests::works ... ok\n");
    assert_contains(&stdout, "\n    test result: FAILED. 1 passed; 1 failed;");
    // which is not coloured, as stdout is not a terminal
    assert!(!stdout.contains('\x1b'), "{stdout}");
}

#[test]
fn crate_test_errors() {
    let project = Project::new("crate-test-errors", &[("Jig", "\
crate k 'k.rs'
    target 'riscv64gc-unknown-none-elf'
    test
crate app 'app.rs'
    test
test app
    command 'true'
")]);
    let host = project.host();
    project.configure().failed()
        // other environments are not supported yet
        .stderr(&format!("error[test-target]: tests for target \"riscv64gc-unknown-none-elf\" cannot be run, only those for targets which the host \"{host}\" can run, or a target with `test`\n --> Jig:2:12"))
        // the test of a crate is forward declared with it
        .stderr("error[redeclare-custom]: redeclaration of test \"app\"\n --> Jig:6:6")
        .stderr("note: previously declared here\n --> Jig:4:7");
}

#[test]
fn commands() {
    let project = Project::new("commands", &[("Jig", "log note 'generated'\n")]);
    let run = |args: &[&str]| {
        let output = Command::new(configure_path()).args(args).current_dir(&project.dir).output().unwrap();
        (output.status.success(), String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap())
    };
    // the input of `generate` is `Jig` by default
    let (success, _, stderr) = run(&["generate"]);
    assert!(success, "{stderr}");
    assert_contains(&stderr, "note: generated");
    let (success, stdout, _) = run(&["help"]);
    assert!(success);
    assert_contains(&stdout, "usage: jig [+<toolchain>] <command>");
    // a mistaken command is shown with the usage
    for (args, error) in [
        (&[][..], "missing required argument `command`"),
        (&["Jig"][..], "unknown command \"Jig\""),
        (&["generate", "Jig", "build.ninja"][..], "unexpected argument \"build.ninja\""),
        (&["check"][..], "missing required argument `label`"),
    ] {
        let (success, _, stderr) = run(args);
        assert!(!success, "{args:?}");
        assert_contains(&stderr, error);
        assert_contains(&stderr, "usage: jig [+<toolchain>] <command>");
    }
}

#[test]
fn build_dir() {
    let project = Project::new("build-dir", &[("Jig", "\
set build-dir 'out'
crate kernel 'kernel.rs'
    test
custom image
    input kernel
    command 'touch' output
"), ("kernel.rs", "")]);
    let host = project.host();
    project.configure().ok();
    // everything which is built is in the build directory, with `build.ninja`, as ninja runs there
    assert!(!project.dir.join("build").exists());
    let ninja = project.ninja_in("out");
    assert_contains(&ninja, &format!("\nbuild {host}/libkernel.rlib: rustc kernel.rs\n"));
    assert_contains(&ninja, &format!("-L dependency={host}"));
    assert_contains(&ninja, &format!("\nbuild {host}/test/kernel: rustc kernel.rs\n"));
    assert_contains(&ninja, &format!("\nbuild {host}/check/libkernel.rmeta | {host}/check/kernel.json: rustc-check"));
    assert_contains(&ninja, &format!("\nbuild image: custom {host}/libkernel.rlib\n"));
    // with a directory for the temporary files of each edge, as they are named after the crate
    assert_contains(&ninja, "--out-dir=$tmpdir ");
    assert_contains(&ninja, &format!("\n    tmpdir = tmp/{host}/kernel\n"));
    assert_contains(&ninja, &format!("\n    tmpdir = tmp/{host}/test/kernel\n"));
    // while the database is at the project root, so that it is found without the configuration
    assert!(project.dir.join(".jig").exists());
    // `jig build` finds it from within the project, and runs ninja in the build directory, while the custom command
    // runs from the project root
    std::fs::create_dir_all(project.dir.join("sub")).unwrap();
    let build = Command::new(configure_path()).args(["build", "image"]).current_dir(project.dir.join("sub")).output().unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stdout));
    assert!(project.dir.join("out/image").exists());
    // and fails when ninja does
    let build = Command::new(configure_path()).args(["build", "nope"]).current_dir(&project.dir).output().unwrap();
    assert!(!build.status.success());
    assert_contains(&String::from_utf8(build.stderr).unwrap(), "the build failed");

    // the directory cannot change once it is used
    let project = Project::new("build-dir-used", &[("Jig", "crate kernel 'kernel.rs'\nset build-dir 'out'\n")]);
    project.configure().failed()
        .stderr("error[read-only-property]: cannot set read-only builtin property \"build-dir\"\n --> Jig:2:5")
        .stderr("note: build-dir: the directory of what is built, relative to the project root, which is read-only once it is used\n");
}

#[test]
fn regenerates_itself() {
    // `build.ninja` regenerates itself with the configure tool which made it, and again when the tool changes
    let project = Project::new("regenerate", &[("Jig", "crate app 'app.rs'\n"), ("app.rs", "")]);
    let output = project.configure().ok();
    // from the project root, with the root file as it was given
    assert_contains(&output.ninja, &format!("\
rule configure
    command = cd '{}' && '{}' generate 'Jig'
", project.dir.display(), configure_path().display()));
    assert_contains(&output.ninja, &format!("\nbuild build.ninja: configure Jig | {}\n", configure_path().display()));
    assert!(!output.ninja.contains("subninja"), "{}", output.ninja);
    // and ninja can use it on its own, in the build directory, regenerating it first as it has not recorded how it was
    // made
    let host = project.host();
    let ninja = Command::new("ninja").arg(format!("{host}/libapp.rlib")).current_dir(project.dir.join("build")).output().unwrap();
    let stdout = String::from_utf8_lossy(&ninja.stdout);
    assert!(ninja.status.success(), "{stdout}");
    assert_contains(&stdout, "CONFIGURE build.ninja");
    assert!(project.dir.join(format!("build/{host}/libapp.rlib")).exists());
}

#[test]
fn rebased_paths() {
    // ninja runs in the build directory, so the paths within it are relative to it, and the others are absolute
    let project = Project::new("rebased-paths", &[(
        "Jig",
        "crate app 'app.rs'\ncustom tool\n    input 'app.rs'\n    output 'jig'\n    command 'true'\n",
    ), ("app.rs", "")]);
    let dir = project.dir.display();
    let host = project.host();
    // a configure tool within the build directory is relative to it, so that it can be the output which builds it
    let tool = project.dir.join("build/jig");
    std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
    make_executable(&configure_path(), &tool);
    let output = Command::new(&tool).args(["generate", "Jig"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let ninja = std::fs::read_to_string(project.dir.join("build/build.ninja")).unwrap();
    assert_contains(&ninja, &format!("\nrule configure\n    command = cd '{dir}' && '{dir}/build/jig' generate 'Jig'\n"));
    assert_contains(&ninja, &format!("\nbuild build.ninja: configure {dir}/Jig | jig\n"));
    assert_contains(&ninja, &format!("\nbuild jig: custom {dir}/app.rs\n"));
    assert_contains(&ninja, &format!("\nbuild {host}/libapp.rlib: rustc {dir}/app.rs\n"));
    let depfile = std::fs::read_to_string(project.dir.join("build/build.ninja.d")).unwrap();
    assert_eq!(depfile, format!("build.ninja: {dir}/Jig reconfigure.stamp\n"));
}

/// Custom targets need a nightly toolchain, which the project uses but a temporary directory may not.
const NIGHTLY: &[(&str, &str)] = &[("RUSTUP_TOOLCHAIN", "nightly")];

#[test]
fn custom_target() {
    let project = Project::new("custom-target", &[("Jig", "\
crate kernel 'kernel.rs'
    target 'my-kernel'
target my-kernel
    inherit 'riscv64gc-unknown-none-elf'
    code-model 'large'
    no-redzone
    target-feature ['+m', '+a']
    os 'bluemetal'
")]);
    let output = project.configure_with(NIGHTLY).ok();
    let spec = std::fs::read_to_string(project.dir.join("build/targets/my-kernel.json")).unwrap();
    // inherited fields are kept, replaced in place, or added at the end
    assert_contains(&spec, "{\n  \"arch\": \"riscv64\",\n");
    assert_contains(&spec, "\n  \"code-model\": \"large\",\n");
    assert_contains(&spec, "\n  \"features\": \"+m,+a\",\n");
    assert_contains(&spec, "\n  \"metadata\": {\n");
    assert!(spec.ends_with(",\n  \"disable-redzone\": true,\n  \"os\": \"bluemetal\"\n}\n"), "{spec}");
    // crates for the target use its specification
    assert_contains(output.build("my-kernel/libkernel.rlib"), "\
build my-kernel/libkernel.rlib: rustc kernel.rs | targets/my-kernel.json
    crate-name = kernel
    crate-type = lib
    flags = --target=targets/my-kernel.json -Zunstable-options -L dependency=my-kernel");

    // an unchanged specification is not written again, as every crate for the target depends on it
    let modified = std::fs::metadata(project.dir.join("build/targets/my-kernel.json")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    project.configure_with(NIGHTLY).ok();
    let again = std::fs::metadata(project.dir.join("build/targets/my-kernel.json")).unwrap().modified().unwrap();
    assert_eq!(again, modified);
}

#[test]
fn custom_target_errors() {
    let project = Project::new("custom-target-errors", &[("Jig", "\
target bad
    inherit 'riscv64gc-unknown-none'
    max-atomic-width 32
    code-model 5
    no-redzone 'yes'
    target-feature [1]
target riscv64gc-unknown-none-elf
    os 'x'
target bad
crate k 'k.rs'
    target 'nonexistent'
crate j 'j.rs'
    target 'later'
target later
    inherit 'x86_64-unknown-none'
")]);
    let output = project.configure_with(NIGHTLY).failed();
    output
        .stderr("error[unknown-target]: unknown target \"riscv64gc-unknown-none\", did you mean \"riscv64gc-unknown-none-elf\"?\n --> Jig:2:13")
        // only some fields can be replaced
        .stderr("error[unknown-attribute]: unknown target attribute \"max-atomic-width\"\n --> Jig:3:5")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:4:16")
        .stderr("error[invalid-operand]: str value used where bool is required\n --> Jig:5:16")
        .stderr("error[invalid-operand]: list value used where str is required\n --> Jig:6:20")
        .stderr("error[builtin-target-name]: \"riscv64gc-unknown-none-elf\" is the name of a builtin target, which can be changed with `override`\n --> Jig:7:8")
        .stderr("error[redeclare-custom]: redeclaration of target \"bad\"\n --> Jig:9:8")
        .stderr("error[unknown-target]: unknown target \"nonexistent\"\n  --> Jig:11:12")
        .stderr("configuration failed with 8 errors");
    // a target can be declared after the crates which use it
    assert!(!output.stderr.contains("\"later\""), "{}", output.stderr);
}

#[test]
fn multi_line_strings() {
    let project = Project::new("multi-line-strings", &[("Jig", "\
declare base
    default '0x80000000'
log note '''
    ENTRY(_start)

    SECTIONS {{
        . = {base};
    }}
'''
log note '''
    first
# a comment, which is skipped
      indented {upper('x')}
    # not a comment
'''
log note '''
    a
''' == 'a'
")]);
    let output = project.configure_with(NIGHTLY).ok();
    // the indentation of the lines is removed, except for any beyond it, and the lines are templates
    output.stderr("note: first\n        indented X\n      # not a comment\n").stderr("note: true\n");
    // where braces are doubled
    output.stderr("note: ENTRY(_start)\n\n      SECTIONS {\n          . = 0x80000000;\n      }\n");
}

#[test]
fn multi_line_string_errors() {
    let project = Project::new("multi-line-string-errors", &[("Jig", "\
log note '''
    ok
   bad
      fine
  '''
log warning 'after'
log note '''
    never ended
")]);
    project.configure().failed()
        .stderr("error[string-indentation]: lines of a multi-line string must be indented by 4 spaces, or end it\n --> Jig:3:1")
        .stderr("error[string-indentation]: lines of a multi-line string must be indented by 4 spaces, or end it\n --> Jig:5:1")
        .stderr("error[unterminated-string]: multi-line string is not terminated\n --> Jig:1:10")
        // the statement after it is still read
        .stderr("warning: after\n --> Jig:6:13")
        .stderr("error[unterminated-string]: multi-line string is not terminated\n --> Jig:7:10")
        .stderr("configuration failed with 4 errors, 1 warning");
}

#[test]
fn crate_targets() {
    let project = Project::new("crate-targets", &[("Jig", "\
crate core 'core.rs'
    target ['riscv64gc-unknown-none-elf', host-target]
    builtin
crate util 'util.rs'
    target ['riscv64gc-unknown-none-elf', host-target]
crate app 'app.rs'
    target ['riscv64gc-unknown-none-elf']
    use util
")]);
    let host = project.host();
    let output = project.configure().ok();
    // a crate is declared for each target, with its dependencies for the same target
    let mut outputs = output.outputs();
    outputs.sort();
    let mut expected = vec![
        "riscv64gc-unknown-none-elf/libapp.rlib".to_string(),
        "riscv64gc-unknown-none-elf/libcore.rlib".into(),
        "riscv64gc-unknown-none-elf/libutil.rlib".into(),
        format!("{host}/libcore.rlib"),
        format!("{host}/libutil.rlib"),
    ];
    expected.sort();
    assert_eq!(outputs, expected);
    assert_contains(output.build(&format!("{host}/libutil.rlib")), &format!(
        "--extern core={host}/libcore.rlib",
    ));
}

#[test]
fn crate_target_errors() {
    let project = Project::new("crate-target-errors", &[("Jig", "\
crate a 'a.rs'
    target ['riscv64gc-unknown-none-elf', 'riscv64gc-unknown-none-elf']
crate b 'b.rs'
    target ['x86_64-unknown-none', 5]
crate c 'c.rs'
    target [host-target, 'riscv64gc-unknown-none-elf']
    test
")]);
    let host = project.host();
    let output = project.configure().failed();
    output
        .stderr("error[repeated-target]: target \"riscv64gc-unknown-none-elf\" is listed more than once\n --> Jig:2:12")
        .stderr("error[invalid-operand]: list value used where str is required\n --> Jig:4:12")
        // only the crate for the host target is tested
        .stderr(&format!("error[test-target]: tests for target \"riscv64gc-unknown-none-elf\" cannot be run, only those for targets which the host \"{host}\" can run, or a target with `test`\n --> Jig:6:12"))
        .stderr("configuration failed with 3 errors");
}

#[test]
fn target_test_runners() {
    let project = Project::new("target-runners", &[("Jig", "\
crate app 'app.rs'
    target ['vm', 'plain', host-target]
    test
target vm
    inherit 'riscv64gc-unknown-none-elf'
    test
    runner ['./run.sh', '-m', 'virt']
target plain
    inherit 'x86_64-unknown-none'
    test true
"), ("run.sh", "")]);
    let host = project.host();
    let output = project.configure_with(NIGHTLY).ok();
    // a test is named with its target unless it is the host target, and needs its runner
    assert_contains(&output.ninja, "\nbuild test/app@vm: phony vm/test/app run.sh\n");
    assert_contains(&output.ninja, "\nbuild test/app@plain: phony plain/test/app\n");
    assert_contains(&output.ninja, &format!("\nbuild test/app: phony {host}/test/app\n"));
    // the runner runs the harness, which is its last argument, while without one the harness runs directly
    let database = std::fs::read(project.dir.join(".jig")).unwrap();
    let strings = |strings: &[&str]| {
        let mut bytes = (strings.len() as u32).to_le_bytes().to_vec();
        for s in strings {
            bytes.extend((s.len() as u32).to_le_bytes());
            bytes.extend(s.as_bytes());
        }
        bytes
    };
    let contains = |needle: &[u8]| database.windows(needle.len()).any(|window| window == needle);
    assert!(contains(&strings(&["./run.sh", "-m", "virt", "./build/vm/test/app"])));
    assert!(contains(&strings(&["./build/plain/test/app"])));
}

#[test]
fn target_test_runner_errors() {
    let project = Project::new("target-runner-errors", &[("Jig", "\
crate lib 'lib.rs'
    target 'no-tests'
    test
target no-tests
    inherit 'x86_64-unknown-none'
    runner 'ignored'
target bad
    inherit 'x86_64-unknown-none'
    test 'yes'
    runner []
")]);
    let host = project.host();
    project.configure_with(NIGHTLY).failed()
        // a runner does not allow tests on its own
        .stderr(&format!("error[test-target]: tests for target \"no-tests\" cannot be run, only those for targets which the host \"{host}\" can run, or a target with `test`\n --> Jig:2:12"))
        .stderr("error[invalid-operand]: str value used where bool is required\n --> Jig:9:10")
        .stderr("error[invalid-operand]: list value used where str or a list of str is required\n  --> Jig:10:12")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn target_override() {
    let project = Project::new("target-override", &[("Jig", "\
crate app 'app.rs'
    target ['riscv64gc-unknown-none-elf', host-target]
    test
target riscv64gc-unknown-none-elf
    override
    test
    runner ['qemu-system-riscv64', '-kernel']
")]);
    let host = project.host();
    let output = project.configure_with(NIGHTLY).ok();
    // a builtin target keeps its specification, but can be tested
    assert_contains(output.build("riscv64gc-unknown-none-elf/test/app"), "\
    flags = --target=riscv64gc-unknown-none-elf -L dependency=riscv64gc-unknown-none-elf --test");
    assert_contains(&output.ninja, "\nbuild test/app@riscv64gc-unknown-none-elf: phony riscv64gc-unknown-none-elf/test/app\n");
    assert_contains(&output.ninja, &format!("\nbuild test/app: phony {host}/test/app\n"));
    assert!(!project.dir.join("build/targets").exists());
}

#[test]
fn link_scripts() {
    let project = Project::new("link-scripts", &[("Jig", "\
custom generated
    output 'generated.x'
    command 'gen' output
crate kernel 'kernel.rs'
    target 'my-kernel'
    crate-type 'bin'
crate util 'util.rs'
    target 'my-kernel'
    test
crate other 'other.rs'
    target 'my-kernel'
    crate-type 'bin'
    link-script 'other.x'
crate app 'app.rs'
    target 'riscv64gc-unknown-none-elf'
    crate-type 'bin'
target my-kernel
    inherit 'riscv64gc-unknown-none-elf'
    test
    link-script 'kernel/link.x'
target riscv64gc-unknown-none-elf
    override
    link-script generated
log note '{my-kernel.link-script} {riscv64gc-unknown-none-elf.link-script} {my-kernel/other.link-script}'
")]);
    let output = project.configure_with(NIGHTLY).ok();
    // a path is relative to the file, and a reference is to what is built
    output.stderr("note: kernel/link.x build/generated.x other.x\n");
    // the script is passed to the linker, rather than written to the specification, and what is linked depends on it
    assert_contains(output.build("my-kernel/kernel"), "\
build my-kernel/kernel: rustc kernel.rs | targets/my-kernel.json kernel/link.x
    crate-name = kernel
    crate-type = bin
    flags = --target=targets/my-kernel.json -Zunstable-options -L dependency=my-kernel -C link-arg=-Tkernel/link.x\n");
    let spec = std::fs::read_to_string(project.dir.join("build/targets/my-kernel.json")).unwrap();
    assert!(!spec.contains("link.x"), "{spec}");
    // a library is not linked, but its test harness is
    assert_contains(output.build("my-kernel/libutil.rlib"), "\
build my-kernel/libutil.rlib: rustc util.rs | targets/my-kernel.json
    crate-name = util
    crate-type = lib
    flags = --target=targets/my-kernel.json -Zunstable-options -L dependency=my-kernel\n");
    assert_contains(output.build("my-kernel/test/util"), "\
build my-kernel/test/util: rustc util.rs | targets/my-kernel.json kernel/link.x
    crate-name = util
    crate-type = lib
    flags = --target=targets/my-kernel.json -Zunstable-options -L dependency=my-kernel -C link-arg=-Tkernel/link.x --test\n");
    // nor is a check
    assert_eq!(output.ninja.matches("-C link-arg=-Tkernel/link.x").count(), 2, "{}", output.ninja);
    // a crate's own replaces its target's
    assert_contains(output.build("my-kernel/other"), "\
build my-kernel/other: rustc other.rs | targets/my-kernel.json other.x
    crate-name = other
    crate-type = bin
    flags = --target=targets/my-kernel.json -Zunstable-options -L dependency=my-kernel -C link-arg=-Tother.x\n");
    // a builtin target can have one with `override`, which is built first when it is generated
    assert_contains(output.build("riscv64gc-unknown-none-elf/app"), "\
build riscv64gc-unknown-none-elf/app: rustc app.rs | generated.x
    crate-name = app
    crate-type = bin
    flags = --target=riscv64gc-unknown-none-elf -L dependency=riscv64gc-unknown-none-elf -C link-arg=-Tgenerated.x\n");
}

#[test]
fn link_script_errors() {
    let project = Project::new("link-script-errors", &[("Jig", "\
custom two
    output 'a.x' 'b.x'
    command 'gen'
target my-kernel
    inherit 'riscv64gc-unknown-none-elf'
    link-script ['a.x']
crate kernel 'kernel.rs'
    link-script two
    link-script 1
log note my-kernel.link-script
")]);
    project.configure_with(NIGHTLY).failed()
        .stderr("error[invalid-operand]: list value used where str, or a reference to one file is required\n --> Jig:6:17")
        .stderr("error[invalid-operand]: ref value used where str, or a reference to one file is required\n --> Jig:8:17")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n --> Jig:9:17")
        .stderr("error[attribute-not-set]: attribute \"link-script\" of target \"my-kernel\" is not set");
}

#[test]
fn target_override_errors() {
    let project = Project::new("target-override-errors", &[("Jig", "\
target riscv64gc-unknown-none-elf
    override
    inherit 'x86_64-unknown-none'
    os 'x'
    test
target riscv64gc-unknown-none
    override
target x86_64-unknown-none
    os 'x'
target riscv64gc-unknown-none-elf
    override
")]);
    project.configure_with(NIGHTLY).failed()
        // it cannot be used with the attributes which describe a specification
        .stderr("error[override-attribute]: attribute \"inherit\" cannot be used with `override`\n --> Jig:3:5")
        .stderr("error[override-attribute]: attribute \"os\" cannot be used with `override`\n --> Jig:4:5")
        .stderr("error[unknown-target]: unknown target \"riscv64gc-unknown-none\", did you mean \"riscv64gc-unknown-none-elf\"?\n --> Jig:6:8")
        .stderr("error[builtin-target-name]: \"x86_64-unknown-none\" is the name of a builtin target, which can be changed with `override`\n --> Jig:8:8")
        .stderr("error[redeclare-custom]: redeclaration of target \"riscv64gc-unknown-none-elf\"\n  --> Jig:10:8")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn attribute_comments() {
    let project = Project::new("attribute-comments", &[("Jig", "\
declare size
    # type 'str'
    type 'unsigned'
    #default 'x'
    default 1
# a comment
crate app 'app.rs'
    # use missing
    default
    #
log note 'size is {size}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    // an indented comment comments out an attribute, without ending the statement
    output.stderr("note: size is 1\n");
    assert_contains(&output.ninja, &format!("\ndefault {host}/libapp.rlib\n"));

    // a comment must be at the indentation of an attribute
    let project = Project::new("attribute-comment-errors", &[("Jig", "crate bad 'bad.rs'\n        # too deep\n")]);
    project.configure().failed()
        .stderr("error[expected-identifier]: expected identifier\n --> Jig:2:5")
        .stderr("configuration failed with 1 error");
}

#[test]
fn meta_for() {
    let project = Project::new("meta-for", &[("Jig", "\
declare targets
    default ['riscv64gc-unknown-none-elf', 'x86_64-unknown-none']
@for t in targets
crate k 'k.rs'
    target t
@for n in 1 2 [3, 4]
@if n % 2 == 0
log note 'even {n}'
@for a in 'x' 'y'
@for b in [1, 2]
log note '{a}{b}'
@for e in []
log note 'never'
@if false
@for f in [missing]
log note 'skipped'
log note 'after'
")]);
    let output = project.configure().ok();
    // the statement is processed for each value, with the meta-statements after the `@for`
    let mut outputs = output.outputs();
    outputs.sort();
    assert_eq!(outputs, ["riscv64gc-unknown-none-elf/libk.rlib", "x86_64-unknown-none/libk.rlib"]);
    output.stderr("note: even 2\n").stderr("note: even 4\n");
    assert!(!output.stderr.contains("even 1") && !output.stderr.contains("even 3"), "{}", output.stderr);
    // loops can be nested
    output.stderr("note: x1\n").stderr("note: x2\n").stderr("note: y1\n").stderr("note: y2\n");
    // without values, or in a skipped statement, nothing is processed or evaluated
    assert!(!output.stderr.contains("never") && !output.stderr.contains("skipped"), "{}", output.stderr);
    output.stderr("note: after\n");
}

#[test]
fn meta_for_errors() {
    let project = Project::new("meta-for-errors", &[("Jig", "\
@for x 1
log note 'a'
@for x in
log note 'b'
@for x in 1

log note 'c'
log note '{x}'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[expected-exact]: expected \"in\"\n --> Jig:1:8")
        .stderr("error[expected-exact]: expected \" \"\n --> Jig:3:10")
        .stderr("error[expected-statement-after-meta]: expected statement on the line after a meta-statement\n --> Jig:5:1")
        // the property is only set for the statement
        .stderr("error[unset-property]: no value for property \"x\"\n --> Jig:8:12")
        .stderr("configuration failed with 4 errors");
    assert!(!output.stderr.contains("note: a\n") && !output.stderr.contains("note: b\n"), "{}", output.stderr);
}

#[test]
fn meta_then() {
    let project = Project::new("meta-then", &[("Jig", "\
declare x
    default 0
@for a in 1 2
set x (x + a)
@then
log note 'inc {a}, total: {x}'
@if x > 5
log note 'big'
@then
log note 'also big'
@else
log note 'small'
@then
@if x == 3
log note 'three'
@then
log note 'still three'
@for a in []
log note 'never'
@then
log note 'never either'
log note 'end'
")]);
    let output = project.configure().ok();
    // a chained statement is repeated by `@for` with the first
    output.stderr("note: inc 1, total: 1\n").stderr("note: inc 2, total: 3\n");
    // and is skipped with it, while `@else` applies to the whole chain
    assert!(!output.stderr.contains("big"), "{}", output.stderr);
    output.stderr("note: small\n");
    // a chained statement can have its own meta-statements, which apply to the rest of the chain
    output.stderr("note: three\n").stderr("note: still three\n");
    assert!(!output.stderr.contains("never"), "{}", output.stderr);
    output.stderr("note: end\n");
}

#[test]
fn meta_then_errors() {
    let project = Project::new("meta-then-errors", &[("Jig", "\
@then
log note 'a'
log note 'b'

@then
log note 'c'
@then

log note 'd'
@then x
log note 'e'
")]);
    let output = project.configure().failed();
    output
        .stderr("error[then-without-statement]: \"@then\" must be directly after a statement\n --> Jig:1:1")
        .stderr("note: b\n")
        .stderr("error[then-without-statement]: \"@then\" must be directly after a statement\n --> Jig:5:1")
        .stderr("error[expected-statement-after-meta]: expected statement on the line after a meta-statement\n --> Jig:7:1")
        .stderr("error[expected-exact]: expected \"\\n\"\n  --> Jig:10:6")
        .stderr("note: e\n")
        .stderr("configuration failed with 4 errors");
    assert!(!output.stderr.contains("note: a\n") && !output.stderr.contains("note: c\n"), "{}", output.stderr);
}

#[test]
fn crate_types() {
    let project = Project::new("crate-types", &[
        ("Jig", "\
crate answer 'answer.rs'
    crate-type 'proc-macro'
crate app 'app.rs'
    crate-type 'bin'
    use answer
    use util
crate util 'util.rs'
    use answer
set target 'riscv64gc-unknown-none-elf'
crate kernel 'kernel.rs'
    use answer
"),
        ("answer.rs", "\
use proc_macro::TokenStream;
#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream { \"42\".parse().unwrap() }
"),
        ("app.rs", "fn main() { println!(\"{} {}\", answer::answer!(), util::twice()); }\n"),
        ("util.rs", "pub fn twice() -> u32 { answer::answer!() * 2 }\n"),
        ("kernel.rs", "#![no_std]\npub fn k() -> u32 { answer::answer!() }\n"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    let answer = format!("{host}/{}answer{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    // a procedural macro is for the host target, even when another is the default, and is used by any target
    assert_contains(output.build(&answer), &format!("\
    crate-type = proc-macro
    flags = --target={host} -L dependency={host} --extern proc_macro"));
    assert_contains(output.build("riscv64gc-unknown-none-elf/libkernel.rlib"), &format!(
        "--extern answer={answer}",
    ));
    assert_contains(output.build(&format!("{host}/app")), "crate-type = bin");
    // the binary is built and runs, using the procedural macro directly and through a library
    let ninja = Command::new("ninja").arg(format!("{host}/app")).current_dir(project.dir.join("build")).output().unwrap();
    assert!(ninja.status.success(), "{}", String::from_utf8_lossy(&ninja.stdout));
    let app = Command::new(project.dir.join(format!("build/{host}/app"))).output().unwrap();
    assert_eq!(String::from_utf8(app.stdout).unwrap(), "42 84\n");
    // checking uses the procedural macro itself, as rustc runs it
    let (stdout, _) = project.check(&format!("app@{host}"));
    assert_eq!(stdout, "");
    let json = std::fs::read_to_string(project.dir.join(".rust-project.json")).unwrap();
    assert_contains(&json, &format!(
        "\"is_proc_macro\": true,\n            \"proc_macro_dylib_path\": \"{}/build/{answer}\",",
        std::path::absolute(&project.dir).unwrap().display(),
    ));
}

#[test]
fn crate_type_errors() {
    let project = Project::new("crate-type-errors", &[("Jig", "\
crate a 'a.rs'
    crate-type 'dylib'
crate b 'b.rs'
    crate-type 'proc-macro'
    target 'riscv64gc-unknown-none-elf'
crate tool 'tool.rs'
    crate-type 'bin'
crate c 'c.rs'
    use tool
crate d 'd.rs'
    crate-type 5
")]);
    let host = project.host();
    project.configure().failed()
        .stderr("error[invalid-crate-type]: invalid crate type \"dylib\", which must be \"lib\", \"bin\" or \"proc-macro\"\n --> Jig:2:16")
        .stderr(&format!("error[proc-macro-target]: procedural macros cannot be compiled for target \"riscv64gc-unknown-none-elf\", only the host target \"{host}\"\n --> Jig:5:12"))
        .stderr("error[use-binary]: crate \"tool\" is a binary, which cannot be used by another crate\n --> Jig:9:9")
        .stderr("note: declared here\n --> Jig:6:7\n  |\n6 | crate tool 'tool.rs'\n  |       ^^^^\n")
        .stderr("error[invalid-operand]: unsigned value used where str is required\n  --> Jig:11:16")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn join_and_split() {
    let project = Project::new("join-split", &[("Jig", "\
declare features
    default ['+m', '+a', 1, true]
log note join(features, ',')
log note join([], ',') == ''
log note join([['a', 'b'], 'c'], '/')
log note len(split('a,,b', ','))
log note split('a::b', '::') == ['a', 'b']
log note split('', ',') == ['']
log note join(split('x-y-z', '-'), '_')
declare a
set a split('a', '')
set a join('a', ',')
set a split(['a'], ',')
")]);
    project.configure().failed()
        // items are converted as in a string template
        .stderr("note: +m,+a,1,true\n")
        .stderr("note: true\n --> Jig:4:10")
        .stderr("note: a b/c\n")
        // empty items are kept
        .stderr("note: 3\n")
        .stderr("note: true\n --> Jig:7:10")
        .stderr("note: true\n --> Jig:8:10")
        .stderr("note: x_y_z\n")
        .stderr("error[empty-separator]: the separator cannot be empty\n  --> Jig:11:18")
        .stderr("error[invalid-operand]: str value used where list is required\n  --> Jig:12:12")
        .stderr("error[invalid-operand]: list value used where str is required\n  --> Jig:13:13")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn get_function() {
    let project = Project::new("get", &[("Jig", "\
declare items
    default ['a', 'b', ['c']]
log note '{get(items, 0)} {get(items, 1)} {get(items, {'1'})}'
log note get(items, 2) == ['c']
log note '{get(items, 5, 'none')} {get(items, -1, 'negative')} {get([], 0, 7)}'
declare a
set a get(items, 3)
set a get(items, -1)
set a get(items)
set a get(items, 'x')
set a get('ab', 0)
set a get(items, 0, 1, 2)
")]);
    project.configure().failed()
        .stderr("note: a b b\n")
        .stderr("note: true\n")
        // the default is used for an index out of range
        .stderr("note: none negative 7\n")
        .stderr("error[index-out-of-range]: index out of range for a list of length 3\n --> Jig:7:18")
        .stderr("error[index-out-of-range]: index out of range for a list of length 3\n --> Jig:8:18")
        .stderr("error[argument-count]: function \"get\" takes 2 or 3 arguments, but 1 was given\n --> Jig:9:7")
        .stderr("error[invalid-operand]: str value used where integer is required\n  --> Jig:10:18")
        .stderr("error[invalid-operand]: str value used where list or dict is required\n  --> Jig:11:11")
        .stderr("error[argument-count]: function \"get\" takes 2 or 3 arguments, but 4 were given\n  --> Jig:12:7")
        .stderr("configuration failed with 6 errors");
}

#[test]
fn bitwise_operators() {
    let project = Project::new("bitwise", &[("Jig", "\
declare flags
    default 0x0f
log note '{flags & 0x3} {flags | 0x30} {flags ^ 0xff} {1 << 4} {0x100 >> 4} {!0} {!flags & 0xf0}'
log note '{1 + 2 << 1} {1 | 2 & 3} {6 & 3 == 2} {-8 >> 1} {!-1} {-1 & 1}'
log note '{1 << 63 >> 63} {0x8000000000000000 << 1} {!{0}} {!true}'
log note '{1 << 64}'
log note '{1 << -1}'
log note '{true & 1}'
")]);
    project.configure().failed()
        // `!` is the complement of an integer
        .stderr("note: 3 63 240 16 16 18446744073709551615 240\n")
        // the precedence is the same as in Rust, and `>>` keeps the sign
        .stderr("note: 6 3 true -4 0 1\n")
        // bits shifted out are lost, while `{}` is taken as a boolean by `!`
        .stderr("note: 1 0 true false\n")
        .stderr("error[shift-amount]: shift amount must be from 0 to 63\n --> Jig:6:17")
        .stderr("error[shift-amount]: shift amount must be from 0 to 63\n --> Jig:7:17")
        .stderr("error[invalid-operand]: bool value used where integer is required\n --> Jig:8:12")
        .stderr("configuration failed with 3 errors");
}

#[test]
fn min_max() {
    let project = Project::new("min-max", &[("Jig", "\
declare sizes
    default [4, 16, 2]
declare offset
    type 'signed'
    default -5
declare unsigned
    type 'unsigned'
    default 1
log note '{min(sizes)} {max(sizes)} {min(3)} {max(1, 7, 3)} {min(sizes, 1)}'
log note '{max([3, -1])} {min(offset, 2)} {max(offset, [0])} {max([1], [9, 2], 3)} {min({'4'}, 6)}'
log note '{min()}'
log note '{min([])}'
log note '{min('a')}'
log note '{min(offset, unsigned)}'
")]);
    project.configure().failed()
        // each item of a list is a value
        .stderr("note: 2 16 3 7 1\n")
        // literals, including in lists, can be signed or unsigned
        .stderr("note: 3 -5 0 9 4\n")
        .stderr("error[argument-count]: function \"min\" takes at least 1 argument, but 0 were given\n  --> Jig:11:12")
        .stderr("error[no-values]: there are no values to compare\n  --> Jig:12:12")
        .stderr("error[invalid-operand]: str value used where integer is required\n  --> Jig:13:16")
        .stderr("error[mismatched-operands]: mismatched operand types signed and unsigned\n  --> Jig:14:12")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn output_properties() {
    let project = Project::new("output-properties", &[("Jig", "\
crate core 'core.rs'
    target ['riscv64gc-unknown-none-elf', host-target]
crate kernel 'kernel.rs'
    target 'riscv64gc-unknown-none-elf'
custom image
    input riscv64gc-unknown-none-elf/kernel
    output 'kernel.bin'
    command 'objcopy' '-O' 'binary' input output
custom pair
    output ['a.txt', 'b.txt']
    command 'touch' output
log note riscv64gc-unknown-none-elf/core
@for t in ['riscv64gc-unknown-none-elf', host-target]
log note {t}/core
log note '{image} {pair} {8 / 2}'
@for t in ['riscv64gc-unknown-none-elf']
log note {t}/kernel
declare reference
    type 'ref'
    default core
log note '{reference} {core.target == target} {reference.output}'
set target 'riscv64gc-unknown-none-elf'
log note core.target == target
")]);
    let host = project.host();
    let output = project.configure().ok();
    // a crate is referred to with its target, as the path of its output
    output.stderr("note: build/riscv64gc-unknown-none-elf/libcore.rlib\n");
    output.stderr(&format!("note: build/{host}/libcore.rlib\n"));
    // and a custom command by its name, as its outputs
    output.stderr("note: build/kernel.bin build/a.txt build/b.txt 4\n");
    // the target can be computed, as any part of a name can
    output.stderr("note: build/riscv64gc-unknown-none-elf/libkernel.rlib\n  --> Jig:17:10");
    // a crate is also referred to by its name alone, for the target in effect where it is referred to
    output.stderr(&format!("note: build/{host}/libcore.rlib true build/{host}/libcore.rlib\n"));
    output.stderr("note: true\n  --> Jig:23:10");
    assert_contains(output.build("kernel.bin"), "build kernel.bin: custom riscv64gc-unknown-none-elf/libkernel.rlib");
}

#[test]
fn output_property_errors() {
    let project = Project::new("output-property-errors", &[("Jig", "\
crate core 'core.rs'
    target 'riscv64gc-unknown-none-elf'
custom image
    output 'kernel.bin'
    command 'true'
log note x86_64-unknown-none/core
log note riscv64gc-unknown-none-elf/nothing
declare image
set image 'x'
log note target/core
log note target/core.target
declare {host-target}/util
    type 'str'
crate util 'util.rs'
")]);
    let host = project.host();
    project.configure().failed()
        .stderr("error[unknown-crate]: no crate \"core\" for target \"x86_64-unknown-none\"\n --> Jig:6:10")
        .stderr("error[unknown-crate]: no crate \"nothing\" for target \"riscv64gc-unknown-none-elf\"\n --> Jig:7:10")
        // a target is not taken from a property, which it would be ambiguous with
        .stderr("error[invalid-target]: `target` is not a valid target\n  --> Jig:10:10")
        .stderr("error[invalid-target]: `target` is not a valid target\n  --> Jig:11:10")
        // the property of a crate is declared by the crate
        .stderr(&format!("error[invalid-identifier]: invalid character '/' in identifier \"{host}/util\"\n  --> Jig:12:9\n   |\n12 | declare {{host-target}}/util\n   |         ^^^^^^^^^^^^^^^^^^\n"))
        // the properties of outputs are declared and read-only
        .stderr("error[redeclare-property]: redeclaration of property \"image\"\n --> Jig:8:9")
        .stderr("note: image: the output of the custom command\n")
        .stderr("error[read-only-property]: cannot set read-only property \"image\"\n --> Jig:9:5")
        .stderr("configuration failed with 7 errors");
}

#[test]
fn computed_name_parts() {
    let project = Project::new("computed-name-parts", &[("Jig", "\
declare b
    default 'a'
declare a-a
    default 'apples'
log note a-{b}
set a-{b} 'pears'
log note '{a-{b}}'
declare x-{b}{b}
    default 1
log note x-aa
declare x/{b}/9
log note a/{b}x
declare plain/x
declare {b}/x
")]);
    project.configure().failed()
        // any part of a name may be computed, including in a string template
        .stderr("note: apples\n")
        .stderr("note: pears\n")
        .stderr("note: 1\n")
        .stderr("error[invalid-identifier]: invalid character '9' in identifier \"x/a/9\"\n  --> Jig:11:9")
        .stderr("error[invalid-target]: `a` is not a valid target\n  --> Jig:12:10")
        // only crates and codegen options have properties with a `/`
        .stderr("error[invalid-identifier]: invalid character '/' in identifier \"plain/x\"\n  --> Jig:13:14")
        .stderr("error[invalid-identifier]: invalid character '/' in identifier \"a/x\"\n  --> Jig:14:9")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn custom_outputs() {
    let project = Project::new("custom-outputs", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "\
crate tool 'tool.rs'
    crate-type 'bin'
custom apples
    command 'touch' output
custom pears
    output output 'data'
    command 'ls'
log note pears.output
custom a
    input {host-target}/tool
    output 'example'
    command 'cat' input output
custom b
    command tool output
custom c
    input pears 'local.txt'
    output '/tmp/absolute'
    command 'cat' input
"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    // an output is named after its command, in the build directory, unless it is replaced
    assert_contains(output.build("apples"), "build apples: custom\n    command-line = 'touch' 'build/apples'");
    output.stderr("note: build/pears build/data\n");
    // the paths of a crate or custom command are not resolved again, relative to the file which refers to them
    assert_contains(output.build("example"), &format!(
        "build example: custom {host}/tool\n    command-line = 'cat' 'build/{host}/tool' 'build/example'",
    ));
    assert_contains(output.build("b"), &format!(
        "build b: custom | {host}/tool\n    command-line = './build/{host}/tool' 'build/b'",
    ));
    assert_contains(output.build("/tmp/absolute"), "build /tmp/absolute: custom pears data sub/local.txt");
}

#[test]
fn run_reference() {
    let project = Project::new("run-reference", &[
        ("Jig", "use 'sub'\n"),
        ("sub/Jig", "set diagnostic/impure 'allow'\ncustom script\n    command 'true'\nlog note run(script, 'a')\n"),
        ("build/script", "#!/bin/sh\necho ran \"$0\" \"$1\"\n"),
    ]);
    make_executable(&project.dir.join("build/script"), &project.dir.join("build/script"));
    // the program is the path of the output, rather than resolved again relative to the file
    project.configure().ok().stderr("note: ran ./build/script a\n");
}

#[test]
fn overlapping_outputs() {
    let project = Project::new("overlapping-outputs", &[("Jig", "\
crate util 'util.rs'
    test
custom clash
    output '{host-target}/libutil.rlib'
    command 'true'
custom harness
    output '{host-target}/test'
    command 'true'
custom dir
    command 'true'
custom inside
    output 'dir/x'
    command 'true'
custom twice
    output 'dir'
    command 'true'
")]);
    let host = project.host();
    project.configure().failed()
        .stderr(&format!(
            "error[overlapping-output]: output \"build/{host}/libutil.rlib\" of the custom command is also the output of the crate \"{host}/util\"\n --> Jig:3:8",
        ))
        .stderr("note: declared here\n --> Jig:1:7")
        .stderr(&format!(
            "error[overlapping-output]: output \"build/{host}/test\" of the custom command overlaps \"build/{host}/test/util\", the output of the crate",
        ))
        .stderr("error[overlapping-output]: output \"build/dir/x\" of the custom command overlaps \"build/dir\", the output of the custom command \"dir\"")
        .stderr("error[overlapping-output]: output \"build/dir\" of the custom command is also the output of the custom command \"dir\"\n  --> Jig:14:8")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn namespaces() {
    let project = Project::new("namespaces", &[("Jig", "\
log note len(codegen/)
set codegen/opt-level 3
set codegen/target-feature ['+a', '-b']
log note codegen/
log note get(codegen/, 'opt-level')
@with codegen/opt-level as 's'
log note codegen/
log note codegen/ == [opt-level: 3, target-feature: ['+a', '-b']]
crate core 'core.rs'
crate util 'util.rs'
    target 'riscv64gc-unknown-none-elf'
log note {host-target}/
log note keys(riscv64gc-unknown-none-elf/)
log note len(aarch64-unknown-none/)
"), ("core.rs", ""), ("util.rs", "")]);
    let host = project.host();
    project.configure().ok()
        // only the properties which are set, by the rest of their names
        .stderr("note: 0\n")
        .stderr("note: opt-level: 3, target-feature: +a -b\n --> Jig:4:10")
        .stderr("note: 3\n")
        // with their values where it is evaluated
        .stderr("note: opt-level: s, target-feature: +a -b\n --> Jig:7:10")
        .stderr("note: true\n")
        // the crates of a target, whose name can be computed
        .stderr(&format!("note: core: build/{host}/libcore.rlib\n"))
        .stderr("note: util\n")
        .stderr("note: 0\n  --> Jig:14:10");

    // a namespace must have properties declared in it, or be a target
    let project = Project::new("namespace-errors", &[("Jig", "log note nope/\nlog note {'nope'}/.x\n")]);
    project.configure().failed()
        .stderr("error[unknown-namespace]: no property or target has the namespace `nope/`\n --> Jig:1:10")
        .stderr("error[unknown-namespace]: no property or target has the namespace `nope/`\n --> Jig:2:10");
}

#[test]
fn codegen_options() {
    let project = Project::new("codegen-options", &[("Jig", "\
set codegen/opt-level 's'
set codegen/debug-assertions true
set codegen/llvm-args ['-a', '-b c']
set codegen/link-arg ['-Tlink.x', '-static']
crate kernel 'kernel.rs'
    crate-type 'bin'
set codegen/opt-level 3
crate other 'other.rs'
crate fast 'fast.rs'
    codegen/opt-level 2
    codegen/target-feature ['+a', '-b']
    codegen/opt-level 1
log note '{codegen/opt-level} {codegen/opt-level.info}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    // the options are those set where the crate is declared, and are also used to check it
    let kernel = format!("\
    flags = --target={host} -L dependency={host} -C debug-assertions=true -C link-arg=-Tlink.x -C link-arg=-static \
-C 'llvm-args=-a -b c' -C opt-level=s");
    assert_contains(output.build(&format!("{host}/kernel")), &kernel);
    assert_eq!(output.ninja.matches("-C 'llvm-args=-a -b c' -C opt-level=s").count(), 2, "{}", output.ninja);
    assert_contains(output.build(&format!("{host}/libother.rlib")), "-C opt-level=3");
    // an attribute replaces the property for the crate, where the last is used
    assert_contains(output.build(&format!("{host}/libfast.rlib")), "-C 'llvm-args=-a -b c' -C opt-level=1 -C target-feature=+a,-b");
    output.stderr("note: 3 the value of `-C opt-level` for the crates declared while it is set\n");
}

#[test]
fn profiles() {
    let project = Project::new("profiles", &[("Jig", "\
use profile/release
crate fast 'fast.rs'
use profile/debug
crate slow 'slow.rs'
set codegen/opt-level 1
@for profile in ['release']
use profile/{profile}
crate other 'other.rs'
log note codegen/
")]);
    let host = project.host();
    let output = project.configure().ok();
    assert_contains(
        output.build(&format!("{host}/libfast.rlib")),
        "-C debug-assertions=false -C debuginfo=0 -C opt-level=3 -C overflow-checks=false",
    );
    assert_contains(
        output.build(&format!("{host}/libslow.rlib")),
        "-C debug-assertions=true -C debuginfo=2 -C opt-level=0 -C overflow-checks=true",
    );
    // a profile which has already been used is applied again, replacing what was set since
    assert_contains(
        output.build(&format!("{host}/libother.rlib")),
        "-C debug-assertions=false -C debuginfo=0 -C opt-level=3 -C overflow-checks=false",
    );
    output.stderr("note: debug-assertions: false, debuginfo: 0, opt-level: 3, overflow-checks: false");
    // the profiles are part of jig, so the configuration does not depend on them as files
    let depfile = project.depfile();
    assert!(depfile.starts_with("build.ninja: Jig reconfigure.stamp"), "{depfile}");
}

#[test]
fn builtin_errors() {
    let project = Project::new("builtin-errors", &[("Jig", "\
use profile/relase
use profile/fast
use other/release
use profile/debug
    file
")]);
    project.configure().failed()
        .stderr("error[unknown-builtin]: unknown builtin file \"profile/relase\", did you mean \"profile/release\"?\n --> Jig:1:5")
        .stderr("error[unknown-builtin]: unknown builtin file \"profile/fast\"\n --> Jig:2:5")
        .stderr("error[unknown-builtin]: unknown builtin file \"other/release\"\n --> Jig:3:5")
        .stderr("error[unknown-attribute]: unknown use attribute \"file\"\n --> Jig:5:5")
        .stderr("configuration failed with 4 errors");
}

#[test]
fn msrv() {
    let project = Project::new("msrv", &[("Jig", "\
set rust-version '1.0'
use msrv
log note 'supported'
    quiet
")]);
    project.configure().ok().stderr("note: supported\n");

    // a later version is fatal, so nothing after it is processed
    let project = Project::new("msrv-unsupported", &[("Jig", "\
set rust-version '1000.0.0'
use msrv
log note 'unreachable'
")]);
    let output = project.configure().failed();
    let version = output.stderr.split("Rust version ").nth(1).unwrap().split(':').next().unwrap().to_string();
    output
        .stderr(&format!("fatal[msrv]: unsupported Rust version {version}: minimum required is 1000.0.0\n"))
        .stderr("configuration failed with 1 error");
    assert!(!output.stderr.contains("unreachable"), "{}", output.stderr);

    let project = Project::new("msrv-unset", &[("Jig", "use msrv\n")]);
    project.configure().failed()
        .stderr("error[msrv]: `use msrv` needs `rust-version` to be set before it\n")
        .stderr("configuration failed with 1 error");
}

#[test]
fn codegen_option_errors() {
    let project = Project::new("codegen-option-errors", &[("Jig", "\
set codegen/opt-level ['3']
set codegen/target-feature [a: 1]
crate kernel 'kernel.rs'
set codegen/opt-levl 3
log note codegen/debuginfo
crate other 'other.rs'
    codegen/opt-levl 1
    codegen/debuginfo [1]
")]);
    project.configure().failed()
        .stderr("error[invalid-operand]: list value used where str is required\n --> Jig:3:7")
        .stderr("note: list value set here\n --> Jig:1:23")
        .stderr("error[invalid-operand]: dict value used where str or list is required\n --> Jig:3:7")
        .stderr("note: dict value set here\n --> Jig:2:28")
        .stderr("error[unset-property]: no value for property \"codegen/debuginfo\"\n --> Jig:5:10")
        .stderr("error[unknown-crate-attribute]: unknown crate attribute \"codegen/opt-levl\"\n --> Jig:7:5")
        .stderr("error[invalid-operand]: list value used where str is required\n --> Jig:8:23")
        .stderr("error[undeclared-property]: property \"codegen/opt-levl\" is undeclared\n --> Jig:4:5")
        // the properties are in effect for the second crate too
        .stderr("configuration failed with 8 errors");
}

#[test]
fn diagnostic_overrides() {
    let project = Project::new("diagnostic-overrides", &[("Jig", "\
set diagnostic/unset-property 'warning'
log note missing
set diagnostic/unset-property 'allow'
log note hidden
set diagnostic/unset-property 'error'
log note '{try(lowered, 1)}'
set diagnostic/user 'error'
log note 'raised'
set diagnostic/user 'note'
log error 'lowered'
set diagnostic/repeated-key 'bogus'
set diagnostic/no-such-thing 'error'
crate util 'util.rs'
    target 'nope'
set diagnostic/unknown-target 'allow'
")]);
    let output = project.configure().failed();
    // a diagnostic has the severity of its property where it is reported
    output
        .stderr("warning[unset-property]: no value for property \"missing\"\n --> Jig:2:10")
        // an error is still lowered by `try`
        .stderr("warning[unset-property]: no value for property \"lowered\"\n --> Jig:6:16")
        .stderr("error: raised\n --> Jig:8:10")
        .stderr("note: lowered\n  --> Jig:10:11")
        .stderr("error[unknown-severity]: unknown severity \"bogus\"\n  --> Jig:11:29")
        .stderr("error[undeclared-property]: property \"diagnostic/no-such-thing\" is undeclared\n  --> Jig:12:5")
        .stderr("configuration failed with 3 errors, 2 warnings");
    // an allowed diagnostic is hidden, with what is attached to it
    assert!(!output.stderr.contains("hidden") && !output.stderr.contains("nope"), "{}", output.stderr);
}

#[test]
fn meta_with() {
    let project = Project::new("meta-with", &[("Jig", "\
declare level
    type 'unsigned'
    default 1
@with target as 'riscv64gc-unknown-none-elf'
crate kernel 'kernel.rs'
@then
log note target
log note target
@with level as 2
@for x in [1, 2]
log note '{x} {level}'
log note level
@with level as 'x'
log note 'typed'
@with host-target as 'x'
log note 'read-only'
@with nothing as 1
log note 'undeclared'
@with level 2
log note 'syntax'
@if false
@with level as {missing}
log note 'skipped'
@with level as 3
@if level == 3
log note 'three'
@else
log note 'not three'
@with diagnostic/unset-property as 'allow'
log note missing
log note '{level}'
@with level as 4
set level 5
log note '{level}'
@with level as 6
@with level as 7
log note '{level}'
log note '{level}'
set later 1
@with later as 2
log note '{later} later'
declare later
log note '{later} after'
@with checked as 'x'
log note checked
declare checked
    type 'unsigned'
@with revived as 3
log note revived
declare revived
log note '{try(revived, 'not revived')}'
")]);
    let host = project.host();
    let output = project.configure().failed();
    // the property is set for the statement and those chained to it, then restored
    output
        .stderr("note: riscv64gc-unknown-none-elf\n --> Jig:7:10")
        .stderr(&format!("note: {host}\n --> Jig:8:10"))
        .stderr("note: 1 2\n  --> Jig:11:10")
        .stderr("note: 2 2\n  --> Jig:11:10")
        .stderr("note: 1\n  --> Jig:12:10")
        // it is set as by `set`
        .stderr("error[mismatched-type]: invalid data type for property \"level\"\n  --> Jig:13:16")
        .stderr("error[read-only-property]: cannot set read-only builtin property \"host-target\"\n  --> Jig:15:7")
        // a property which is never declared is reported, although its value does not remain
        .stderr("note: undeclared\n")
        .stderr("error[undeclared-property]: property \"nothing\" is undeclared\n  --> Jig:17:7")
        .stderr("error[expected-exact]: expected \"as\"\n  --> Jig:19:13")
        // the conditions after it continue the chain
        .stderr("note: three\n  --> Jig:26:10")
        .stderr("note: 1\n  --> Jig:31:10")
        // unless the statements set it, while a nested `@with` restores its own value
        .stderr("note: 5\n  --> Jig:34:10")
        .stderr("note: 7\n  --> Jig:37:10")
        .stderr("note: 5\n  --> Jig:38:10")
        // the property may be declared later, as with `set`, which checks the value it was given
        .stderr("note: 2 later\n")
        .stderr("note: 1 after\n")
        .stderr("error[mismatched-type]: invalid data type for property \"checked\"\n  --> Jig:44:18")
        // but not given the value again
        .stderr("note: not revived\n")
        .stderr("configuration failed with 5 errors");
    for skipped in ["typed", "read-only", "syntax", "skipped", "not three", "missing"] {
        assert!(!output.stderr.contains(&format!("note: {skipped}")), "{}", output.stderr);
    }
}

#[test]
fn use_with_target() {
    let project = Project::new("use-with-target", &[("Jig", "\
crate util 'util.rs'
    target ['riscv64gc-unknown-none-elf', 'x86_64-unknown-none']
crate kernel 'kernel.rs'
    target 'riscv64gc-unknown-none-elf'
    use riscv64gc-unknown-none-elf/util
crate third 'third.rs'
    use host-target/util
")]);
    let host = project.host();
    project.configure().failed()
        // the crate is still used by its name, rather than taking the target as one
        .stderr("\
error[use-with-target]: cannot use a crate of a different target
 --> Jig:5:9
  |
5 |     use riscv64gc-unknown-none-elf/util
  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
note: declared here
 --> Jig:1:7
  |
1 | crate util 'util.rs'
  |       ^^^^
suggestion: use the crate by its name
 --> Jig:5:9
  |
5 |     use util
  |         ~~~~
")
        // without a declaration of the crate for the target, there is nothing to note
        .stderr("error[use-with-target]: cannot use a crate of a different target\n --> Jig:7:9\n  |\n7 |     use host-target/util\n  |         ^^^^^^^^^^^^^^^^\nsuggestion")
        .stderr("suggestion: use the crate by its name\n --> Jig:7:9\n  |\n7 |     use util\n")
        .stderr(&format!("error[unknown-crate]: no crate \"util\" for target \"{host}\"\n --> Jig:7:9"))
        .stderr("configuration failed with 3 errors");
}

#[test]
fn attribute_access() {
    let project = Project::new("attribute-access", &[("Jig", "\
declare data
    type 'unsigned'
    default 1
    info 'some data'
set data 2
log note '{data} {data.default} {data.read-only} {data.type} {data.info} {host-target.read-only}'
crate util 'util.rs'
    target ['riscv64gc-unknown-none-elf', 'my-kernel']
crate macros 'm.rs'
    crate-type 'proc-macro'
log note '{my-kernel/util.target} {macros.crate-type} {my-kernel/util.use} {my-kernel/util.path} {my-kernel/util.test}'
target my-kernel
    inherit 'riscv64gc-unknown-none-elf'
    test
    runner ['qemu', '-kernel']
    target-cpu 'generic-rv64'
target riscv64gc-unknown-none-elf
    override
log note '{my-kernel.inherit} {my-kernel.test} {my-kernel.runner} {my-kernel.target-cpu} {riscv64gc-unknown-none-elf.override}'
declare plain
log note plain.default
log note data.bogus
log note my-kernel.code-model
log note nothing.default
log note my-kernel/util.bogus
")]);
    project.configure_with(NIGHTLY).failed()
        // the attributes of a property are those it was declared with, rather than its value
        .stderr("note: 2 1 false unsigned some data true\n")
        .stderr("note: my-kernel proc-macro  util.rs false\n")
        .stderr("note: riscv64gc-unknown-none-elf true qemu -kernel generic-rv64 true\n")
        .stderr("error[attribute-not-set]: attribute \"default\" of property \"plain\" is not set\n  --> Jig:21:16")
        .stderr("error[unknown-attribute-of]: unknown attribute \"bogus\" of property \"data\"\n  --> Jig:22:15")
        .stderr("error[attribute-not-set]: attribute \"code-model\" of target \"my-kernel\" is not set\n  --> Jig:23:20")
        .stderr("error[unknown-reference]: no property, custom command, target or test named \"nothing\"\n  --> Jig:24:10")
        .stderr("error[unknown-attribute-of]: unknown attribute \"bogus\" of crate \"util\"\n  --> Jig:25:25")
        .stderr("configuration failed with 5 errors");
}

#[test]
fn custom_attribute_access() {
    let project = Project::new("custom-attribute-access", &[
        ("Jig", "use 'sub'\nlog note '{image.input} | {image.output} | {image.command} | {image.default} | {image}'\nlog note image.bogus\n"),
        ("sub/Jig", "\
custom image
    input ['a.dts', 'b.dtsi']
    output 'image.bin'
    command './tool.sh' '-o' output input
    default
"),
    ]);
    project.configure().failed()
        // the paths are relative to the project, as they are in the command, while an output is in the build directory
        .stderr("note: sub/a.dts sub/b.dtsi | build/image.bin | ./sub/tool.sh -o build/image.bin sub/a.dts sub/b.dtsi | true | build/image.bin\n")
        .stderr("error[unknown-attribute-of]: unknown attribute \"bogus\" of custom command \"image\"\n --> Jig:3:16")
        .stderr("configuration failed with 1 error");
}

#[test]
fn test_attribute_access() {
    let project = Project::new("test-attribute-access", &[
        ("Jig", "\
use 'sub'
log note '{unit.input} | {unit.command}'
log note unit.output
crate app 'app.rs'
    test
log note app.command
"),
        ("sub/Jig", "test unit\n    input 'data.txt'\n    command './check.sh' input\n"),
    ]);
    let host = project.host();
    project.configure().failed()
        .stderr("note: sub/data.txt | ./sub/check.sh sub/data.txt\n")
        .stderr("error[unknown-attribute-of]: unknown attribute \"output\" of test \"unit\"\n --> Jig:3:15")
        // the test of a crate is forward declared with it
        .stderr(&format!("note: ./build/{host}/test/app\n"))
        .stderr("configuration failed with 1 error");
}

#[test]
fn dicts() {
    let project = Project::new("dicts", &[("Jig", "\
declare n
    default 2
declare flags
    type 'dict'
    default [opt-level: 2, debug: true, {'x-{n}'}: 'y']
log note flags
log note '{get(flags, 'opt-level')} {get(flags, 'nope', 0)} {contains(flags, 'debug')} {contains(flags, 'x')} {len(flags)}'
log note '{keys(flags)} | {values(flags)}'
log note flags + [debug: false, extra: 'z']
log note '{[:] == [:]} {[a: 1, b: 2] == [b: 2, a: 1]} {len([:])} {get([1, 2], 1)}'
")]);
    project.configure().ok()
        // the entries are kept in the order of their names
        .stderr("note: debug: true, opt-level: 2, x-2: y\n")
        .stderr("note: 2 0 true false 3\n")
        .stderr("note: debug opt-level x-2 | true 2 y\n")
        // the entries on the right replace those on the left
        .stderr("note: debug: false, extra: z, opt-level: 2, x-2: y\n")
        .stderr("note: true true 0 2\n");
}

#[test]
fn dict_errors() {
    let project = Project::new("dict-errors", &[("Jig", "\
declare flags
    type 'dict'
    default [:]
log note [a: 1, a: 2]
log note [a: 1, 2]
log note get(flags, 'x')
log note flags + [1]
log note keys([1])
set flags [1]
")]);
    project.configure().failed()
        .stderr("error[repeated-key]: key \"a\" is repeated in the dict\n --> Jig:4:17")
        .stderr("error[expected-dict-entry]: expected an entry of a dict, as `name: value`\n --> Jig:5:17")
        .stderr("error[missing-key]: no entry for key \"x\"\n --> Jig:6:21")
        .stderr("error[mismatched-operands]: mismatched operand types dict and list\n --> Jig:7:10")
        .stderr("error[invalid-operand]: list value used where dict is required\n --> Jig:8:15")
        .stderr("error[mismatched-type]: invalid data type for property \"flags\"\n --> Jig:9:11")
        .stderr("configuration failed with 6 errors");
}

#[test]
fn install_attributes() {
    let project = Project::new("install-attributes", &[("Jig", "\
crate app 'app.rs'
    crate-type 'bin'
crate util 'util.rs'
crate macros 'macros.rs'
    crate-type 'proc-macro'
custom data
    command 'true'
crate tool 'tool.rs'
    install-mode 0o700
    crate-type 'bin'
    install
custom page
    output 'page.1'
    command 'true'
    install true
    install-dir 'share/man/man1'
    install-mode 0o444
log note '{app.install} {app.install-dir} {app.install-mode}'
log note '{util.install} {util.install-dir} {util.install-mode}'
log note '{macros.install} {macros.install-dir} {macros.install-mode}'
log note '{data.install} {data.install-dir} {data.install-mode}'
log note '{tool.install} {tool.install-dir} {tool.install-mode}'
log note '{page.install} {page.install-dir} {page.install-mode}'
log note install-prefix
")]);
    let host = project.host();
    let output = project.configure().ok();
    // the defaults depend on the item, and the modes are permissions, shown in decimal
    output
        .stderr("note: false bin 493\n")
        .stderr("note: false lib 420\n")
        .stderr("note: false lib 493\n")
        .stderr("note: false share 420\n")
        .stderr("note: true bin 448\n")
        .stderr("note: true share/man/man1 292\n")
        .stderr("note: /usr/local\n");
    // `install` builds what is installed
    assert_contains(&output.ninja, &format!("\nbuild install: phony {host}/tool page.1\n"));
}

#[test]
fn install_errors() {
    let project = Project::new("install-errors", &[("Jig", "\
crate a 'a.rs'
    install-mode 0o10000
crate b 'b.rs'
    install 'yes'
    install-dir
custom c
    command 'true'
    install-mode -1
test d
    command 'true'
    install
@for target in 'x86_64-unknown-linux-gnu' 'aarch64-unknown-linux-gnu'
crate e 'e.rs'
    crate-type 'bin'
    install
custom f
    output 'sub/e'
    command 'true'
    install-dir 'bin'
    install
")]);
    project.configure().failed()
        .stderr("error[invalid-install-mode]: invalid install mode 0o10000, which must be permissions from 0 to 0o7777\n --> Jig:2:18")
        .stderr("error[invalid-operand]: str value used where bool is required\n --> Jig:4:13")
        .stderr("error[invalid-operand]: none value used where str is required\n --> Jig:5:5")
        .stderr("error[invalid-install-mode]: invalid install mode -1, which must be permissions from 0 to 0o7777\n --> Jig:8:18")
        .stderr("error[unknown-attribute]: unknown test attribute \"install\"\n  --> Jig:11:5")
        .stderr("error[install-conflict]: the crate \"x86_64-unknown-linux-gnu/e\" installs \"bin/e\", which the crate \"aarch64-unknown-linux-gnu/e\" also installs")
        .stderr("error[install-conflict]: the custom command \"f\" installs \"bin/e\", which the crate \"aarch64-unknown-linux-gnu/e\" also installs");
}

#[test]
fn install_command() {
    let project = Project::new("install-command", &[
        ("Jig", "\
set install-prefix 'stage'
crate app 'app.rs'
    crate-type 'bin'
    install
crate util 'util.rs'
custom doc
    output 'doc/a.txt' 'doc/b.txt'
    command 'sh' '-c' 'echo a > \"$1\"; echo b > \"$2\"' 'sh' output
    install
    install-dir 'share/doc/app'
    install-mode 0o600
"),
        ("app.rs", "fn main() {}\n"),
        ("util.rs", ""),
    ]);
    project.configure().ok();
    let install = |args: &[&str]| {
        let output = Command::new(configure_path()).arg("install").args(args).current_dir(&project.dir).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "install failed:\n{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
        stdout
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    // the `install-prefix` property is relative to the project root
    let stdout = install(&[]);
    let stage = project.dir.join("stage");
    assert_contains(&stdout, &format!("INSTALL {}\n", stage.join("bin/app").display()));
    assert!(stdout.ends_with(&format!("\ninstalled 3 files in {}\n", stage.display())), "{stdout}");
    assert_eq!(mode(&stage.join("bin/app")), 0o755);
    assert_eq!(std::fs::read_to_string(stage.join("share/doc/app/b.txt")).unwrap(), "b\n");
    assert_eq!(mode(&stage.join("share/doc/app/a.txt")), 0o600);
    // a library without `install` is not installed
    assert!(!stage.join("lib").exists());
    // a prefix which is given replaces it, and installing again replaces the files
    install(&["other"]);
    install(&["other"]);
    assert_eq!(mode(&project.dir.join("other/bin/app")), 0o755);
}

#[test]
fn install_proxy() {
    let project = Project::new("install-proxy", &[
        ("Jig", "\
crate app 'app.rs'
    crate-type 'bin'
    install
custom doc
    output 'doc.txt'
    command 'sh' '-c' 'echo doc > \"$1\"' 'sh' output
    install
"),
        ("app.rs", "fn main() { println!(\"one {:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }\n"),
    ]);
    project.configure().ok();
    let output = Command::new(configure_path()).args(["install", "--proxy", "stage"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8(output.stdout).unwrap().ends_with("\ninstalled 1 wrapper and 1 link in stage\n"));
    let stage = project.dir.join("stage");
    // a program is a wrapper, which runs it with its arguments, from any directory, with only its output on stdout
    let run_with_stderr = || {
        let output = Command::new(stage.join("bin/app")).arg("a b").current_dir(std::env::temp_dir()).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        (String::from_utf8(output.stdout).unwrap(), stderr)
    };
    let run = || run_with_stderr().0;
    // it is up to date, so it is run without a diagnostic
    assert_eq!(run_with_stderr(), ("one [\"a b\"]\n".into(), String::new()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(stage.join("bin/app")).unwrap().permissions().mode() & 0o7777, 0o755);
    // which builds it again when it changes, with a warning
    std::fs::write(project.dir.join("app.rs"), "fn main() { println!(\"two\"); }\n").unwrap();
    let (stdout, stderr) = run_with_stderr();
    assert_eq!(stdout, "two\n");
    let host = project.host();
    assert_eq!(stderr, format!(
        "warning[proxy-build]: building \"{host}/app\"\n",
    ));
    // as is a change to the configuration alone, before what configuring reports
    let jig = std::fs::read_to_string(project.dir.join("Jig")).unwrap();
    std::fs::write(project.dir.join("Jig"), format!("{jig}log note 'configured'\n    quiet\n")).unwrap();
    let (stdout, stderr) = run_with_stderr();
    assert_eq!(stdout, "two\n");
    assert!(stderr.starts_with(&format!(
        "warning[proxy-build]: regenerating the configuration and building \"{host}/app\"\nnote: configured\n",
    )), "{stderr}");
    assert_eq!(run_with_stderr(), ("two\n".into(), String::new()));
    // any other file is a link to the output
    let doc = stage.join("share/doc.txt");
    assert_eq!(std::fs::read_link(&doc).unwrap(), project.dir.join("build/doc.txt"));
    // installing without `--proxy` replaces the wrapper and the link with copies
    let output = Command::new(configure_path()).args(["install", "stage"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(!std::fs::symlink_metadata(&doc).unwrap().is_symlink());
    assert_eq!(run(), "two\n");
}

#[test]
fn install_nothing() {
    let project = Project::new("install-nothing", &[("Jig", "crate util 'util.rs'\n"), ("util.rs", "")]);
    project.configure().ok();
    let output = Command::new(configure_path()).arg("install").current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.ends_with("\nnothing to install\n"), "{stdout}");
}

#[test]
fn proxy_build_severity() {
    let project = Project::new("proxy-build-severity", &[
        ("Jig", "\
set diagnostic/proxy-build 'error'
crate app 'app.rs'
    crate-type 'bin'
    install
"),
        ("app.rs", "fn main() { println!(\"one\"); }\n"),
    ]);
    project.configure().ok();
    let output = Command::new(configure_path()).args(["install", "--proxy", "stage"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let run = || Command::new(project.dir.join("stage/bin/app")).output().unwrap();
    // as an error, an out of date program is neither built nor run
    std::fs::write(project.dir.join("app.rs"), "fn main() { println!(\"two\"); }\n").unwrap();
    let output = run();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
    assert_contains(&stderr, "error[proxy-build]: ");
    assert_contains(&stderr, "not building or running the program");
    // as `allow`, it is built without one
    let jig = std::fs::read_to_string(project.dir.join("Jig")).unwrap();
    std::fs::write(project.dir.join("Jig"), jig.replace("'error'", "'allow'")).unwrap();
    project.configure().ok();
    let output = run();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "two\n");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn proxy_reinstalls_itself() {
    let tool = |output: &str, mode: &str| format!("\
custom tool
    output '{output}'
    command 'sh' '-c' 'printf \"#!/bin/sh\\necho tool \\\"\\$*\\\"\\n\" > \"$1\"; chmod +x \"$1\"' 'sh' output
    install
    install-dir 'bin'
    install-mode {mode}
");
    let project = Project::new("proxy-reinstall", &[("Jig", &tool("tool", "0o755"))]);
    project.configure().ok();
    let output = Command::new(configure_path()).args(["install", "--proxy", "stage"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let wrapper = project.dir.join("stage/bin/tool");
    let run = || {
        let output = Command::new(&wrapper).arg("a b").current_dir(std::env::temp_dir()).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        (output.status.success(), String::from_utf8(output.stdout).unwrap(), stderr)
    };
    let script = || std::fs::read_to_string(&wrapper).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = || std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o7777;
    let installed = script();
    let (success, stdout, stderr) = run();
    assert!(success, "{stderr}");
    assert_eq!(stdout, "tool a b\n");
    // an up to date wrapper is left as it is
    assert_eq!(script(), installed);

    // when the program moves, the wrapper is reinstalled, then run again, which runs the program once
    std::fs::write(project.dir.join("Jig"), tool("v2/tool", "0o700")).unwrap();
    let (success, stdout, stderr) = run();
    assert!(success, "{stderr}");
    assert_eq!(stdout, "tool a b\n");
    assert_contains(&stderr, "warning[proxy-build]: ");
    assert_contains(&script(), "/build/v2/tool");
    assert_eq!(mode(), 0o700);
    // and is then up to date
    let reinstalled = script();
    let (success, _, stderr) = run();
    assert!(success);
    assert_eq!(stderr, "");
    assert_eq!(script(), reinstalled);

    // as it is when the build directory changes
    std::fs::write(project.dir.join("Jig"), format!("set build-dir 'out'\n{}", tool("v2/tool", "0o700"))).unwrap();
    let (success, stdout, stderr) = run();
    assert!(success, "{stderr}");
    assert_eq!(stdout, "tool a b\n");
    assert_contains(&script(), "/out/v2/tool");

    // a wrapper which the project no longer installs fails
    std::fs::write(project.dir.join("Jig"), tool("v2/tool", "0o700").replace("    install\n", "")).unwrap();
    let (success, stdout, stderr) = run();
    assert!(!success);
    assert_eq!(stdout, "");
    assert_contains(&stderr, &format!("no program is installed at {wrapper:?}"));
}

#[test]
fn build_dir_change() {
    let project = Project::new("build-dir-change", &[
        ("Jig", "\
custom data
    command 'sh' '-c' 'echo data > \"$1\"' 'sh' output
    default
test passes
    input data
    command 'true'
"),
    ]);
    project.configure().ok();
    let jig = |args: &[&str]| {
        let output = Command::new(configure_path()).args(args).current_dir(&project.dir).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{args:?} failed:\n{}\n{stderr}", String::from_utf8_lossy(&output.stdout));
    };
    jig(&["build"]);
    assert!(project.dir.join("build/data").is_file());
    // changing `build-dir` regenerates `build.ninja` in the new build directory, where the build then runs
    let config = std::fs::read_to_string(project.dir.join("Jig")).unwrap();
    std::fs::write(project.dir.join("Jig"), format!("set build-dir 'out'\n{config}")).unwrap();
    jig(&["build", "custom/data"]);
    assert!(project.dir.join("out/data").is_file());
    assert_contains(&std::fs::read_to_string(project.dir.join("out/build.ninja")).unwrap(), "\nbuild custom/data: phony data\n");
    // the previous build directory has a stub, which is up to date for ninja
    let stub = std::fs::read_to_string(project.dir.join("build/build.ninja")).unwrap();
    assert_contains(&stub, "`build-dir` is now \"out\"");
    let ninja = Command::new("ninja").current_dir(project.dir.join("build")).output().unwrap();
    assert!(ninja.status.success());
    // and changing it back regenerates it there again
    std::fs::write(project.dir.join("Jig"), config).unwrap();
    jig(&["test"]);
    assert_contains(&std::fs::read_to_string(project.dir.join("build/build.ninja")).unwrap(), "\nbuild test/passes: phony data\n");
    assert_contains(&std::fs::read_to_string(project.dir.join("out/build.ninja")).unwrap(), "`build-dir` is now \"build\"");
}

#[test]
fn proxy_rebuilds_its_jig() {
    // the program is the `jig` the wrapper runs, which is replaced while it builds it, as when `jig` is built by itself
    let configure = configure_path();
    let quoted = format!("'{}'", configure.display());
    let project = Project::new("proxy-jig", &[
        ("Jig", &format!("\
custom jig
    input 'stamp' {quoted}
    command 'sh' '-c' 'cp \"$1\" \"$2.tmp\" && mv \"$2.tmp\" \"$2\"' 'sh' {quoted} output
    install
    install-dir 'bin'
    install-mode 0o755
")),
        ("stamp", ""),
    ]);
    project.configure().ok();
    let built = project.dir.join("build/jig");
    let output = Command::new(&configure).args(["build", "custom/jig"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    let output = Command::new(&built).args(["install", "--proxy", "stage"]).current_dir(&project.dir).output().unwrap();
    assert!(output.status.success(), "install failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let wrapper = project.dir.join("stage/bin/jig");
    let installed = std::fs::read_to_string(&wrapper).unwrap();
    std::fs::write(project.dir.join("stamp"), "changed").unwrap();
    let output = Command::new(&wrapper).arg("help").output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert_contains(&String::from_utf8(output.stdout).unwrap(), "usage: jig");
    assert_contains(&stderr, "warning[proxy-build]: building \"jig\"");
    // the wrapper still runs the `jig` at its path, which has been replaced
    assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), installed);
}

#[test]
fn depends_attribute() {
    let project = Project::new("depends", &[
        ("Jig", "\
custom data
    command 'sh' '-c' 'echo data > \"$1\"' 'sh' output
crate app 'app.rs'
    crate-type 'bin'
    test
    depends data
custom copy
    input 'in.txt'
    depends data 'extra.txt'
    output 'copy.txt'
    command 'cp' input output
test runs
    depends app
    command 'true'
log note '{app.depends} | {copy.depends} | {runs.depends} | {copy.input}'
"),
        ("app.rs", "fn main() {}\n#[test]\nfn data() { assert!(std::path::Path::new(\"build/data\").exists()); }\n"),
        ("in.txt", ""),
        ("extra.txt", ""),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    // the paths are resolved as inputs are, but are not inputs
    output.stderr(&format!("note: build/data | build/data extra.txt | build/{host}/app | in.txt\n"));
    // which ninja builds first, without rebuilding when they change
    assert_contains(&output.ninja, &format!("\nbuild {host}/app: rustc app.rs || data\n"));
    assert_contains(&output.ninja, &format!("\nbuild {host}/test/app: rustc app.rs || data\n"));
    assert_contains(&output.ninja, "\nbuild copy.txt: custom in.txt || data extra.txt\n");
    // and which are built for a test
    assert_contains(&output.ninja, &format!("\nbuild test/app: phony {host}/test/app data\n"));
    assert_contains(&output.ninja, &format!("\nbuild test/runs: phony {host}/app\n"));
    // so that the test of the crate finds what it depends on
    let run = Command::new(configure_path()).args(["test", "app"]).current_dir(&project.dir).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stdout));
}

#[test]
fn host_compatible_targets_test() {
    let host = Command::new("rustc").arg("--print=host-tuple").output().unwrap();
    let host = String::from_utf8(host.stdout).unwrap().trim().to_string();
    // a musl target which the host can run, as it is statically linked
    let Some(musl) = host.strip_suffix("-gnu").map(|prefix| format!("{prefix}-musl")) else {
        return;
    };
    let project = Project::new("host-compatible", &[("Jig", &format!("\
crate app 'app.rs'
    target '{musl}'
    test
"))]);
    let output = project.configure().ok();
    // tests are allowed for the target without a `target` statement
    assert_contains(&output.ninja, &format!("\nbuild test/app@{musl}: phony {musl}/test/app\n"));
    // or with one which does not change it
    let project = Project::new("host-compatible-override", &[("Jig", &format!("\
target {musl}
    override
    runner 'env'
log note '{{{musl}.test}}'
"))]);
    project.configure().ok().stderr("note: true\n");
    // unless it disallows them
    let project = Project::new("host-compatible-disallowed", &[("Jig", &format!("\
target {musl}
    override
    test false
crate app 'app.rs'
    target '{musl}'
    test
"))]);
    project.configure().failed().stderr(&format!("error[test-target]: tests for target \"{musl}\" cannot be run"));
}

#[test]
fn unique_and_sort() {
    let project = Project::new("unique-and-sort", &[("Jig", "\
log note '{unique('a', 'b', 'a')} | {unique([1, 2], 2, [3, 1])}'
log note '{sort('b', 'a', 'c')} | {sort([3, -1, 2])} | {sort(true, false)} | {len(sort([]))}'
log note '{sort(unique(['b', 'a'], 'b'))}'
")]);
    project.configure().ok()
        // each item of a list is a value, and the first of those which are equal is kept
        .stderr("note: a b | 1 2 3\n")
        // integers of either sign are compared as signed
        .stderr("note: a b c | -1 2 3 | false true | 0\n")
        .stderr("note: a b\n");
}

#[test]
fn unique_and_sort_errors() {
    let project = Project::new("unique-and-sort-errors", &[("Jig", "\
log note sort('a', 1)
log note sort([['a']])
log note sort(0xffff_ffff_ffff_ffff, -1)
log note unique()
")]);
    project.configure().failed()
        .stderr("error[mismatched-operands]: mismatched operand types str and unsigned\n --> Jig:1:10")
        .stderr("error[invalid-operand]: list value used where str, bool or an integer is required\n --> Jig:2:15")
        .stderr("error[integer-overflow]: integer overflow\n --> Jig:3:15")
        .stderr("error[argument-count]: function \"unique\" takes at least 1 argument, but 0 were given\n --> Jig:4:10");
}

#[test]
fn default_flag() {
    // `default` is `true` without a value, or takes a `bool`
    let project = Project::new("default-flag", &[("Jig", "\
crate a 'a.rs'
    default
crate b 'b.rs'
    default false
crate c 'c.rs'
    default target == host-target
custom d
    command 'true'
    default true
custom e
    command 'true'
    default 1 == 2
crate f 'f.rs'
    builtin false
    test {'false'}
crate g 'g.rs'
    builtin
    test true
log note '{a.default} {b.default} {c.default} {d.default} {e.default}'
log note '{f.builtin} {f.test} {g.builtin} {g.test}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    output.stderr("note: true false true true false\n").stderr("note: false false true true\n");
    // `builtin` and `test` of crates are flags too
    assert_contains(&output.ninja, &format!("\nbuild test/g: phony {host}/test/g\n"));
    assert!(!output.ninja.contains("test/f"), "{}", output.ninja);
    assert_contains(&output.ninja, &format!("\ndefault custom/d {host}/liba.rlib {host}/libc.rlib\n"));
    let project = Project::new("default-flag-errors", &[("Jig", "\
crate a 'a.rs'
    default 'yes'
    builtin 'yes'
    test 0
custom b
    command 'true'
    default 1
")]);
    project.configure().failed()
        .stderr("error[invalid-operand]: str value used where bool is required\n --> Jig:2:13")
        .stderr("error[invalid-operand]: str value used where bool is required\n --> Jig:3:13")
        .stderr("error[invalid-operand]: unsigned value used where bool is required\n --> Jig:4:10")
        .stderr("error[invalid-operand]: unsigned value used where bool is required\n --> Jig:7:13");
}

#[test]
fn target_override_linker() {
    let project = Project::new("target-override-linker", &[("Jig", "\
target aarch64-unknown-linux-musl
    override
    linker 'rust-lld'
    linker-flavor 'ld.lld'
target riscv64gc-unknown-linux-musl
    override
    linker 'tools/ld.lld'
crate app 'app.rs'
    crate-type 'bin'
    target ['aarch64-unknown-linux-musl', 'riscv64gc-unknown-linux-musl']
crate own 'own.rs'
    crate-type 'bin'
    target 'aarch64-unknown-linux-musl'
    codegen/linker 'cc'
log note '{aarch64-unknown-linux-musl.linker} {aarch64-unknown-linux-musl.linker-flavor} {riscv64gc-unknown-linux-musl.linker}'
")]);
    let output = project.configure().ok();
    // the linker of a builtin target is given to the crates for it, where a path is relative to the file
    output.stderr("note: rust-lld ld.lld tools/ld.lld\n");
    assert_contains(output.build("aarch64-unknown-linux-musl/app"), "\
    flags = --target=aarch64-unknown-linux-musl -L dependency=aarch64-unknown-linux-musl -C linker=rust-lld -C linker-flavor=ld.lld");
    assert_contains(output.build("riscv64gc-unknown-linux-musl/app"), " -C linker=tools/ld.lld");
    // unless a crate has its own, as a linker and its flavor go together
    let own = output.build("aarch64-unknown-linux-musl/own");
    assert_contains(own, " -C linker=cc");
    assert!(!own.contains("rust-lld") && !own.contains("linker-flavor"), "{own}");
}

#[test]
fn toolchain_option() {
    let project = Project::new("toolchain", &[("Jig", "\
log note 'toolchain: {toolchain}'
    quiet
custom version
    output 'version.txt'
    command 'sh' '-c' 'rustc --version > \"$1\"' 'sh' output
    default
")]);
    let jig = |args: &[&str]| {
        let output = Command::new(configure_path()).args(args).current_dir(&project.dir).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        (output.status.success(), String::from_utf8(output.stdout).unwrap() + &stderr)
    };
    let version = || std::fs::read_to_string(project.dir.join("build/version.txt")).unwrap();
    // the project is configured with a toolchain given with `+`, which its commands are run with
    let (success, output) = jig(&["+nightly", "generate"]);
    assert!(success, "{output}");
    assert_contains(&output, "note: toolchain: nightly\n");
    let ninja = project.ninja();
    assert_contains(&ninja, "' '+nightly' generate 'Jig'\n");
    assert_contains(&ninja, "\n    command = RUSTUP_TOOLCHAIN='nightly' $crate-env rustc ");
    assert_contains(&ninja, " && RUSTUP_TOOLCHAIN='nightly' $command-line\n");
    // and which is kept by the other commands
    let (success, output) = jig(&["build"]);
    assert!(success, "{output}");
    assert_contains(&version(), "-nightly");
    // until it is configured without one
    let (success, output) = jig(&["generate"]);
    assert!(success, "{output}");
    assert_contains(&output, "note: toolchain: \n");
    assert!(!project.ninja().contains("RUSTUP_TOOLCHAIN"));
    // or another command is given one, which configures it again
    let (success, output) = jig(&["+nightly", "build"]);
    assert!(success, "{output}");
    assert_contains(&output, "note: toolchain: nightly\n");
    assert_contains(&version(), "-nightly");
    let (success, output) = jig(&["+no-such-toolchain", "build"]);
    assert!(!success);
    assert_contains(&output, "cannot use toolchain \"no-such-toolchain\": toolchain 'no-such-toolchain' is not installed");
    let (success, output) = jig(&["+", "build"]);
    assert!(!success);
    assert_contains(&output, "missing required argument `toolchain`");
}

#[test]
fn semver_compatible() {
    let project = Project::new("semver-compatible", &[("Jig", "\
log note '{semver-compatible('1.4.0', '1.2')} {semver-compatible('2.0.0', '1.2')} {semver-compatible('0.3.0', '0.2.1')}'
log note '{semver-compatible('1.2.3-beta.2', '1.2.3-beta.1')} {semver-compatible('1.3.0-beta', '1.2')}'
")]);
    project.configure().ok()
        // as cargo compares a version to a dependency's requirement
        .stderr("note: true false false\n")
        // where a pre-release only meets a requirement for one of the same version
        .stderr("note: true false\n");
    let project = Project::new("semver-compatible-errors", &[("Jig", "\
log note semver-compatible('1.2', '1')
log note semver-compatible('1.2.3', 'x')
")]);
    project.configure().failed()
        .stderr("error[invalid-version]: invalid version \"1.2\", as its patch number is missing\n --> Jig:1:28")
        .stderr("error[invalid-version]: invalid version requirement \"x\", as its major number, \"x\", is not a number\n --> Jig:2:37");
}

#[test]
fn log_id() {
    let project = Project::new("log-id", &[("Jig", "\
set diagnostic/deprecated 'warning'
log note 'old api'
    id 'deprecated'
log note 'quiet one'
    id 'deprecated'
    quiet
@with diagnostic/deprecated as 'allow'
log error 'hidden'
    id 'deprecated'
log note 'plain'
log note diagnostic/deprecated.info
    quiet
")]);
    let output = project.configure().ok();
    // the identifier is shown, and its property, which may be set before the first, overrides the severity
    output
        .stderr("warning[deprecated]: old api\n --> Jig:2:10")
        .stderr("warning[deprecated]: quiet one\n")
        .stderr("note: plain\n")
        .stderr("note: the severity of the diagnostic where it is reported");
    assert!(!output.stderr.contains("hidden"), "{}", output.stderr);
    let project = Project::new("log-id-errors", &[("Jig", "\
log note 'a'
    id 'no spaces'
log note 'b'
    id ''
")]);
    project.configure().failed()
        .stderr("error[invalid-identifier]: invalid character ' ' in identifier \"no spaces\"\n --> Jig:2:8")
        .stderr("error[expected-identifier]: expected identifier\n --> Jig:4:8");
}

#[test]
fn rustc_properties() {
    let project = Project::new("rustc-properties", &[("Jig", "\
log note 'version {rustc/version}'
log note 'sysroot {rustc/sysroot}'
log note keys(rustc/)
log note semver-compatible(rustc/version, '1')
log note 'targets {len(rustc/targets)} {contains(rustc/targets, host-target)} {contains(rustc/targets, 'no-such-target')}'
")]);
    let rustc = |arg: &str| {
        let output = Command::new("rustc").arg(arg).current_dir(&project.dir).output().unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let version = rustc("-vV");
    let version = version.lines().find_map(|line| line.strip_prefix("release: ")).unwrap().to_string();
    let sysroot = rustc("--print=sysroot");
    let targets = rustc("--print=target-list").lines().count();
    project.configure().ok()
        // the version is the release of rustc, which is semantic
        .stderr(&format!("note: version {version}\n"))
        .stderr(&format!("note: sysroot {}\n", sysroot.trim()))
        .stderr("note: sysroot targets version\n")
        .stderr(&format!("note: {}\n", !version.contains('-')))
        // the builtin targets, which include the host
        .stderr(&format!("note: targets {targets} true false\n"));
    let project = Project::new("rustc-properties-read-only", &[("Jig", "set rustc/version '2.0.0'\n")]);
    project.configure().failed().stderr("error[read-only-property]: ");
}

#[test]
fn semver_compatible_ignoring_pre_releases() {
    let project = Project::new("semver-ignore-pre", &[("Jig", "\
log note semver-compatible('1.101.0-nightly', '1.95')
log note semver-compatible('1.101.0-nightly', '1.95', ignore-pre-release: true)
log note semver-compatible('1.95.0-nightly', '1.95', ignore-pre-release: {'true'})
log note semver-compatible('1.94.0-nightly', '1.95', ignore-pre-release: true)
")]);
    // a nightly version is compatible as its release would be
    project.configure().ok().stderr("note: false\n").stderr("note: true\n --> Jig:2:10").stderr("note: true\n --> Jig:3:10")
        .stderr("note: false\n --> Jig:4:10");
    let project = Project::new("semver-ignore-pre-errors", &[("Jig", "\
log note semver-compatible('1.0.0', '1', nightly: true)
log note semver-compatible('1.0.0', '1', ignore-pre-release: true, ignore-pre-release: false)
log note semver-compatible('1.0.0', ignore-pre-release: 1)
")]);
    project.configure().failed()
        .stderr("error[unknown-keyword-argument]: function \"semver-compatible\" has no keyword argument \"nightly\"\n --> Jig:1:42")
        .stderr("error[repeated-keyword-argument]: keyword argument \"ignore-pre-release\" cannot be repeated\n --> Jig:2:68")
        .stderr("error[argument-count]: function \"semver-compatible\" takes 2 arguments, but 1 was given\n --> Jig:3:10");
}

#[test]
fn target_has_std() {
    let project = Project::new("target-has-std", &[("Jig", "\
target custom
    inherit 'x86_64-unknown-none'
log note '{({host-target}.has-std)} {riscv64gc-unknown-none-elf.has-std} {custom.has-std}'
log note '{riscv64gc-unknown-none-elf.override} {riscv64gc-unknown-none-elf.test} {({host-target}.test)}'
@for target in host-target 'riscv64gc-unknown-none-elf'
log note {target}.has-std
")]);
    let host = project.host();
    let output = project.configure_with(NIGHTLY).ok();
    // the standard library of the toolchain's host is installed with it, while a target without an operating system
    // has none, and rustup has none for a custom target
    output.stderr("note: true false false\n");
    // the attributes of a builtin target without a `target` statement are its defaults
    output.stderr("note: false false true\n");
    // of a target computed from a property, as for those of `targets`
    output.stderr("note: true\n --> Jig:6:10").stderr("note: false\n --> Jig:6:10");
    // the configuration is generated again when the libraries of a target change, such as when its standard library
    // is installed
    assert_contains(&project.depfile(), &format!("/lib/rustlib/{host}/lib "));
}

#[test]
fn target_can_run() {
    let project = Project::new("target-can-run", &[("Jig", "\
target s390x-unknown-linux-gnu
    override
    runner 'true'
target powerpc64le-unknown-linux-gnu
    override
    runner 'false'
log note '{({host-target}.can-run)} {s390x-unknown-linux-gnu.can-run} {powerpc64le-unknown-linux-gnu.can-run}'
log note '{riscv64gc-unknown-none-elf.can-run} {s390x-unknown-linux-gnu.test} {powerpc64le-unknown-linux-gnu.test}'
crate app 'app.rs'
    target 's390x-unknown-linux-gnu'
    test
")]);
    let output = project.configure().ok();
    // a probe is run with the runner of a target, which here stands in for an emulator, while the host target can run
    // its own, and a target which is not for Linux has no probe
    output.stderr("note: true true false\n").stderr("note: false true false\n");
    // and `test` is whether it can run by default
    assert_contains(&output.ninja, "\nbuild test/app@s390x-unknown-linux-gnu: phony s390x-unknown-linux-gnu/test/app\n");
    let project = Project::new("target-can-run-errors", &[("Jig", "\
target powerpc64le-unknown-linux-gnu
    override
    runner 'false'
crate app 'app.rs'
    target 'powerpc64le-unknown-linux-gnu'
    test
")]);
    project.configure().failed()
        .stderr("error[test-target]: tests for target \"powerpc64le-unknown-linux-gnu\" cannot be run");
}

#[test]
fn dotted_target_names() {
    // some targets have a `.`, which a computed name may have, as one written in a name takes an attribute
    let project = Project::new("dotted-targets", &[("Jig", "\
@for target in rustc/targets
@if {target}.has-std
log note 'std {target}'
    quiet
crate app 'app.rs'
    target 'thumbv8m.main-none-eabihf'
log note '{({'thumbv8m.main-none-eabihf'}/app)} {({'thumbv8m.main-none-eabihf'}.can-run)}'
")]);
    let host = project.host();
    project.configure().ok()
        .stderr(&format!("note: std {host}\n"))
        .stderr("note: build/thumbv8m.main-none-eabihf/libapp.rlib false\n");
    // while a crate's name is given to rustc, which rejects it
    let project = Project::new("dotted-crate-name", &[("Jig", "crate {'a.b'}\n")]);
    project.configure().failed().stderr("error[invalid-crate-name]: invalid character '.' in crate name \"a.b\"");
}

#[test]
fn proxy_installed_by_itself() {
    // a `jig` which installs wrappers over itself, as when jig installs itself, does not make a wrapper run itself
    let configure = configure_path();
    let quoted = format!("'{}'", configure.display());
    let project = Project::new("proxy-itself", &[("Jig", &format!("\
custom jig
    input {quoted}
    command 'sh' '-c' 'cp \"$1\" \"$2.tmp\" && mv \"$2.tmp\" \"$2\"' 'sh' {quoted} output
    install
    install-dir 'bin'
    install-mode 0o755
"))]);
    project.configure().ok();
    let run = |program: &std::path::Path, args: &[&str]| {
        let output = Command::new(program).args(args).current_dir(&project.dir).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{program:?} {args:?} failed:\n{stderr}");
        String::from_utf8(output.stdout).unwrap()
    };
    run(&configure, &["install", "stage"]);
    let wrapper = project.dir.join("stage/bin/jig");
    run(&wrapper, &["install", "--proxy", "stage"]);
    let script = std::fs::read_to_string(&wrapper).unwrap();
    assert!(!script.contains(&format!("exec '{}' build", wrapper.display())), "{script}");
    assert_contains(&script, &format!("exec '{}' build", project.dir.join("build/jig").display()));
    assert_contains(&run(&wrapper, &["help"]), "usage: jig");
}

#[test]
fn generates_when_needed() {
    let project = Project::new("generates-when-needed", &[
        ("Jig", "crate app 'app.rs'\n    crate-type 'bin'\n    default\n"),
        ("app.rs", "fn main() {}\n"),
        ("sub/file", ""),
    ]);
    let host = project.host();
    let build = |dir: &str| {
        let output = Command::new(configure_path()).arg("build").current_dir(project.dir.join(dir)).output().unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    };
    let app = project.dir.join(format!("build/{host}/app"));
    // a project which has not been configured is configured from its `Jig`, found from within it
    build("sub");
    assert!(app.is_file() && project.dir.join(".jig").is_file());
    // as it is when its build directory has been removed
    std::fs::remove_dir_all(project.dir.join("build")).unwrap();
    build(".");
    assert!(app.is_file());
    // or its database is of another version
    std::fs::write(project.dir.join(".jig"), b"JIGD\x01\x00\x00\x00").unwrap();
    build(".");
    assert!(std::fs::read(project.dir.join(".jig")).unwrap() != b"JIGD\x01\x00\x00\x00");
    // while without a project, there is nothing to configure
    let outside = std::env::temp_dir();
    let output = Command::new(configure_path()).arg("build").current_dir(&outside).output().unwrap();
    if !outside.ancestors().any(|dir| dir.join("Jig").is_file() || dir.join(".jig").is_file()) {
        assert!(!output.status.success());
        assert_contains(&String::from_utf8(output.stderr).unwrap(), "no `.jig` or `Jig` in the working directory or its parents");
    }
}

#[test]
fn target_codegen_options() {
    let project = Project::new("target-codegen", &[("Jig", "\
set codegen/opt-level 2
set codegen/{host-target}/opt-level 3
set codegen/riscv64gc-unknown-none-elf/debuginfo 1
set codegen/custom/opt-level 's'
target custom
    inherit 'x86_64-unknown-none'
crate app 'app.rs'
    target [host-target, 'riscv64gc-unknown-none-elf', 'custom']
crate own 'own.rs'
    codegen/opt-level 1
log note codegen/{host-target}/
log note '{len(codegen/aarch64-unknown-none/)} {codegen/{host-target}/lto.info}'
")]);
    let host = project.host();
    let output = project.configure_with(NIGHTLY).ok();
    let flags = |output_path: &str| output.build(output_path).lines().find(|line| line.contains("flags =")).unwrap().to_string();
    // the options for a target replace the others for its crates
    assert!(flags(&format!("{host}/libapp.rlib")).ends_with(" -C opt-level=3"), "{}", flags(&format!("{host}/libapp.rlib")));
    assert!(flags("riscv64gc-unknown-none-elf/libapp.rlib").ends_with(" -C debuginfo=1 -C opt-level=2"));
    assert!(flags("custom/libapp.rlib").ends_with(" -C opt-level=s"));
    // while a crate's own replace them
    assert!(flags(&format!("{host}/libown.rlib")).ends_with(" -C opt-level=1"));
    // and those for a target are a namespace, of those which are set, while they are declared when they are used,
    // including before the target is declared
    output.stderr("note: opt-level: 3\n");
    output.stderr(&format!("note: 0 the value of `-C lto` for the crates for `{host}` declared while it is set"));
    let project = Project::new("target-codegen-errors", &[("Jig", "\
set codegen/no-such-target/opt-level 3
set codegen/{host-target}/opt-level [1]
crate app 'app.rs'
    target [host-target, 'riscv64gc-unknown-none-elf']
")]);
    let output = project.configure().failed();
    output.stderr("error[unknown-target]: unknown target \"no-such-target\"\n --> Jig:1:5");
    assert_eq!(output.stderr.matches("error[invalid-operand]").count(), 1, "{}", output.stderr);
}

#[test]
fn default_after_set() {
    // a file used first, such as for local configuration, sets what a later one declares with a default
    let project = Project::new("default-after-set", &[
        ("Jig", "set uart-base 0x2000\nuse 'board'\nlog note '{uart-base} {ram-size} {uart-base.default}'\n"),
        ("board/Jig", "declare uart-base\n    type 'unsigned'\n    default 0x1000\ndeclare ram-size\n    default 0x8000\n"),
    ]);
    project.configure().ok().stderr("note: 8192 32768 4096\n");
    // while the type of a value set before the declaration is checked, and an invalid one leaves the default
    let project = Project::new("default-after-invalid-set", &[
        ("Jig", "set uart-base 'x'\ndeclare uart-base\n    type 'unsigned'\n    default 0x1000\nlog note uart-base\n"),
    ]);
    project.configure().failed().stderr("error[mismatched-type]: ").stderr("note: 4096\n");
}

#[test]
fn use_lists() {
    let project = Project::new("use-lists", &[("Jig", "\
declare drivers
    type 'list'
    default ['uart', 'plic']
crate uart 'uart.rs'
crate plic 'plic.rs'
crate kernel 'kernel.rs'
    use {drivers}
crate other 'other.rs'
    use ['virtio']
    use uart
crate third 'third.rs'
    use [plic]
crate none 'none.rs'
    use {[]}
crate virtio 'virtio.rs'
log note '{kernel.use} | {other.use} | {third.use} | {len(none.use)}'
")]);
    let host = project.host();
    let output = project.configure().ok();
    // the crates of a list are used, which may be chosen by a property, or be references to crates already declared
    output.stderr("note: uart plic | virtio uart | plic | 0\n");
    let externs = |crate_name: &str| output.build(&format!("{host}/lib{crate_name}.rlib")).to_string();
    assert_contains(&externs("kernel"), &format!("--extern plic={host}/libplic.rlib --extern uart={host}/libuart.rlib"));
    assert_contains(&externs("other"), &format!("--extern uart={host}/libuart.rlib --extern virtio={host}/libvirtio.rlib"));
    assert!(!externs("none").contains("--extern"), "{}", externs("none"));
    let project = Project::new("use-list-errors", &[("Jig", "crate app 'app.rs'\n    use [1]\n")]);
    project.configure().failed().stderr("error[invalid-operand]: list value used where list of crate names is required");
}

#[test]
fn cfg_and_env() {
    let jig = |uart: &str| format!("\
crate app 'app.rs'
    crate-type 'bin'
    cfg 'virt'
    cfg [feature: ['uart', 'plic'], board: 'virt']
    env [UART_BASE: {uart}]
    default
log note '{{app.cfg}} | {{app.env}}'
");
    let project = Project::new("cfg-and-env", &[
        ("Jig", &jig("0x1000")),
        ("app.rs", "#[cfg(all(virt, feature = \"uart\", feature = \"plic\", board = \"virt\"))]\nfn main() { println!(\"{}\", env!(\"UART_BASE\")); }\n"),
    ]);
    let host = project.host();
    let output = project.configure().ok();
    // a name, or names with values, as rustc reads them
    output.stderr("note: virt board=\"virt\" feature=\"uart\" feature=\"plic\" | UART_BASE: 4096\n");
    let build = || {
        let output = Command::new(configure_path()).arg("build").current_dir(&project.dir).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        let app = Command::new(project.dir.join(format!("build/{host}/app"))).output().unwrap();
        String::from_utf8(app.stdout).unwrap()
    };
    assert_eq!(build(), "4096\n");
    // which rust-analyzer is given too
    let json = std::fs::read_to_string(project.dir.join(".rust-project.json")).unwrap();
    assert_contains(&json, "\"cfg\": [\"virt\", \"board=\\\"virt\\\"\", \"feature=\\\"uart\\\"\", \"feature=\\\"plic\\\"\"]");
    assert_contains(&json, "\"env\": { \"UART_BASE\": \"4096\" }");
    // and the crate is built again when they change
    std::fs::write(project.dir.join("Jig"), jig("0x2000")).unwrap();
    assert_eq!(build(), "8192\n");
    let project = Project::new("cfg-and-env-errors", &[("Jig", "\
crate app 'app.rs'
    cfg 1
    env [uart-base: 1]
    env 'x'
")]);
    project.configure().failed()
        .stderr("error[invalid-operand]: unsigned value used where str, list of str, or dict is required\n --> Jig:2:9")
        .stderr("error[invalid-identifier]: invalid character '-' in identifier \"uart-base\"\n --> Jig:3:9")
        .stderr("error[invalid-operand]: str value used where dict is required\n --> Jig:4:9");
}

#[test]
fn fatal_diagnostics() {
    let project = Project::new("fatal", &[
        ("Jig", "\
log note 'before'
use 'sub'
log note 'after'
crate app 'app.rs'
    target 'no-such-target'
"),
        ("sub/Jig", "@for x in 1 2 3\nlog fatal 'stop at {x}'\n"),
    ]);
    let output = project.configure().failed();
    // a fatal diagnostic stops the configuration where it is reported, including the loop and the file which used it
    output.stderr("note: before\n").stderr("fatal: stop at 1\n");
    for after in ["stop at 2", "after", "unknown-target"] {
        assert!(!output.stderr.contains(after), "{}", output.stderr);
    }
    // so nothing is generated
    assert!(!project.dir.join("build/build.ninja").exists());
    // and any diagnostic can be made fatal
    let project = Project::new("fatal-override", &[("Jig", "\
set diagnostic/unknown-function 'fatal'
log note nope()
log note 'after'
")]);
    let output = project.configure().failed();
    output.stderr("fatal[unknown-function]: unknown function \"nope\"");
    assert!(!output.stderr.contains("after"), "{}", output.stderr);
}

#[test]
fn defined_function() {
    let project = Project::new("defined", &[("Jig", "\
declare board
declare ram-size
    default 0x1000
set early 1
crate uart 'uart.rs'
log note '{defined(board)} {defined(ram-size)} {defined(early)} {defined(uart)}'
log note '{defined({host-target}/uart)} {defined({host-target}/plic)} {defined(codegen/opt-level)} {defined(codegen/{host-target}/opt-level)}'
set codegen/{host-target}/opt-level 2
log note defined(codegen/{host-target}/opt-level)
@if !defined(board)
log note 'no board'
declare early
")]);
    project.configure().ok()
        // whether a property has a value, where a crate is one, and one which is not declared for a target is not
        .stderr("note: false true true true\n")
        .stderr("note: true false false false\n")
        .stderr("note: true\n")
        .stderr("note: no board\n");
    let project = Project::new("defined-errors", &[("Jig", "log note defined(missing)\nlog note defined(a, b)\n")]);
    project.configure().failed()
        // a property which is not declared, nor set, is misspelt
        .stderr("error[undeclared-property]: property \"missing\" is undeclared\n --> Jig:1:18")
        .stderr("error[argument-count]: function \"defined\" takes 1 argument, but 2 were given");
}

#[test]
fn select_computed_names() {
    let project = Project::new("select-computed", &[("Jig", "\
set codegen/{host-target}/opt-level 2
crate uart 'uart.rs'
declare board
declare fallback
    default 'virt'
log note select(codegen/{host-target}/debuginfo, codegen/{host-target}/opt-level)
log note '{select(board, {host-target}/uart)}'
log note select(board, fall{'back'})
")]);
    let host = project.host();
    // names may be computed, and have a `/`, as for `defined`, such as a codegen option of a target, or a crate
    project.configure().ok()
        .stderr("note: 2\n")
        .stderr(&format!("note: build/{host}/libuart.rlib\n"))
        .stderr("note: virt\n");
    let project = Project::new("select-computed-errors", &[("Jig", "log note select(codegen/{host-target}/nope, missing)\n")]);
    project.configure().failed()
        .stderr("error[undeclared-property]: property \"missing\" is undeclared");
}

#[test]
fn log_location() {
    let project = Project::new("log-location", &[("Jig", "\
declare uart-base
    default 0x1000
set uart-base 0x1001
crate kernel 'kernel.rs'
target custom
    inherit 'x86_64-unknown-none'
test check
    command 'true'
declare unset
log warning 'misaligned'
    location uart-base
log note 'the kernel'
    location kernel
log note 'the target'
    location custom
log note 'the test'
    location check
log note 'not set'
    location unset
@for name in 'custom'
log note 'in a loop'
    location {name}
log note 'quietly'
    location kernel
    quiet
")]);
    let output = project.configure_with(NIGHTLY).ok();
    // where a property was given its value, or declared, or where a crate, target or test was declared
    output
        .stderr("warning: misaligned\n --> Jig:3:5")
        .stderr("note: the kernel\n --> Jig:4:7")
        .stderr("note: the target\n --> Jig:5:8")
        .stderr("note: the test\n --> Jig:7:6")
        .stderr("note: not set\n --> Jig:9:9")
        .stderr("note: in a loop\n --> Jig:5:8")
        // which is not where it was logged, so it is shown without it
        .stderr("note: quietly\n --> Jig:4:7");
    let project = Project::new("log-location-errors", &[("Jig", "\
log note 'a'
    location host-target
log note 'b'
    location nope
log note 'c'
    location ['a']
")]);
    project.configure().failed()
        .stderr("error[no-location]: \"host-target\" is builtin, so it has no location\n --> Jig:2:14")
        .stderr("error[unknown-reference]: no property, custom command, target or test named \"nope\"\n --> Jig:4:14")
        .stderr("error[invalid-operand]: list value used where name is required\n --> Jig:6:14");
}

#[test]
fn inherited_spec_needs_nightly() {
    let project = Project::new("inherited-spec", &[("Jig", "target rv64\n    inherit 'riscv64gc-unknown-none-elf'\n")]);
    let version = Command::new("rustc").arg("-vV").current_dir(&project.dir).output().unwrap();
    if String::from_utf8(version.stdout).unwrap().contains("-nightly") {
        return;
    }
    // the specification of the target it inherits is unstable, so a stable toolchain cannot read it
    project.configure().failed()
        .stderr("custom target \"rv64\" needs a nightly toolchain to read the specification of \"riscv64gc-unknown-none-elf\"")
        .stderr("as rustc reported:\nerror: the option `Z` is only accepted on the nightly compiler");
    project.configure_with(NIGHTLY).ok();
}

#[test]
fn other_jig_warning() {
    let project = Project::new("other-jig", &[("Jig", "crate app 'app.rs'\n"), ("app.rs", "")]);
    project.configure().ok();
    let copy = project.dir.join("other/jig");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    make_executable(&configure_path(), &copy);
    let build = |jig: &std::path::Path| {
        let output = Command::new(jig).arg("build").current_dir(&project.dir).output().unwrap();
        (output.status.success(), String::from_utf8(output.stderr).unwrap())
    };
    // the `jig` which configured the project is not warned about
    let (success, stderr) = build(&configure_path());
    assert!(success && !stderr.contains("other-jig"), "{stderr}");
    // while another is, as the one which configured it regenerates `build.ninja`
    let (success, stderr) = build(&copy);
    assert!(success, "{stderr}");
    assert_contains(&stderr, &format!(
        "warning[other-jig]: the project is configured by {:?}, which regenerates `build.ninja`, rather than the running {:?}",
        configure_path().display().to_string(),
        copy.display().to_string(),
    ));
    // unless it is allowed, or as an error, the build stops
    for (severity, expected) in [("allow", None), ("error", Some("error[other-jig]: "))] {
        std::fs::write(project.dir.join("Jig"), format!("set diagnostic/other-jig '{severity}'\ncrate app 'app.rs'\n")).unwrap();
        project.configure().ok();
        let (success, stderr) = build(&copy);
        match expected {
            None => assert!(success && !stderr.contains("other-jig"), "{stderr}"),
            Some(expected) => {
                assert!(!success);
                assert_contains(&stderr, expected);
                assert_contains(&stderr, "stopped, as `diagnostic/other-jig` makes it an error");
            },
        }
    }
}
