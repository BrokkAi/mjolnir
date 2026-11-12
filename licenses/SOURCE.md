# Corresponding Source

Each official Mjolnir artifact identifies its version as `X.Y.Z`. The complete
corresponding source for that artifact, including the build and release scripts,
is the Git tag `vX.Y.Z` in the Mjolnir repository:

https://github.com/BrokkAi/mjolnir/releases/tag/vX.Y.Z

Replace `X.Y.Z` with the version printed by `mj --version`. The release page
provides source archives for that exact tag. The repository history is also
available at:

https://github.com/BrokkAi/mjolnir

Mjolnir and its voice worker are licensed under `GPL-3.0-only`; `LICENSE`
contains the GNU GPL version 3 text. `THIRD_PARTY_LICENSES.html` covers the
locked Rust graph. `SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt` and the other files in
this directory cover native libraries, embedded fonts, and standalone notices
not fully represented by Cargo metadata.

The HTML and supplemental reports are generated during GitHub release packaging
and included alongside this file in release archives, not stored in the source
checkout. The license-policy configuration and audited upstream texts remain
source inputs.

The local GitHub relevance model's native ONNX Runtime is downloaded from the
official pinned upstream artifacts rather than bundled with the application.
The verified runtime cache contains its upstream license and third-party notice
texts; Windows also retains the DirectML redistribution licenses beside its DLL.
The source URLs, versions and checksums are in
`mj-controller/src/github_student/runtime.json`.
