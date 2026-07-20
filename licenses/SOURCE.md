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

Official archives also contain an `anvil-licenses` directory generated from the
locked graph of the pinned Anvil release. Its `SOURCE.md` identifies the source
for that separately built and aggregated binary.
