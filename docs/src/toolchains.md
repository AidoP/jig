# Toolchains

jig runs rustc, rustdoc and clippy as `rustc`, `rustdoc` and `clippy-driver`, which are rustup's proxies where rustup
is installed, so the toolchain is the one rustup chooses: by `RUSTUP_TOOLCHAIN`, a `rust-toolchain.toml` in the project
or a directory above it, an override for the directory, or rustup's default.

A rustup toolchain is chosen instead with `+<toolchain>` before the command, as with cargo, such as
`jig +nightly build`. Without rustup, a toolchain cannot be chosen this way.

```
./build/bootstrap/jig +nightly build
```

## The toolchain stays with the project

The project is configured with the toolchain it is given, which is recorded in `.jig`. Every later command uses it,
for rustc, the custom commands, the tests and `jig run`, whether or not it is given again, until the project is
configured with another:

- a command given another toolchain configures the project again with it;
- `jig +<toolchain> generate` configures it with the toolchain;
- `jig generate` without one configures it without one, leaving the toolchain to rustup again.

So after `jig +nightly lint`, `jig build` builds with nightly too. To go back to rustup's choice, configure the project
without a toolchain:

```
./build/bootstrap/jig +1.97 lint -D
./build/bootstrap/jig generate
```

A toolchain changes what a project is configured with, such as the targets which have standard libraries, which
`has-std` finds. A configuration which builds only for the targets which have them, as jig's own does, leaves out what
is built for a target while it is configured with a toolchain which lacks its standard library.

The builtin property `toolchain` is the toolchain given, or empty, and `rustc/version`, `rustc/sysroot`, `rustc/targets`
and `host-target` are what the toolchain reports.
