# brokk-mj-desktop

`brokk-mj-desktop` is Mjolnir's native desktop application for its remote
viewer. It installs the `mj-desktop` executable and provides the platform
WebView and certificate-verification library used by that executable. `mj app`
launches it as a sibling process, so the main `mj` executable never links native
desktop libraries.

`mj-desktop` may also be launched directly when it is installed beside `mj`.
It asks `mj` to start or connect to the controller daemon, then opens the
authenticated viewer. Linux builds use the system WebKitGTK runtime and are
supported on GNU/Linux rather than static musl targets.

The executable and library are versioned and distributed with Mjolnir. The
Rust crate name is `mj_desktop`, and its API currently follows Mjolnir's release
cycle rather than a separate stability guarantee.

For installation and user documentation, see the
[Mjolnir repository](https://github.com/BrokkAi/mjolnir).

License: GPL-3.0-only.
