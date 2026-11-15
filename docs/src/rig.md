# rig as a library

`rig/` is rig, the build engine which builds projects, as [rig's design](rig/design.md) describes. It is a crate which `jig`
uses, and has a `Cargo.toml` so that cargo projects can use it as a git or path dependency. `jig generate` writes rig's
graph to `graph` in the build directory, and rig builds the project, keeping the state of the build in `.rig` in the
build directory.

A graph is built through rig's API, of files and the edges which produce them, each by running a program, without a
shell, which a command which needs one runs itself, as `sh -c`. A project which uses rig builds one, and has rig bring
it up to date:

```rust
use std::path::Path;

fn main() -> Result<(), rig::Error> {
    // `hello.o` is compiled from `hello.c`, with the headers it includes found in the depfile `cc` writes
    let mut graph = rig::Graph::new();
    let (source, object) = (graph.file("hello.c"), graph.file("hello.o"));
    let cc = rig::Exec {
        program: "cc".into(),
        args: ["-c", "hello.c", "-o", "hello.o", "-MD"].map(String::from).to_vec(),
        ..rig::Exec::default()
    };
    graph.add_edge(rig::Edge {
        kind: rig::EdgeKind::Command(cc),
        description: "CC hello.o".into(),
        outputs: vec![object],
        inputs: vec![source],
        depfile: Some("hello.d".into()),
        ..rig::Edge::default()
    })?;
    graph.add_default(&[object]);
    // what ran is recorded in `.rig`, so that the next build runs only what is out of date
    let dir = Path::new(".");
    let mut state = rig::state::State::open(&dir.join(".rig"))?;
    let options = rig::exec::Options::new(dir);
    let mut console = rig::exec::Console::new(std::io::stdout(), false, false, false);
    let report = rig::exec::build(&graph, &mut state, &options, &[], &mut console)?;
    std::process::exit(if report.succeeded() { 0 } else { 1 });
}
```

A graph can be saved with `Graph::save`, and loaded again with `Graph::load`, so that what describes the build need not
run again. rig's tests run the [test helper](https://github.com/AidoP/jig/blob/main/rig/test_helper.rs), their own executable run again, as the program of
their edges, so that they need neither a shell nor coreutils, as can a project's.
