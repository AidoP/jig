# Builtin properties

- `rustc/sysroot`: the sysroot of the Rust toolchain, as printed by `rustc --print=sysroot`, which is read-only
- `rustc/targets`: the builtin targets of rustc, as a list of their names, as printed by `rustc --print=target-list`,
  which is read-only. It does not include targets declared by `target` statements
- `rustc/version`: the version of rustc, from the `release` line of `rustc -vV`, such as `1.95.0`, or `1.96.0-nightly`
  for a nightly toolchain, which is read-only. As it is semantic, it can be compared with `semver-compatible`, where a
  nightly version is a pre-release, unless it is given `ignore-pre-release: true`
- `build-dir`: the directory of what is built, relative to the project root, `build` by default. It is read-only
  once it is used, such as by a crate or the outputs of a custom command, so it is set before them. When it changes,
  `jig build`, `jig check`, `jig test` and `jig install` regenerate the graph in the new build directory, and build
  there. The previous build directory is kept
- `target`: the target triple for crates without a `target` attribute, initially the host
- `install-prefix`: the directory `jig install` installs in when it is not given one, relative to the project root,
  `/usr/local` by default, or on Windows, where a user's own programs go, `%LOCALAPPDATA%\Programs`, which needs no
  administrator
- `test-tags`: the tags of the tests `jig test` runs without names or tags, beside the tests without tags, as a list,
  which is empty by default
- `test-timeout`: the timeout of the tests declared while it is set, in seconds, such as `60` or `'0.5'`, which a
  test's own `timeout` replaces, with no value until it is set, so that a test has no timeout. Any other value is the
  error `invalid-operand` where it is set
- `change-detection`: how rig finds that a file has changed, `'timestamp'` by default, by its modification time and
  size, or `'hash'`, by a BLAKE3 hash of its contents too. With `'hash'`, a file which is written again unchanged, or
  changed and changed back, rebuilds nothing, and what depends on an output which is written unchanged is not rebuilt.
  A file is only read again when its time or size has changed, or when it is a source which was modified within a tick
  of the file system's clock of being hashed, as it may have changed again without its time showing it. Any other
  value is the error `invalid-operand` where it is set. A crate or custom command's own `change-detection` attribute
  replaces it, and the statements of C hash their files unless theirs says otherwise
- `cache/dir`, `cache/size`, `cache/remote` and `cache/push`: rig's cache, as [caching](../caching.md) describes, usually set in the user's
  configuration: its directory, `$XDG_CACHE_HOME/jig`, `%LOCALAPPDATA%\jig\cache` on Windows, or
  `~/.cache/jig` by default, or `'off'`; the size it is
  collected down to, `'10G'` by default; the remote cache it is in front of, empty by default; and whether it pushes
  to it, `false` by default. Each is overridden by an environment variable: `JIG_CACHE_DIR`, `JIG_CACHE_SIZE`,
  `JIG_CACHE_REMOTE` and `JIG_CACHE_PUSH`
- `cache/token-file`: the file of the token which proves rig to a remote cache which needs one, as one reached over
  TCP does, and to workers reached over TCP, empty by default, which `JIG_CACHE_TOKEN_FILE` overrides
- `host-target`: the target triple of the host, as printed by `rustc --print=host-tuple`, which is read-only
- `msrv` and `profile/<name>`: the builtin files, which `use` processes, as described for [`use`](statements.md), which are read-only
- `rust-version`: the minimum supported Rust version, such as `1.95.0`, which `use msrv` requires of rustc, and which
  `jig lint` gives clippy, as cargo does, so that it suggests only what that version has, with no value until it is set
- `toolchain`: the rustup toolchain given with `+<toolchain>`, or empty if none was given, which is read-only
- `workers/list`: the workers which cached commands run on once a build's own jobs are full, as
  [workers](../workers.md) describes, empty by default, which `JIG_WORKERS_LIST` overrides
- `workers/only`: whether what a worker may build waits for one, rather than being built here, as described for
  [workers](../workers.md), `false` by default, which `JIG_WORKERS_ONLY=1` overrides
- `workers/download`: what is fetched of what workers build, `'all'`, as by default, or `'targets'`, as described for
  [workers](../workers.md), which `JIG_WORKERS_DOWNLOAD` overrides
- `codegen/<option>`: a codegen option of the crates declared while it is set, as described for [codegen options](codegen.md), while
  `codegen/` is a dict of those which are set
- `codegen/<target>/<option>`: a codegen option of the crates for a target declared while it is set, which replaces
  `codegen/<option>`
- `c/compiler`, `c/archiver`, `c/flags`, `asm/flags` and `c/link-flags`: the C compiler, the archiver, and the
  arguments of the compiler for C, for assembly and for linking, of the [static libraries](static-libraries.md) and
  [executables](executables.md) declared while they are set, which are `CC`, `AR`, `CFLAGS`, `ASFLAGS` and `LDFLAGS`
  by default, and `cc` and `ar` for the host target without them
- `cxx/compiler`, `cxx/flags` and `cxx/runtime`: the C++ compiler, its arguments for C++, which are `CXX` and
  `CXXFLAGS` by default, and `c++` for the host target without it, and the library of the C++ runtime, as
  [static libraries](static-libraries.md#compilers-and-flags) describes
- `c/<target>/<name>`, `cxx/<target>/<name>` and `asm/<target>/<name>`: those of the static libraries for a target,
  which replace the others
- `c/tidy`: the checks of clang-tidy, as its `--checks` takes them, such as `'-*,bugprone-*'`, which
  [jig lint](../commands/lint.md) runs on the sources of C and C++ of the libraries and executables declared while it is
  set. It is empty by default, so they are not checked
- `clippy/<lint>`: the level of a clippy lint for the crates declared while it is set, which `jig lint` lints them with:
  `'allow'`, `'warn'`, `'force-warn'`, `'deny'` or `'forbid'`, such as `set clippy/needless_return 'deny'`, where
  `-` can be written for `_`. A group, such as `clippy/pedantic`, is given before the single lints, so that a lint in
  it can be given another level. As clippy has many lints, each is declared when it is first used, while `clippy/` is
  a dict of those which are set
- `diagnostic/<id>`: the severity of a diagnostic, as [diagnostics](diagnostics.md) describes

The variable which overrides a setting of the cache or the workers is named after it, as `JIG_` and its name in
capitals, with `_` for `/` and `-`. A size, a remote cache, a worker, or what is fetched of what workers build, which
is not one is the error `invalid-setting` where it is set, unless its variable overrides it.

```jig
set build-dir 'out'
set test-tags ['slow']

log note 'rustc {rustc/version} for {host-target}, building in {build-dir}'
@if !contains(rustc/targets, 'riscv64gc-unknown-none-elf')
log warning 'this rustc cannot build for RISC-V'
```
