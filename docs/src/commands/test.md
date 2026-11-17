# jig test

`jig test [-v] [-j <jobs>] [--timeout <seconds>] [--all] [--tag <tag> ...] [<name> ...] [-- <argument> ...]` builds
what the tests need and runs them, showing the output of those which fail, or of each with `-v`. Without names or tags, it runs the tests without a `tag`, and those
with a tag in the `test-tags` property, which is empty by default, so that slow tests can be left out. Tests can be
named, whatever their tags, or chosen by tag with `--tag`, which can be repeated, and `--all` runs every test. Named
tests run in the order they are named, followed by the others, in the order they were declared. A name or tag which no
test has is an error, as is an option it does not have. Options may follow the names.

Each test which runs here is a command of rig's graph, `test-run/<name>`, which runs always: it runs every time it
is asked for, as a test's result may depend on more than it declares, such as the clock or the network, and is never
cached. So the tests run in the build which builds what they need, in its jobs, each as soon as what it needs is
built, and the build shows each running as `TEST <name>`. What a test prints is kept rather than shown by the build,
and the results are shown once the build is done, in the order the tests were chosen, whatever order they ran in.
A test whose inputs fail to build isn't run, and fails. The tests a [worker](../workers.md) runs are sent once what
they need is built.

- `-j <jobs>` builds and tests with as many jobs, which are as many as the machine runs at once, and two more, by
  default. A test which must not run beside others, as it times what it runs, or writes files others read, is
  `serial`, and runs with nothing else running.
- `-k <failures>` and `-q` are those of [jig build](build.md), while `-v` shows the output of the tests rather than the
  commands of the build. The build goes on after a failure, so that every test which can run does.
- `--timeout <seconds>` replaces the timeout of every test which runs here, which may be a fraction, such as `0.5`.
- The arguments after `--` are given to each test which runs, after its own, such as a filter for the tests of a
  crate's test harness, as in `jig test app -- parser::`.

A test's `timeout`, or the `test-timeout` property where it is declared, is how long it may run before it is killed,
with every process it started, as each test runs in a process group of its own, or a job object on Windows. It then
fails, showing `timed out after <seconds>s` before what it printed until then. A test has no timeout unless one is
given. A test runs with the variables of its `env`, beside those `jig test` inherits, and the directories of the
project's [shared libraries](../language/shared-libraries.md) before the platform's search path of them.

A test runs while rig holds the build's state, so a test which builds the same project with jig fails to, while one
which builds another project, in a directory of its own, can.

A crate's tests are a test named after it, with its `tag`, `timeout` and `serial`, and a `test` statement declares
one of a command:

```jig
set test-timeout 60

crate app 'app.rs'
    crate-type 'bin'
    test
    tag 'slow'

test smoke
    command app '--version'
    tag 'quick'
    timeout 5

test benchmark
    command app '--bench'
    serial
```

```
./build/bootstrap/jig test
./build/bootstrap/jig test --tag quick
./build/bootstrap/jig test -v app smoke
./build/bootstrap/jig test --all
./build/bootstrap/jig test -j 4 --timeout 60
./build/bootstrap/jig test app -- parser::
```
