# Async runtime, part 2: I/O, networking, processes, files, signals

Package `go/crates/tokio` (the same package as the core). Oracle module
`tokio_oracle.rs`, feature `tokio`. Start only after the core
(`go/crates/tokio/ARCHITECTURE.md`) exists: read it and
`.agents/docs/go-port/tokio-contract.md` first. You add files; do not change
the core's scheduling code without saying so in your report.

Mjolnir uses `tokio::io` (`AsyncReadExt`/`AsyncWriteExt` about 100 times,
`AsyncBufReadExt` 43, `BufReader` 214, `duplex` 51), `tokio::net`
(`TcpListener`/`TcpStream` about 180, `UnixListener`/`UnixStream` about 130),
`tokio::process` (about 220, with `kill_on_drop`), `tokio::fs` (35),
`tokio::signal` (15), `tokio::io::{stdin, stdout, stderr}`. Grep the Rust
sources for the exact methods and cover all of them.

Every readiness wait is an await point of the core: cancellation ends the
goroutine there, a current-thread runtime gives up its run token while
waiting, and an operation tokio documents as cancel-safe loses nothing when
cancelled (bytes read into an internal buffer stay there for the next call).

1. Traits as Go interfaces, with the extension methods as package functions
   so they work on any implementation:

       type IoAsyncRead interface { PollRead... }   // design the minimal method set
       type IoAsyncWrite interface { ... }           // write, flush, shutdown
       type IoAsyncBufRead interface { IoAsyncRead; FillBuf(ctx) ([]byte, error); Consume(n int) }
       type IoAsyncReadWrite interface { IoAsyncRead; IoAsyncWrite }

   `AsyncReadExt`: `Read(ctx, buf)`, `ReadExact`, `ReadToEnd`, `ReadToString`,
   `ReadBuf`, `Take`, `Chain`, the integer readers used. `AsyncWriteExt`:
   `Write`, `WriteAll`, `WriteBuf`, `Flush`, `Shutdown`, the integer writers
   used. `AsyncBufReadExt`: `ReadLine`, `ReadUntil`, `Lines` (with
   `NextLine(ctx)`), `Split`, `FillBuf`, `Consume`. Each with tokio's results
   and errors (`UnexpectedEof` text, `WriteZero`, invalid UTF-8) as
   `rsio.Error` values, and tokio's documented cancel-safety.
2. `BufReader`, `BufWriter`, `BufStream` (default capacity 8 KiB as tokio's),
   `duplex(max_buf_size)` (an in-memory pipe with back-pressure and tokio's
   close semantics: reads return EOF after the writer is dropped, writes fail
   with `BrokenPipe` after the reader is dropped), `split`/`into_split`,
   `copy`, `copy_bidirectional`, `empty`, `sink`, `stdin`/`stdout`/`stderr`,
   `io::Cursor` and byte slices as readers (`&[u8]` implements `AsyncRead`),
   `Vec<u8>` as a writer, `simplex` if used.
3. `net`: `TcpListener` (`bind`, `accept`, `local_addr`, `from_std`,
   `into_std`), `TcpStream` (`connect`, `peer_addr`, `local_addr`,
   `set_nodelay`, `into_split`/`split`, `readable`/`writable`/`try_read`/
   `try_write` if used, `shutdown`), `UnixListener`, `UnixStream`
   (`connect`, `pair`, `into_split`, `peer_cred`), `UnixDatagram` if used,
   Windows named pipes if used. Over Go's `net` with deadlines for
   cancellation, errors converted as Rust reports them.
4. `process`: `Command` (the `std::process` surface from `go/rs/rsprocess`
   plus `kill_on_drop`, `spawn`, `output`, `status`), `Child` (`wait(ctx)`,
   `try_wait`, `kill(ctx)`, `start_kill`, `id`, the async `stdin`/`stdout`/
   `stderr` handles, `wait_with_output(ctx)`), and DROP: a child with
   `kill_on_drop(true)` is killed when its handle is dropped (give `Child` a
   `Drop()`), one without is left running and reaped in the background as
   tokio's orphan queue does.
5. `fs`: the functions and `File`/`OpenOptions` methods used, each the
   `go/rs/rsfs` operation run as tokio runs it (on the blocking pool, so the
   task yields while it runs).
6. `signal`: `ctrl_c()`, `unix::signal(SignalKind)` with `recv(ctx)` and
   tokio's coalescing, the `SignalKind` constructors used, Windows `ctrl_c`/
   `ctrl_break`/`ctrl_close` if used.

Tests: port tokio's tests for what you cover (`tests/io_*.rs`,
`tests/tcp_*.rs`, `tests/uds_*.rs`, `tests/process_*.rs`, `tests/fs*.rs`,
`tests/signal_*.rs`), and drive every stream type with more than 64 KB in both
directions at once, so that buffer-boundary deadlocks and truncation show
(AGENTS.md, Subprocess Rules). Trace tests against the oracle for the
current-thread interleaving of readers and writers over `duplex`.
