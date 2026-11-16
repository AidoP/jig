# jig test

`jig test [-v] [-j <jobs>] [--timeout <seconds>] [--all] [--tag <tag> ...] [<name> ...] [-- <argument> ...]` builds
what the tests need and runs them, showing the output of those which fail, or of each with `-v`. Without names or tags, it runs the tests without a `tag`, and those
with a tag in the `test-tags` property, which is empty by default, so that slow tests can be left out. Tests can be
named, whatever their tags, or chosen by tag with `--tag`, which can be repeated, and `--all` runs every test. Named
tests run in the order they are named, followed by the others, in the order they were declared. A name or tag which no
test has is an error, as is an option it does not have. Options may follow the names.

- `-j <jobs>` builds what the tests need with as many jobs, and runs as many tests at once, rather than one at a
  time. The results are shown in the same order
  whatever the number of jobs, each once its test has finished. Tests which write the same files, or which time
  what they run, should not run at once, so one job is the default. The tests a [worker](../workers.md) runs are
  sent at once regardless.
- `-k <failures>` and `-q` are those of [jig build](build.md), for building what the tests need, while `-v` shows the
  output of the tests rather than the commands of the build.
- `--timeout <seconds>` kills a test which runs here for longer, which may be a fraction, such as `0.5`, and fails
  it, showing `timed out after <seconds>s` before what it printed until then. Only the test's own process is killed,
  not what it started.
- The arguments after `--` are given to each test which runs, after its own, such as a filter for the tests of a
  crate's test harness, as in `jig test app -- parser::`.

A crate's tests are a test named after it, with its `tag`, and a `test` statement declares one of a command:

```jig
crate app 'app.rs'
    crate-type 'bin'
    test
    tag 'slow'

test smoke
    command app '--version'
    tag 'quick'
```

```
./build/bootstrap/jig test
./build/bootstrap/jig test --tag quick
./build/bootstrap/jig test -v app smoke
./build/bootstrap/jig test --all
./build/bootstrap/jig test -j 4 --timeout 60
./build/bootstrap/jig test app -- parser::
```
