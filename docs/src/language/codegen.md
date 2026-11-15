# Codegen options and profiles

The stable codegen options of rustc are builtin properties named `codegen/<option>`, such as `codegen/opt-level`, which
have no value until they are set. A crate is compiled, and checked, with `-C <option>=<value>` for each of them which is
set where it is declared, as with the `target` property, unless the crate has an attribute for the option,
`codegen/<option> <value>`, which replaces it. A value is converted to a string as in a string template. For
`target-feature` and `remark`, a list is joined by `,`, for `llvm-args` and `passes` by spaces, and for `link-arg` each
item is a separate option. A path, such as for `linker`, is relative to the project root, unless it is a reference.
`codegen/` is a dict of the options which are set, by name, as a crate declared there would be given them, such as
`[opt-level: 's']`, as described for [namespaces](statements.md).

So that the options can be set for the whole project, as with cargo, some are not given to every crate:

- A procedural macro is not given `codegen/lto`, which rustc refuses for one, or `codegen/panic`, as it must unwind,
  unless it has the attribute itself.
- A procedural macro is for the host, where rustc loads it, so it is not given the options which describe the machine
  the crates are for, or how they are linked for it, by `codegen/<option>`: `code-model`, `default-linker-libraries`,
  `link-arg`, `link-self-contained`, `linker`, `linker-flavor`, `no-redzone`, `relocation-model`, `target-cpu` and
  `target-feature`. It is given them by `codegen/<host>/<option>`, where the host is `host-target`, or its own
  attribute. So one of them can be set for a whole project for a target without an operating system, such as
  `codegen/relocation-model 'static'`, which a procedural macro could not be linked with.
- A test harness is never given `panic`, as it must unwind on stable rustc, to catch each test's panic, nor are the
  checks and lints of a crate with tests, which are of its harness.
- A library built with `panic=abort` cannot be linked into what unwinds, a test harness or a procedural macro, nor can
  a library which uses one, as `panic` changes the hash of a crate. So each of those libraries which a test harness or
  a procedural macro links is built again without `panic`, as `<target>/unwind/lib<name>.rlib`, which they link
  instead, as cargo does, while binaries link the libraries which abort. Nothing is built again without `panic=abort`.

Each target has a property for each option too, `codegen/<target>/<option>`, such as
`codegen/x86_64-unknown-linux-gnu/opt-level`, which replaces `codegen/<option>` for the crates for that target, while a
crate's own attribute replaces both. They are declared when they are first used, for a builtin target or one declared
by a `target` statement, which may come after they are set. `codegen/<target>/` is a dict of those which are set for
the target, which are also in `codegen/`, by `<target>/<option>`.

```
set codegen/opt-level 's'
set codegen/debug-assertions true
set codegen/{host-target}/opt-level 3
crate kernel 'kernel.rs'
crate boot 'boot.rs'
    codegen/opt-level 'z'
```

The builtin profiles set the codegen options which are usually wanted together, as cargo's profiles do:

- `profile/release`: optimised aggressively, with `opt-level` 3, fat LTO, as `lto` `'fat'`, a single codegen unit, as
  `codegen-units` 1, `strip` `'symbols'`, which takes the symbols and debug information from what is linked, and
  `panic` `'abort'`, with `debuginfo` 0, and `debug-assertions` and `overflow-checks` off. It builds slowly, as each
  binary is optimised with all of the crates it links, in one codegen unit, and the libraries which tests link are built
  again to unwind, as described for codegen options
- `profile/debug`: `opt-level` 0, `debuginfo` 2, `debug-assertions` and `overflow-checks` on, and `lto` `'off'`,
  `codegen-units` 256, `strip` `'none'` and `panic` `'unwind'`, so that it undoes the release profile

```
use profile/release
set codegen/debuginfo 1
crate app 'main.rs'
    crate-type 'bin'
```
