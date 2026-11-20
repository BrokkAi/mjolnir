# Processes and the operating system

Build each as its own package, finishing one before starting the next.

1. `go/rs/rsprocess` for `std::process` and its Unix and Windows extensions.
   Oracle module `std_process.rs`, feature `std_process`. Mjolnir spawns
   children in hundreds of places (its helpers are in
   `mj-core/src/subprocess.rs`: read it), and how a child is spawned is
   observable. Reproduce `std/src/process.rs` and `std/src/sys/process/`:
   - `Command`: `new`, `arg`, `args`, `env`, `envs`, `env_remove`,
     `env_clear`, `current_dir`, the three streams with every `Stdio` form,
     the getters, `spawn`, `output`, `status`. Program lookup as Rust does it
     (the child's `PATH` if one was set, no shell), the environment as Rust
     builds it, the argument vector verbatim, and on Windows Rust's own
     quoting and its `.exe`, `cmd` and batch rules.
   - Unix: `uid`, `gid`, `groups`, `process_group`, `arg0`, `exec`, and
     `pre_exec`. Go cannot run arbitrary code between fork and exec: list
     every `pre_exec` closure the product installs and what it does, and
     provide a named option for each that maps onto `syscall.SysProcAttr` or a
     helper re-exec. Report any that cannot be expressed.
   - `Child`: `id`, `wait`, `try_wait`, `kill`, `wait_with_output`, the stream
     handles; dropping a `Child` neither kills nor waits.
   - `ExitStatus` with its exact `Display` (`exit status: 1`, `signal: 9
     (SIGKILL)`, core dumps, the Windows forms), `ExitCode`, `Output`,
     `exit`, `abort`, `id()`.
   - Errors are `rsio.Error` values. Only descriptors 0, 1, 2 reach the child.
   Tests: the std tests that apply; differential tests spawning a helper
   (written in Go under `testdata/`, built by the test; the oracle spawns the
   same binary) that prints its argv, environment, cwd, process group and
   session, open descriptors, signal dispositions and mask, under many
   `Command` configurations; and exit status text for each outcome.
2. `go/rs/rsnet` for `std::net` and `std::os::unix::net`: addresses (parsing
   with Rust's acceptance rules and error text, `Display`), `ToSocketAddrs`,
   TCP, UDP and Unix sockets with the methods used, the error kind on timeout
   per platform, abstract addresses on Linux, listener defaults as Rust sets
   them, errors as `rsio.Error`.
3. `go/crates/sysinfo` for sysinfo 0.32.1 (feature `system`). Oracle
   `sysinfo_oracle.rs`. Process inspection in about 9 files: `System`, the
   refresh calls with their kinds, `Process` accessors, `Pid`, `Signal`,
   `ProcessStatus`. Port the Linux implementation from `/proc` (how name, exe
   and cmd are derived, the `(deleted)` suffix, threads, how `cpu_usage` is
   computed and normalised, `start_time`), macOS through `libproc`/`sysctl`
   without C as far as possible (report what is not), Windows through
   `golang.org/x/sys/windows`. Compare on Linux for live helper processes in
   known states.
4. `go/crates/portablepty` for portable-pty 0.9.0: `native_pty_system`,
   `openpty`, `PtyPair`, `MasterPty`, `SlavePty::spawn_command`,
   `CommandBuilder` with its environment handling, `Child`, `ExitStatus`. On
   Unix exactly what the crate does when spawning in a pty (new session,
   controlling terminal, signal dispositions, close-on-exec, termios, size);
   ConPTY on Windows.
5. `go/crates/signalhook` for signal-hook 0.3.18: what one file in
   `mj-targets` uses (`flag::register`, `low_level`), over `os/signal`.

No C. Everything builds for linux, darwin and windows on amd64 and arm64;
platform code goes behind build tags.
