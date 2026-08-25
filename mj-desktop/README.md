# brokk-mj-desktop

`brokk-mj-desktop` is Mjolnir's native desktop shell for its remote viewer. It
provides the platform WebView and certificate-verification boundary used by the
default `mj app` desktop application on supported desktop platforms.

This crate is primarily published so the Mjolnir workspace components can be
versioned and distributed together. Its Rust crate name is `mj_desktop`, and
its API currently follows Mjolnir's release cycle rather than a separate
stability guarantee.

For installation and user documentation, see the
[Mjolnir repository](https://github.com/BrokkAi/mjolnir).

License: GPL-3.0-only.
