# Installing

Each release on GitHub has a statically linked `jig` for x86_64, aarch64 and riscv64 Linux, and a `jig` for macOS on
Apple silicon, which are built and tested by `.github/workflows/release.yml` when a tag starting with `v` is pushed. A
tag with a suffix, such as `v1.0.0-rc.1`, is a prerelease.

## From source

Building `jig` needs Rust 1.97 or later.

The build system needs to be bootstrapped, with `bootstrap.sh`, which builds `jig` with rustc alone, and configures the
project.

```
sh bootstrap.sh
```

Then run the bootstrapped `jig` to install.

```
build/bootstrap/jig install [prefix]
```

This builds `build/bootstrap/jig` and configures the project. From then on, the project is rebuilt with `jig build`, and
tested with `jig test`.

```
./build/bootstrap/jig build
./build/bootstrap/jig test
```
