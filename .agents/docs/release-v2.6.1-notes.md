Mjolnir 2.6.1 fixes a Codex restart failure that could leave a session permanently showing **Unreachable** before its first prompt.

- **Prevent unused Codex sessions from entering a crash loop.** Mjolnir recreates a native thread only when it can prove the thread was unused. Existing conversations retain their native history, while settings and pending work stay intact. This covers both worker restarts and agent-process crashes.
- **Improve session startup and recovery.** Container uploads retain the correct ownership, launch failures keep useful diagnostics, and stale Git remote-tracking problems can be repaired during session creation.
- **Improve macOS development launches.** Development runs prepare portable Linux workers for container sessions and refresh stale controller processes.
- **Make path entry consistent.** Terminal and web editors share path handling, including home-directory expansion and validation before launch.
- **Choose which agent profiles are enabled.** Enabled profiles are available through the profile-selection controls.

The Codex fix preserves imported sessions and sessions with existing or uncertain history; it does not replace a used conversation with an empty one.
