---
title: License and use cases
description: Practical orientation for running, hosting, distributing, or modifying GPL-licensed Mjolnir.
---

Mjolnir and the voice worker are licensed under
[`GPL-3.0-only`](https://github.com/BrokkAi/mjolnir/blob/master/LICENSE). You may
run them for personal, research, internal, or commercial work. The obligations
change mainly when you give someone else a copy or create a combined work.

This page is practical orientation, not legal advice. The license text controls.
Third-party components and the bundled Anvil runtime have their own notices and
legal files.

## Start with what you do

| Use | Practical orientation |
| --- | --- |
| Run unmodified Mjolnir | GPLv3 places no conditions on merely running the program. Ordinary prompts, source files, diffs, and model output are not automatically GPL-covered. |
| Modify it privately or use it inside one organization | Private changes do not need to be published merely because you run them internally. |
| Operate a hosted service | GPLv3 has no AGPL network-use clause, so network interaction alone does not require publication of a private server-side fork. |
| Invoke `mj` from a separate program | A program communicating through ordinary process/protocol boundaries can normally keep its own license, but the combined-work analysis is factual. |
| Distribute an archive, image, appliance, or installer containing Mjolnir | Recipients must receive the GPL notices and complete corresponding source for the exact covered version, including your changes and required build/install scripts. |
| Link, embed, or copy Mjolnir implementation code | Treat this as a combined- or derivative-work question and obtain legal review before distributing it under different terms. |
| Distribute a modified fork | License the covered work under GPLv3, mark changes, preserve notices, and provide complete corresponding source to recipients. |

A hosted service is different from delivering an on-premise container, VM,
desktop bundle, or appliance. Those artifacts convey a copy.

## Common users

### Individual or researcher

You may install Mjolnir, run it on public or private repositories, benchmark
Councils, and keep experimental modifications private. Provider terms,
repository confidentiality, and rights in generated output remain separate
questions.

### Company using Mjolnir internally

Internal use does not by itself require public release of private changes.
Control which provider receives company source and whether contractors or other
legal entities receive copies of the software.

### Hosted coding service

Operating a private modified copy on your own servers is not automatically a
distribution event under GPLv3. Shipping the same server as a customer
container or appliance is different.

### Distributor or bundled developer environment

If your installer or image includes Mjolnir, include the license and notices,
identify the exact version, and provide the complete corresponding source in a
GPL-compliant way. Do not add restrictions that deny recipients their GPL
rights.

## Official release legal material

Release archives include:

- the GPL license;
- [`licenses/SOURCE.md`](https://github.com/BrokkAi/mjolnir/blob/master/licenses/SOURCE.md);
- the generated Rust dependency report;
- supplemental notices for native libraries and embedded fonts; and
- the legal bundle matching the Anvil binary shipped by that release.

The exact bundled Anvil version is release-specific. Use the legal directory in
the archive you received rather than assuming the repository's newest managed
runtime is the one in an older package.

## Distribution checklist

1. Record the exact Mjolnir, voice-worker, and bundled-runtime versions.
2. Preserve copyright and license notices and mark your modifications.
3. Give recipients the GPLv3 text and complete corresponding source.
4. Include the build and installation scripts needed for that exact binary.
5. Review dependency, native-library, font, and bundled-runtime notices.
6. Do not impose extra restrictions on modification, redistribution, or reverse engineering needed to exercise GPL rights.

Read [Third-party notices](/third-party-notices/) for artifact coverage. Seek
qualified advice when linking Mjolnir into another product, transferring copies
across company boundaries, or distributing an appliance or locked-down device.
