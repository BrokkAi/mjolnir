# brokk-mj-core

`brokk-mj-core` is Mjolnir's frontend-neutral Agent Client Protocol runtime and
session kernel. It contains ACP transport, configuration, session persistence,
provider integration, orchestration primitives, and other shared runtime code.

This crate is primarily published so the Mjolnir workspace components can be
versioned and distributed together. Its Rust crate name is `mj_core`, and its
API currently follows Mjolnir's release cycle rather than a separate stability
guarantee.

For installation and user documentation, see the
[Mjolnir repository](https://github.com/BrokkAi/mjolnir).

License: GPL-3.0-only.
