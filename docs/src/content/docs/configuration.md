---
title: Configuration
description: Configure Council roles, ACP servers, review policy, and appearance.
---

Mjolnir stores user configuration in `~/.config/mj/config.toml`. The default Council selects models automatically:

```toml
version = 2

[thor]
model = "auto"
discrete_review = true

[loki]
model = "auto"

[eitri]
model = "auto"
```

Run `/mjconfig` to edit Council models, accounts, ACP servers, review policy, and appearance from the TUI. A complete schema reference will be added here.
