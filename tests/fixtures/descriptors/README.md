# Synthetic descriptor corpus

These fixtures exercise descriptor parsing and targeted editing without material copied from an
engine installation or third-party plugin.

`cases.toml` records byte format and expected declared state. Files ending in `.hex` contain one
lowercase hexadecimal byte sequence. Test code must decode that sequence before parsing it.
Hexadecimal storage protects byte-order marks and CRLF line endings from Git conversion.

Keep every name and description invented. Add the smallest case that isolates the behavior under
test.
