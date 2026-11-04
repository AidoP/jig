#!/bin/sh
# Bootstrap jig with rustc alone: build rig, then jig, into build/bootstrap, then configure the project with it, which
# writes build/build.ninja and rig's graph. From then on, the project is built with build/bootstrap/jig build.
#
# bootstrap.ninja does the same with ninja.
set -eu
cd "$(dirname "$0")"
mkdir -p build/bootstrap build/tmp/rig build/tmp/jig
rustc --color=always --out-dir=build/tmp/rig -O --emit=link=build/bootstrap/librig.rlib --crate-type=lib \
    --crate-name=rig --edition=2024 rig/lib.rs
rustc --color=always --out-dir=build/tmp/jig -O --emit=link=build/bootstrap/jig --crate-type=bin --crate-name=jig \
    --edition=2024 --extern rig=build/bootstrap/librig.rlib src/main.rs
build/bootstrap/jig generate Jig
