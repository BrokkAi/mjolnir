---
title: Third-party notices
description: Legal material shipped with Mjolnir, the voice worker, dependencies, assets, and Anvil.
---

Mjolnir uses a reviewed, deny-by-default dependency-license policy. Official
release archives include the legal files applicable to that exact artifact.

## Repository material

- [`LICENSE`](https://github.com/BrokkAi/mjolnir/blob/master/LICENSE) — Mjolnir and voice-worker GPLv3 license.
- [`licenses/SOURCE.md`](https://github.com/BrokkAi/mjolnir/blob/master/licenses/SOURCE.md) — source correspondence and build orientation.
- [`licenses/THIRD_PARTY_LICENSES.html`](https://github.com/BrokkAi/mjolnir/blob/master/licenses/THIRD_PARTY_LICENSES.html) — generated Rust dependency report.
- [`licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt`](https://github.com/BrokkAi/mjolnir/blob/master/licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt) — native libraries, embedded fonts, and other reviewed assets.
- `licenses/anvil-vX.Y.Z/` in a release source/archive — the legal bundle matching the Anvil binary shipped in that release.

The generated report covers the locked production workspace graph across native
release targets. Supplemental validation fails when a newly shipped
native-linking crate, standalone Cargo notice, embedded web font, or voice
payload version appears without review.

The release-specific bundle controls. Do not substitute the newest repository
legal directory for an older binary. Contributors changing dependencies or
packaged assets should follow the regeneration and review procedure in
[CONTRIBUTING.md](https://github.com/BrokkAi/mjolnir/blob/master/CONTRIBUTING.md).

See [License and use cases](/license-use-cases/) for practical GPL orientation.
