# Summary

[Introduction](introduction.md)

# Using jig

- [Installing](installing.md)
- [Commands](commands.md)
  - [jig build and jig generate](commands/build.md)
  - [jig run](commands/run.md)
  - [jig test](commands/test.md)
  - [jig lint](commands/lint.md)
  - [jig check and rust-analyzer](commands/check.md)
  - [jig install](commands/install.md)
  - [jig graph](commands/graph.md)
- [Toolchains](toolchains.md)
- [Files jig keeps](files.md)
- [When a project is configured again](reconfiguring.md)
- [Caching](caching.md)
- [Workers](workers.md)
- [The user's configuration](user-configuration.md)
- [Windows](windows.md)

# The configuration language

- [Overview](language.md)
  - [Statements](language/statements.md)
  - [Meta-statements](language/meta-statements.md)
  - [Expressions](language/expressions.md)
  - [Functions](language/functions.md)
  - [Properties](language/properties.md)
  - [Crates](language/crates.md)
  - [Codegen options and profiles](language/codegen.md)
  - [Custom commands](language/custom-commands.md)
  - [Generated files](language/files.md)
  - [Targets](language/targets.md)
  - [Builtin properties](language/builtin-properties.md)
  - [Diagnostics](language/diagnostics.md)

# rig

- [rig as a library](rig.md)
- [rig's design](rig/design.md)
  - [The graph](rig/graph.md)
  - [The state, and how changes are found](rig/changes.md)
  - [Environment variables](rig/environment.md)
  - [Running a build](rig/execution.md)
  - [The cache](rig/cache.md)
  - [Workers](rig/workers.md)
  - [Why rig is as it is](rig/rationale.md)

# Developing jig

- [Developing jig](developing.md)
- [Plans](plans.md)
  - [C and assembly](plans/c-and-assembly.md)
