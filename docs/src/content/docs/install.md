---
title: Install and run
description: Install the mj binary and start a Council session.
---

## Install the latest release

```bash
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

Desktop users can install Mjolnir and its voice worker from crates.io:

```bash
cargo install --locked brokk-mjolnir brokk-mj-voice-worker
```

## Start in a repository

```bash
cd your-project
mj
```

Mjolnir discovers supported local ACP routes and credentials automatically. Configuration and platform-specific installation notes will be expanded as the docs develop.
