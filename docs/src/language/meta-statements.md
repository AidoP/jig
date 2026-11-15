# Meta-statements

A meta-statement is a line starting with `@` which applies to the statement on the next line. There cannot be a blank
line or a comment between them, but several meta-statements can apply to the same statement.

- `@if <expression>`: skip the statement, including its attributes, unless the expression is `true`. The expression
  must be a `bool`, or coerced to one with `{}`. Once a statement is skipped, the conditions of later meta-statements
  are not evaluated

<!-- jig-test
files: dtb/Jig
declare device-tree
    default 'true'
-->
```jig
@if {device-tree}
@if target == 'riscv64gc-unknown-none-elf'
use 'dtb'
```

- `@else`: process the statement only if no statement has been processed in the chain of conditional statements before
  it. A chain starts with a statement with `@if` and is continued by each statement with `@else`, so `@else` followed
  by `@if` is an else-if. Blank lines and comments may separate the statements of a chain, but another statement ends
  it. `@else` must be the first meta-statement of its statement
- `@else-if <expression>`: the same as `@else` followed by `@if <expression>`

<!-- jig-test
declare level
    default 2
-->
```jig
@if level == 1
log note 'one'
@else-if level == 2
log note 'two'
@else
log note 'many'
```

- `@for <name> in <value> ...`: process the meta-statements and statement after it once for each of the values, which
  are expressions separated by spaces, where each item of a list is a value. For each, the property `<name>` has the
  value, within the file and until the end of the statement. Without any values, the statement is skipped

<!-- jig-test
declare targets
    default [host-target]
-->
```jig
@for target in targets
@if target != host-target
crate kernel 'kernel.rs'
    target target
```

- `@then`: chain the statement after it to the statement before it, which it must be directly after, so that the
  statements are processed or skipped together, and repeated together by `@for`. The chained statement can have its
  own meta-statements, which apply to it and the rest of the chain

<!-- jig-test
declare total
    default 0
-->
```jig
@for a in 1 2
set total (total + a)
@then
log note 'added {a}, the total is {total}'
```

- `@with <name> as <value>`: process the meta-statements and statement after it, and the statements chained to it with
  `@then`, with the property `<name>` set to the value, as by `set`, then restore its previous value, unless they set
  it. Unlike `@for`, the property is set everywhere, including in files used by the statements. Unlike `set`, the
  property needn't be declared, as its value is only for the statements, while one which is declared later checks the
  value. Conditions after it continue the chain of those before it, or start one

```jig
@with target as 'riscv64gc-unknown-none-elf'
crate kernel 'kernel.rs'
@then
crate boot 'boot.rs'
```

A property which is only for a file's statements, such as a board a file of its own describes, needs no declaration:

<!-- jig-test
files: boards/Jig
-->
```jig
@with board as 'virt'
use 'boards'
```
