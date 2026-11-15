# Toolchains

A rustup toolchain is selected with `+<toolchain>` before the command, as with cargo, such as `jig +nightly build`. The
project is configured with it, so that rustc, the custom commands, the tests and `run` use it, until the project is
configured with another. `jig +<toolchain> generate` configures it with the
toolchain, and `jig generate` without one leaves the toolchain to rustup, while the other commands configure it again
if they are given a different toolchain. Without rustup, a toolchain cannot be selected.

```
./build/bootstrap/jig +nightly build
```
