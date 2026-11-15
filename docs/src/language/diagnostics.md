# Diagnostics

Each diagnostic has an identifier, shown after its severity, as in `error[unknown-target]`, except for those of `log`
statements without an `id` attribute, whose identifier is `user`. Its severity is replaced by the value of its
property, `diagnostic/<id>`, where it is reported: `'fatal'`, `'error'`, `'warning'`, `'note'`, or `'allow'` to hide
it. A note or suggestion attached to a diagnostic, such as the list of targets after an unknown target, is hidden with
it. Errors within the first argument of `try` are still reported as warnings, except for `impure`. Diagnostics found
once the files are read, such as unknown targets of crates, have the severities set at the end.

A fatal error stops the configuration once the statement which reported it is done, so the rest of its file, the
files which used it and any repetitions of a `@for` are not processed, nor is the configuration checked or generated.
It suits a configuration which cannot go on, such as for an unsupported device, as later diagnostics would only follow
from it.

```
set diagnostic/unknown-target 'warning'
set diagnostic/user 'allow'
```

Hiding or lowering an error does not make the configuration valid, so what is generated may not build.
