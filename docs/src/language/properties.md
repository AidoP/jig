# Properties

A `declare` statement may be followed by indented attributes:

- `type <type>`: the type of the property, one of `'str'`, `'bool'`, `'unsigned'`, `'signed'`, `'list'`, `'dict'`
  or `'ref'`
- `default <value>`: the initial value of the property, unless it is set before the declaration, such as by a file
  used first for local configuration, which replaces the default
- `info <string>`: a description of the property, included in diagnostics about it
- `read-only`: the property can only be assigned once, by a single `set` before the declaration, which replaces its
  default, or otherwise by its default or a single `set` after the declaration

```
declare uart-base
    type 'unsigned'
    default 0x1000_0000
    info 'the base address of the UART'

declare board
    type 'str'
    default 'virt'
    read-only

log note 'UART at {uart-base} on {board}'
```
