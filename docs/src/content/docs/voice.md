---
title: Voice dictation
description: Dictate prompts locally in Mjolnir and understand the supported platforms and data boundary.
---

Mjolnir can transcribe a spoken prompt locally and insert the result into the
composer. Press **Ctrl-R** to start dictation and press it again to stop. Review
or edit the transcript before sending it to Codex.

## Platform support

| Platform | Support | Installation note |
| --- | --- | --- |
| macOS | Supported | The release installer includes `mj-voice-worker`; Cargo users install it separately |
| Linux | Supported | The release installer includes `mj-voice-worker`; Cargo users install it separately |
| Windows | Supported | Use a release archive containing the worker or install both Cargo crates |
| Android | Not currently available | Android builds omit the desktop voice worker and hide the Ctrl-R shortcut |

For a Cargo installation:

```bash
cargo install --locked brokk-mjolnir brokk-mj-voice-worker
```

The worker must be installed beside `mj`. Developers can point to a separate
binary with `MJ_VOICE_WORKER`.

## What stays local

The voice worker records microphone audio only after Ctrl-R activation. It runs
the speech-recognition engine in a separate local process, returns transcribed
text to Mjolnir, and does not send the audio to Codex. Only the prompt text you
choose to submit enters the agent conversation.

The recognition model is downloaded on first use and currently requires about
0.7 GB of cached assets. The sidecar process keeps a native speech-engine crash
from taking down the active Mjolnir session.

Voice input changes how a prompt enters the composer; it does not bypass normal
permissions, workspace limits, review, or provider data boundaries.
