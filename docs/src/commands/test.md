# jig test

`jig test [-v] [--all] [--tag <tag> ...] [<name> ...]` builds what the tests need and runs them in turn, showing the
output of those which fail, or of each with `-v`. Without names or tags, it runs the tests without a `tag`, and those
with a tag in the `test-tags` property, which is empty by default, so that slow tests can be left out. Tests can be
named, whatever their tags, or chosen by tag with `--tag`, which can be repeated, and `--all` runs every test. Named
tests run in the order they are named, followed by the others, in the order they were declared. A name or tag which no
test has is an error.

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
```
