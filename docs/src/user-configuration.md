# The user's configuration

What belongs to the user or the machine rather than a project, such as rig's cache, is set in the user's configuration,
`$XDG_CONFIG_HOME/jig/config`, or on Windows, `%APPDATA%\jig\config`, or `~/.config/jig/config`, or the file the
`JIG_CONFIG` environment variable gives, while a `JIG_CONFIG` which is empty or `off` turns it off. It is a file of the
same statements as `Jig`, which is processed before a project's own, as if the project began with a `use` of it, so that
a project can read what it sets, and set it again. Paths in it are relative to it, as in any file. A project depends on
it, so it is configured again when it changes, or once it is created. `jig cache` and `jig serve`, which have no
project, process it alone, where the builtin properties of the toolchain are empty if rustc cannot be run.

```jig
# ~/.config/jig/config
set cache/remote 'ssh://cache.example'
set cache/size '20G'
@if try(env('CI'), '') == 'true'
set cache/push true
```
