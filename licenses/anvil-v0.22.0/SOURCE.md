# Anvil v0.22.0 Corresponding Source

Mjolnir release archives bundle the separately built Anvil `v0.22.0` binary.
Its complete corresponding source, including build and release scripts, is the
tag at:

https://github.com/BrokkAi/anvil/releases/tag/v0.22.0

The exact source commit is:

`f32f42259dda6a623edda2dce625292f8635d0ac`

Anvil's v0.22.0 manifest declares `LGPL-3.0-only`. The tag accidentally shipped
the GNU GPL version 3 text as its root `LICENSE`; that exact file is preserved
here as `LICENSE`. This bundle adds the missing LGPL text as `LGPL-3.0.md`, and
`GPL-3.0.md` provides an explicitly named copy of the incorporated GPL text.
`THIRD_PARTY_LICENSES.html` was generated from the tag's locked production Rust
graph across all native release targets and its embedded `wasm32-wasip2`
sandbox guest. `SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt` covers standalone notices
and bundled native code not fully represented by Cargo metadata.

The tag used the deprecated SPDX spelling `LGPL-3.0`; the generated report
normalizes that project metadata to `LGPL-3.0-only`, matching Anvil's corrected
canonical license file and manifest metadata adopted after this release.
