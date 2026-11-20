# Translating Mjolnir from Rust to Go: the rules

This is the rulebook for the Go implementation of Mjolnir ("mjolnir-go") under
`go/`. Read all of it before you port anything. The plan it serves is
`.agents/plans/port-mjolnir-to-go.md`; the Go packages that stand in for Rust's
standard library and for third-party crates are listed in
`.agents/docs/go-port/runtime-packages.md`.

## 0. What a port is

The Go code must do what the Rust code does on every path: the same bytes on
the wire, in files, in the database and on the terminal, the same error text,
the same ordering, the same behaviour when something fails or is cancelled. A
user must not be able to tell which implementation they are running, and a Rust
daemon must be able to drive a Go worker (and the reverse) without either
noticing.

So you translate. You do not redesign, simplify, modernise, optimise or
"improve". Non-idiomatic Go is expected and fine. In particular:

- One Go file for each Rust file, with the items in the same order.
- Every string literal is carried over byte for byte, including format strings
  (rule 8), log messages and error text.
- Every comment is carried over. Reword a comment only where it names a Rust
  construct that no longer exists in the Go text.
- When the Rust code looks wrong, port it as it is and say so in your report.
  Do not fix it in the port: the two implementations must agree, and the fix
  belongs in the Rust source first.
- Never leave a gap silently. Code you could not port is `rs.Todo("<rust
  path>: <why>")`, which panics when reached and which `go/port/status.py`
  counts. An omitted branch, a skipped test or a stubbed return value that
  looks like real code is the worst thing you can leave behind.

When a rule here does not cover a construct, pick the translation that keeps
the Rust behaviour on every path, use it, and describe it in your report so the
rule can be added. Do not invent a new shared helper for it in a product
package; helpers go in the runtime packages.

## 1. Layout

The Go module is `github.com/BrokkAi/mjolnir`, with `go.mod` at the repository
root so that the Go build embeds the same asset files the Rust build does.

| Rust | Go |
| --- | --- |
| library crate `mj-core` (library `mj_core`) | package `mjcore` in `go/mjcore/`, all files in that one directory |
| program crate `mj-cli` (binary `mj`) | package `main` in `go/cmd/mj/` |
| program crate `mj-worker-bin` (binary `mj-worker`) | package `main` in `go/cmd/mj-worker/` |
| `src/lib.rs`, `src/main.rs` | `lib.go`, `main.go` |
| `src/a/b.rs` or `src/a/b/mod.rs` (module `a::b`) | `a_b.go` |
| inline `#[cfg(test)] mod tests { .. }` in `src/a/b.rs` | `a_b_test.go` |
| a test-only file such as `src/a/tests.rs` | `a_test.go`; `src/a/test_fixtures.rs` is `a_test_fixtures_test.go` |
| `tests/x.rs` (integration test) | `it_x_test.go` in the crate's Go directory |
| Rust standard library | `go/rs/...` |
| third-party crate `foo-bar` | `go/crates/foobar/` |

A crate is one flat Go package because Rust modules inside a crate refer to
each other in cycles and Go packages cannot.

A Go file name ending in `_test`, an operating system or an architecture
(`_linux`, `_windows`, `_unix`, `_arm64`, ...) changes how Go builds it. A Rust
module whose name ends that way gets `_mod` appended: `terminal_windows.rs` is
`terminal_windows_mod.go`.

`python3 go/port/portmap.py file <path.rs>` prints the Go file, the Go name of
every item, what the file needs, and the Go name of every test. Use it; do not
guess.

Every ported file starts with this header, which is how the port's progress and
drift are measured:

    // Port of mj-core/src/runtime.rs
    // rust-blob: <output of `git rev-parse HEAD:mj-core/src/runtime.rs`>

A test file ported from the same Rust file carries the same two lines.

## 2. Names

Names are derived mechanically so that two people porting different files
arrive at the same identifier without talking to each other.

Module prefix. An item defined in module `a::b_c` gets the prefix `ABC` (each
path segment in PascalCase, joined). Items in the crate root have no prefix.
An inline `mod tests` adds nothing.

- `mj_core::runtime::ShutdownReport` is `mjcore.RuntimeShutdownReport`.
- `mj_core::relay::protocol::RelayRequest` is `mjcore.RelayProtocolRelayRequest`.
- `mj_core::relay::MAX_FRAME_BYTES` is `mjcore.RelayMaxFrameBytes`.
- `mj_core::runtime::block_on` is `mjcore.RuntimeBlockOn`.

The prefix comes from the module that defines the item, never from a
re-export: `pub use config::Config` in `lib.rs` does not create a second name.

Case. PascalCase and camelCase are mechanical: split on `_`, capitalise each
part, lower-case the rest of an all-capitals part. `session_id` is `sessionId`
or `SessionId`, `HTTP_PORT` is `HttpPort`. Do not apply Go's initialism style
(`ID`, `URL`).

Visibility. `pub` becomes an exported (capitalised) name. Everything else
(`pub(crate)`, `pub(super)`, private) becomes an unexported name with a
lower-case first letter: private `runtime::InFlightGuard` is
`runtimeInFlightGuard`. This applies to types, functions, constants, statics,
struct fields and methods alike.

Methods and associated items.

- A method with a `self` receiver keeps its own name with no prefix:
  `fn negotiate(self, ..)` is `Negotiate`, private `fn drain(&mut self)` is
  `drain`.
- An associated function without `self`, and an associated constant, is a
  package-level name: the type's Go name followed by the item's name.
  `ShutdownState::new()` is `RuntimeShutdownStateNew()`,
  `RelayVersionRange::CURRENT` is `RelayProtocolRelayVersionRangeCurrent`.
  The item's own visibility decides the case of the first letter.
- When a type has a field and a method whose Go names would be equal, the
  method gets the suffix `Method`. `portmap.py file` shows this.

Enum variants are `Enum_Variant`, with the underscore:
`RelayRequest::Hello` is `RelayProtocolRelayRequest_Hello`. The underscore keeps
a variant apart from a separate type that happens to be named `EnumVariant`.

Trait implementations use fixed names:

| Rust | Go |
| --- | --- |
| `impl Display for T` | `func (t *T) FmtDisplay(f *rs.Formatter) error` |
| `impl Debug for T` (hand-written) | `func (t *T) FmtDebug(f *rs.Formatter) error` |
| `impl Default for T` | `func TDefault() T` |
| `impl From<A> for T` | `func TFromA(a A) T`, with `A` the Go name of the source type without its package |
| `impl TryFrom<A> for T` | `func TTryFromA(a A) (T, error)` |
| `impl FromStr for T` | `func TFromStr(s string) (T, error)` |
| `impl Drop for T` | `func (t *T) Drop()` |
| `impl Clone for T` (hand-written) | `func (t *T) Clone() T` |
| `impl PartialEq for T` (hand-written) | `func (t *T) Equal(other *T) bool` |
| `impl Ord for T` / `PartialOrd` (hand-written) | `func (t *T) Compare(other *T) int` |
| `impl Hash for T` (hand-written) | `func (t *T) HashKey() string` |
| `impl Iterator for T` | `func (t *T) Next() rs.Option[Item]` |
| `impl Serialize` / `Deserialize` (hand-written) | `SerializeSerde` / `DeserializeSerde`, see rule 9 |
| `impl Deref<Target = U> for T` | `func (t *T) Deref() *U`, and callers write the call out |
| operator traits (`Add`, `Sub`, `Index`, ...) | a method of the trait's method name: `Add`, `Sub`, `Index` |

A derived `Debug`, `Clone`, `PartialEq`, `Eq`, `Ord`, `Hash` or `Default` needs
no method: the runtime does it by reflection (rule 4).

Local variables and parameters keep their names in camelCase. Rust lets a
binding be declared again in the same scope (`let path = path.trim();`). When
the new value has the same type, assign; when it does not, add a digit
(`path2`). A name that is a Go keyword or shadows a package you use gets a
trailing underscore (`type_`, `rs_`).

Tests. Rust test `fn relay_rejects_stale_attach` in module `relay::protocol`
is `func TestRelayProtocol_relay_rejects_stale_attach(t *testing.T)`: `Test`,
the module prefix, an underscore, then the Rust name unchanged. An integration
test in `tests/acp.rs` is `TestItAcp_<name>`. The names are checked against the
Rust sources, so a renamed or missing test is reported.

Third-party and standard-library names follow the same prefix rule over the
path people write in Rust: `tokio::sync::mpsc::channel` is
`tokio.SyncMpscChannel`, `std::fs::read_to_string` is `rsfs.ReadToString`,
`serde_json::to_string` is `serdejson.ToString`. The catalogue lists the
packages.

## 3. Types

| Rust | Go |
| --- | --- |
| `bool`, `u8`..`u64`, `i8`..`i64`, `f32`, `f64` | `bool`, `uint8`..`uint64`, `int8`..`int64`, `float32`, `float64` |
| `usize`, `isize` | `int` (see the note below) |
| `u128`, `i128` | `rs.U128`, `rs.I128` |
| `char` | `rune`; format it with `rs.Char(r)` |
| `String`, `&str`, `Box<str>`, `Arc<str>`, `Cow<str>` | `string` |
| `Vec<u8>`, `&[u8]`, `Bytes` | `[]byte` |
| `Vec<T>`, `&[T]`, `Box<[T]>`, `Arc<[T]>` | `[]T` |
| `[T; N]` | `[N]T` |
| `(A, B)` | two results when returned from a function; `rs.Tuple2[A, B]` when stored or passed |
| `()` | nothing; `rs.Unit` where a type is required |
| `Option<T>` | `rs.Option[T]`, always, including `Option<Box<T>>` and `Option<Arc<T>>` |
| `Result<T, E>` | `(T, error)`; `Result<(), E>` is `error`; `rs.Result[T]` when stored or sent |
| `Box<T>` | `*T` |
| `Box<dyn Trait>`, `Arc<dyn Trait>`, `&dyn Trait` | the Go interface |
| `Arc<T>`, `Rc<T>` | `*T`; `rs.Arc[T]` for the types in the counted list (rule 4) |
| `Weak<T>` | `rs.Weak[T]` |
| `&T`, `&mut T` | `*T` for structs, arrays and anything you assign through; the value itself for `&str`, `&[T]`, `&Path` and small `Copy` types read only |
| `HashMap<K, V>`, `HashSet<T>` | `collections.HashMap[K, V]`, `collections.HashSet[T]` |
| `BTreeMap<K, V>`, `BTreeSet<T>` | `collections.BTreeMap[K, V]`, `collections.BTreeSet[T]` |
| `VecDeque<T>` | `collections.VecDeque[T]` |
| `IndexMap<K, V>` | `indexmap.IndexMap[K, V]` |
| `PathBuf`, `&Path`, `OsString`, `&OsStr` | `string`, handled only through `rspath` and `rs` (rule 7) |
| `Duration` | `time.Duration` |
| `Instant` | `rstime.Instant` (`tokio.TimeInstant` for `tokio::time::Instant`) |
| `SystemTime` | `time.Time` |
| `Mutex<T>`, `RwLock<T>` | `rssync.Mutex[T]`, `rssync.RwLock[T]` |
| `AtomicBool`, `AtomicU64`, ... | `rssync.AtomicBool`, `rssync.AtomicU64`, ... |
| `OnceLock<T>`, `LazyLock<T>` | `rssync.OnceLock[T]`, `rssync.LazyLock[T]` |
| `RefCell<T>`, `Cell<T>` | `rs.RefCell[T]`, `rs.Cell[T]` |
| `serde_json::Value` | `serdejson.Value` (the `jsonvalue` representation of `github.com/BrokkAi/acp-go`) |
| `anyhow::Error`, `Box<dyn Error>` | `error` |
| `impl Future<Output = T>`, `BoxFuture<T>`, `Pin<Box<dyn Future..>>` | `tokio.Fn[T]`, which is `func(ctx context.Context) T` |
| `impl Fn(A) -> B`, `Box<dyn FnMut(A) -> B>` | `func(A) B` |
| types of the `agent-client-protocol` crate | the types of `github.com/BrokkAi/acp-go`; see `.agents/docs/go-port/acp-mapping.md` |

`usize` is `int` because Go lengths and indexes are `int`. Rust arithmetic that
would overflow panics in the development profile, where the tests run, so
correct code never relies on wrapping. Where the Rust code asks for a specific
overflow behaviour it says so, and you use the matching helper:
`rs.CheckedAdd`, `rs.SaturatingSub`, `rs.WrappingMul`, and so on. `usize::MAX`
is `rs.UsizeMax`. A cast written with `as` from a float to an integer is
`rs.CastF64ToI64` and friends, because Rust saturates and Go does not.

Structs.

- A `Copy` struct has value receivers; every other struct has pointer
  receivers.
- Fields keep their order. Each field has a tag carrying its Rust name, which
  formatting and serialization read: `AwaitsLeft int` with the tag
  `rs:"awaits_left"`. Serde attributes go in a `serde` tag (rule 9).
- A newtype over one field is a defined type: `struct SessionId(String)` is
  `type SessionId string`. A tuple struct of several fields has fields `F0`,
  `F1`, ...
- Construct with every field named. Rust refuses a struct literal that leaves a
  field out; Go silently zeroes it, and the `exhaustruct` check in CI rejects
  that. `..Default::default()` and `..base` become a copy of the default or
  base value followed by assignments.

Enums.

- An enum whose variants carry no data is `type E uint8` (or its `#[repr]`
  type, with explicit discriminants kept) and constants `E_Variant`, registered
  once with `rs.RegisterEnum`.
- Any other enum is a sealed interface `E` with an unexported method `isE()`,
  and one struct for each variant, `E_Variant`. A value is always a pointer to
  the variant struct: `&RelayEvent_Started{Id: id}`, `&RelayEvent_Idle{}`.
  Named fields keep their names in PascalCase; tuple fields are `F0`, `F1`, ...
- `match` is a type switch (or a `switch` on the constant). Every match the
  Rust compiler checked as exhaustive ends in
  `default: panic(rs.Unreachable(v))`, and the `go-check-sumtype` check in CI
  enforces that each variant is handled. A `_ =>` arm in Rust is a `default:`
  with the Rust arm's body.
- Pattern guards, bindings with `@`, nested patterns and `if let ... else`
  become explicit conditions in the same order. Rust evaluates arms top to
  bottom and guards in order; keep that order.
- `matches!(v, E::A | E::B { .. })` is `rs.Is[*E_A](v) || rs.Is[*E_B](v)`.
- An enum the Rust code copies implicitly (`derive(Copy)`) and then changes in
  place through a pattern must be copied with `rs.Clone` first, because two Go
  interface values can point at one variant struct.

Traits are Go interfaces with the same method names. Every trait method is in
the interface. An implementation that relies on a default method adds a
one-line method calling `<Trait>Default<Method>(self, ..)`, the package-level
function that holds the default body. Go methods cannot have type parameters,
so a generic method becomes a package-level generic function named
`<Type><Method>`. An extension trait (a trait implemented for a type the crate
does not own) becomes package-level functions named `<Trait><Method>`.

Generics become Go generics with the bound as the constraint. Where Go cannot
express the bound, take the interface or a function argument, and say so in a
comment.

Nothing in Rust is nil. A nil pointer, nil interface or nil map in Go where the
Rust type is not an `Option` is a porting error; the serializer and the
collections panic on it rather than write `null`.

## 4. Ownership: clone, equality, drop

Rust copies only when the code says `.clone()`. Go copies a struct on every
assignment, shallowly, so two copies share slices, maps and pointers.

- A Rust move (assignment, argument, return) is a plain Go assignment. The
  source is not used again, so sharing is harmless.
- `.clone()` is `rs.Clone(v)`, which copies deeply by reflection: slices,
  collections, boxed values and enum variants are duplicated; a pointer field
  whose Rust type is `Arc<T>` or `Rc<T>` is shared, which its tag
  `rs:"name,arc"` tells the cloner. Cloning an `Arc<T>` you hold directly is a
  plain pointer copy. A type with a hand-written `Clone()` method uses it.
  Strings, numbers, and `Copy` types need no call.
- `==` on anything that is not a number, string, bool or data-free enum is
  `rs.Eq(a, b)`, which compares by structure the way `derive(PartialEq)` does
  and uses a hand-written `Equal` where one exists. Go's `==` on an interface
  or pointer compares identity, and on a struct with a slice does not compile.
  Ordering (`<`, `cmp`, `max`, sorting keys) on such values is `rs.Cmp(a, b)`.
- `{:?}` on a derived `Debug` is done by the formatter from the registered Rust
  names (rule 8).
- A slice element or map entry is changed in place through a pointer:
  `item := &items[i]`, `entry, ok := m.GetMut(key)`. Never range over a slice by
  value (`for _, item := range items`) when the Rust loop borrows (`&items`,
  `iter()`, `iter_mut()`): the copy hides writes and duplicates state.

Counted references. Most `Arc<T>` values only share ownership and are plain
pointers. The types listed in `.agents/docs/go-port/counted-arcs.md` are ones
the Rust code downgrades to `Weak`, counts (`Arc::strong_count`), or takes back
(`Arc::try_unwrap`, `Arc::get_mut`); they are `rs.Arc[T]`, with an explicit
`Clone()` for every Rust `Arc::clone` and an explicit `Drop()` where Rust drops
the handle. `Arc::make_mut(&mut a)` on a plain pointer is "copy, then change the
copy": `a = rs.ClonePtr(a)` followed by the mutation.

Drop. Go has no destructors, and a great deal of Rust behaviour lives in them:
closing a channel when the last sender goes, unlocking, deleting a temporary
directory, killing a child process, cancelling a token, releasing a permit.

- A type with `impl Drop`, or with a field that must be dropped, has a `Drop()`
  method. `Drop()` is idempotent: a second call does nothing.
- The runtime types that must be dropped say so in their documentation
  (`tokio.SyncMpscSender`, `rssync.MutexGuard`, `tempfile.TempDir`, ...).
- Where a Rust binding owns such a value to the end of its scope, write
  `defer v.Drop()` on the line after it is created. Deferred calls run last-in
  first-out, as Rust drops locals.
- Where Rust drops earlier (`drop(v)`, the end of an inner block, an
  assignment that replaces the value, a temporary at the end of its
  statement), call `v.Drop()` at that point. The deferred call, if there is
  one, then does nothing.
- Where the value is moved away (returned, stored in a struct, sent, captured
  by a spawned task), the new owner drops it and this scope does not. A struct
  that owns droppable fields drops them in its own `Drop()`, in declaration
  order.
- A lock guard that Rust holds only for one statement
  (`state.lock().unwrap().push(x);`) is `state.With(func(v *T) { ... })`.
  Deferring that unlock to the end of the function is a deadlock.

Getting a drop wrong does not fail to compile. It shows up as a hang, a leaked
process or a lock that is never released, so treat every `let` of a droppable
type as a question: where does this value die?

## 5. Errors and panics

- `?` is `if err != nil { return zero, err }`. Where `?` converts the error
  through a `From` impl, call the conversion: `return zero, FooErrorFromIo(err)`.
- `anyhow` is `go/crates/anyhow`. `bail!("..")` is
  `return zero, anyhow.Errorf("..", args...)`; `anyhow!("..")` is
  `anyhow.Errorf`; `ensure!(c, "..")` is `if !c { return .. }`;
  `.context("..")?` is `anyhow.Context(err, "..")`;
  `.with_context(|| format!(..))?` is `anyhow.WithContext(err, func() string {..})`;
  `.context("..")` on an `Option` is `anyhow.Msg("..")` when the option is None.
- Do not write `fmt.Errorf("...: %w", err)`. An `anyhow` error prints only its
  outermost message with `{}`, the whole chain joined by `: ` with `{:#}`, and
  a "Caused by:" list with `{:?}`. `anyhow.Context` keeps those three forms
  apart; `%w` collapses them and changes text users and tests see.
- `error.downcast_ref::<T>()` is `anyhow.DowncastRef[T](err)`; `error.chain()`
  is `anyhow.Chain(err)`; `error.root_cause()` is `anyhow.RootCause(err)`.
- An error enum is a Go enum (rule 3) whose variants implement `error` through
  their `FmtDisplay`. Match on it with a type switch after
  `anyhow.DowncastRef`.
- Errors from the operating system must read as Rust prints them ("No such
  file or directory (os error 2)"), so file, process and socket calls go
  through `rsfs`, `rsio`, `rsprocess` and `rsnet`, never straight to `os` or
  `net`. `error.kind() == ErrorKind::NotFound` is
  `rsio.KindOf(err) == rsio.ErrorKind_NotFound`.
- Return a literal `nil` for `Ok(())`. A nil pointer of a concrete error type
  stored in an `error` is not nil.

Panics. `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `assert!` outside
tests, `.unwrap()` and `.expect()` are `rs.Panicf`, `rs.Unreachable`,
`rs.Assert`, `rs.Unwrap(v, err)`, `opt.Unwrap()`, `rs.Expect(v, err, "msg")`,
`opt.Expect("msg")`, which produce Rust's message text. `catch_unwind` is
`rs.CatchUnwind`. A panic in a spawned task or thread does not end the process
in Rust and must not in Go: tasks and threads are started only through
`tokio.Spawn`, `tokio.SpawnBlocking` and `rsthread.Spawn`, which recover the
panic, run the panic hook and report it through the join handle. A bare `go`
statement is forbidden in product packages. A program's `main` runs inside
`rs.Main`, which gives a panic Rust's exit status (101).

A poisoned lock is real behaviour. `m.lock().unwrap()` is `m.Lock()`, which
panics when the mutex is poisoned; `m.lock().expect("..")` is
`m.LockExpect("..")`; `m.lock().unwrap_or_else(|e| e.into_inner())` is
`m.LockInner()`.

## 6. Async code and concurrency

The runtime package `go/crates/tokio` reproduces tokio's scheduling,
cancellation, channels and clock. It decides how tasks interleave; you
translate what the task does.

Functions.

- A function takes `ctx context.Context` as its first parameter if and only if
  it is `async` in Rust, and returns its result directly. `.await` on a call
  is the call with `ctx` passed. Functions that are synchronous in Rust never
  take `ctx`, including the ones that reach the runtime through its
  thread-local (`tokio::spawn`, `spawn_blocking`, `Handle::current()`,
  `Instant::now()`); the Go runtime finds the calling goroutine's runtime the
  same way. The model is in `.agents/docs/go-port/tokio-contract.md`.
- `ctx` is the identity of the running task, not a deadline to check. Never
  test `ctx.Err()`, never select on `ctx.Done()`, and never wrap it with
  `context.WithTimeout` or `WithCancel`. Use `tokio.TimeTimeout`,
  `tokioutil.SyncCancellationToken` and `tokio.Select` as the Rust code does.
- Cancellation works as it does in Rust. A future that is dropped stops at the
  `.await` it is suspended in, its destructors run, and nothing after that
  point executes. In Go the runtime call the goroutine is blocked in ends the
  goroutine and runs its deferred calls. So any call that takes `ctx` may never
  return, and everything Rust does in a destructor must be in a `defer`
  (rule 4). Do not add clean-up after an awaiting call expecting it to run on
  cancellation.
- An `async` block or a future stored in a variable is a `tokio.Fn[T]` value:
  `func(ctx context.Context) T { .. }`. It is lazy, as a Rust future is:
  nothing runs until it is awaited (called), spawned, or handed to a select.
  A `move` block that captures a `Copy` value later changed by the enclosing
  code must copy it into a new local first, because a Go closure captures the
  variable, not its value.
- Blocking calls made from async code (`std::fs`, `std::process`, a
  `std::sync::Mutex`) stay blocking calls in Go, in the same places.

Tasks.

| Rust | Go |
| --- | --- |
| `mj_core::runtime::spawn(fut)`, `tokio::spawn(fut)` | `mjcore.RuntimeSpawn(fn)`, `tokio.Spawn(fn)` returning `*tokio.TaskJoinHandle[T]` |
| `handle.await` | `handle.Await(ctx)` returning `(T, *tokio.TaskJoinError)` |
| `handle.abort()` | `handle.Abort()` |
| `tokio::task::spawn_blocking(f)` | `tokio.TaskSpawnBlocking(f)` |
| `tokio::task::yield_now().await` | `tokio.TaskYieldNow(ctx)` |
| `JoinSet`, `AbortHandle` | `tokio.TaskJoinSet[T]`, `tokio.TaskAbortHandle` |
| `std::thread::spawn(f)` | `rsthread.Spawn(f)` |

Select. `tokio::select!` is `tokio.Select`, which returns the index of the arm
that completed; the arm's value is stored through the pointer you give it, and
a `switch` on the index holds the arm bodies, so `break`, `continue` and
`return` keep working:

    var msg rs.Option[Message]
    var joined tokio.TaskJoinResult[rs.Unit]
    switch tokio.Select(ctx, tokio.SelectBiased,
        tokio.ArmIf(!tasks.IsEmpty(), tasks.JoinNextFuture(), &joined),
        tokio.Arm(rx.RecvFuture(), &msg),
        tokio.Arm(cancel.CancelledFuture(), nil),
    ) {
    case 0:
        ...
    case 1:
        ...
    case 2:
        break loop
    }

- Keep the arms in the Rust order. Pass `tokio.SelectBiased` exactly when the
  Rust macro says `biased;`, and `tokio.SelectUnbiased` otherwise.
- Every awaitable runtime operation has two forms: `rx.Recv(ctx)` waits (it is
  `rx.recv().await`), and `rx.RecvFuture()` is the future itself, for an arm.
- `, if cond` is `tokio.ArmIf(cond, ..)`. A refutable pattern
  (`Some(x) = rx.recv() =>`) is `tokio.ArmMatch`, and `else =>` is
  `tokio.ArmElse()`.
- An arm that awaits an arbitrary future (`result = self.sync() => ..`) is
  `tokio.ArmFn(func(ctx context.Context) T { return self.sync(ctx) }, &result)`.
  The future starts when the select starts and is dropped, as in Rust, if
  another arm wins.
- A future pinned outside a loop and polled by reference
  (`tokio::pin!(fut); loop { select! { r = &mut fut => .. } }`) is
  `fut := tokio.Start(fn)` before the loop, `defer fut.Drop()`, and
  `tokio.Arm[T](fut, &r)` in the select.
- `tokio::join!`, `try_join!`, `futures::future::join_all` are `tokio.Join2`,
  `tokio.TryJoin2`, `futures.FutureJoinAll`. They run their futures on the same
  task, as Rust does.

Do not use Go's `select`, bare channels, `sync.Mutex`, `sync.WaitGroup`,
`time.Sleep`, `time.After` or `time.Now` in product packages. Tokio's types
have different closing, fairness, lagging and clock behaviour, and the tests
run on a paused clock that only the runtime's own types follow.

Tests. `#[tokio::test]` is `tokio.Test(t, opts, func(ctx context.Context) { .. })`
with the same flavour, worker count and `start_paused` setting. A
current-thread test runs its tasks one at a time and switches only at an await,
exactly as tokio's current-thread scheduler does, so a test that relies on a
spawned task not having run yet stays deterministic.

## 7. Strings, paths and collections

Strings. A Rust `String` is valid UTF-8; a Go `string` is any bytes.

- Lengths and indexes are in bytes in both. `s.chars().count()` is
  `strs.CharsCount(s)`.
- `String::from_utf8(v)?` is `strs.FromUtf8(v)` and can fail;
  `from_utf8_lossy` is `strs.FromUtf8Lossy`, which substitutes as Rust does (Go's
  own conversion substitutes differently).
- Use `strs` for every method whose result depends on Unicode tables or on
  Rust's exact rules: `to_lowercase`, `to_uppercase`, `trim*`, `lines`,
  `split_whitespace`, `char::is_alphanumeric`, `is_whitespace`, `rsplitn`,
  `split_terminator`, `char_indices`, `escape_debug`, `eq_ignore_ascii_case`.
  Plain byte operations (`contains`, `starts_with`, `find` of a literal,
  `split` on a literal, `replace`) may use `strings`.
- `min_by_key` returns the first minimum and `max_by_key` the last maximum.
  `sort`, `sort_by` and `sort_by_key` are stable: use `rs.SortStable`.
  `sort_unstable*` is `rs.SortUnstable`, which is Rust's own algorithm, because
  the order it leaves equal elements in is visible.

Paths. A path is a `string`, and only `rspath` may interpret it.
`rspath.Join(a, b)` does not clean the result; `path.join("../x")` in Rust
keeps the `..`, and `filepath.Join` removes it. Do not import `path` or
`path/filepath` in product packages. `path.display()` and `to_string_lossy()`
are `rspath.Display(p)`; `path.to_str()` is `rspath.ToStr(p)` and is None for
bytes that are not UTF-8.

Collections. Use the `collections` types and their Rust-named methods (`Get`,
`GetMut`, `Insert`, `Remove`, `Entry`, `Retain`, `Iter`, `Range`, `PopFirst`,
...) rather than Go maps. `BTreeMap` and `BTreeSet` iterate in key order, which
the wire formats and the goldens depend on. A zero-value collection is empty
and usable. Iterate with `for k, v := range m.Iter()`, where `v` is a pointer
into the map.

Iterator chains become loops that do the same work in the same order, or the
`rs` iterator helpers where the chain is a plain map or filter. Keep
short-circuiting (`any`, `all`, `find`, `position`, `take_while`, and
`collect::<Result<_, _>>()`, which stops at the first error).

## 8. Formatting and logging

Format strings are not translated to Go verbs. `rs.Format` reads Rust's format
syntax at run time, so the string stays exactly as it is in the Rust source:

    format!("could not read {}: {error:#}", path.display())
    rs.Format("could not read {}: {error:#}", rspath.Display(path), rs.Arg("error", err))

- Positional arguments are passed in order. A name captured from the
  surrounding scope (`{error}`, `{count:>4}`) is passed as
  `rs.Arg("error", err)`.
- `write!(w, ..)`, `writeln!`, `print!`, `println!`, `eprintln!` are `rs.Write`,
  `rs.Writeln`, `rs.Print`, `rs.Println`, `rs.Eprintln`. `x.to_string()` is
  `rs.ToString(x)`.
- `{}` uses the value's `FmtDisplay`; `{:?}` its `FmtDebug` or, for a derived
  `Debug`, the reflected form with the registered Rust names. Numbers, floats,
  strings, options, slices, maps, durations and errors print as Rust prints
  them.
- A `rune` is an integer to Go. Pass a `char` as `rs.Char(r)`.
- Do not use `fmt.Sprintf`, `fmt.Sprint`, `strconv.Quote` or `%v` to build
  text in product packages.

Logging is `go/crates/tracing`. The target of an event is the Rust module
path, which filters and log lines show, so each ported file declares it once:

    const runtimeTarget = "mj_core::runtime"

    tracing::warn!(session_id = %id, error = ?err, in_flight, "shutting down {}", name);
    tracing.Warn(runtimeTarget, "shutting down {}",
        tracing.Display("session_id", id), tracing.Debug("error", err),
        tracing.Field("in_flight", inFlight), name)

Fields keep their Rust names and order.

## 9. Serialization

Wire messages, journal entries, database columns and configuration files are
read and written by both implementations, and some code measures the encoded
length, so the encoding must match byte for byte. `go/crates/serde` implements
serde's data model and derive behaviour; `serdejson`, `toml` and the other
format packages produce what the Rust crates produce, including error text.

Type declarations for serialized types are generated from the Rust source by
`go/port/portmap.py declarations <file.rs>`; start from its output instead of
typing them. It writes:

- the `rs:"rust_name"` tag on every field, and a `serde:".."` tag carrying the
  field's serde attributes in serde's own spelling (`default`,
  `skip_serializing_if=Option::is_none`, `rename=..`, `alias=..`, `flatten`,
  `with=..`, `skip`);
- one registration for each type, carrying its Rust name and its container
  attributes (`tag`, `content`, `untagged`, `rename_all`, `deny_unknown_fields`,
  `transparent`, `default`) and, for an enum, its variants in order.

A function named by `default = "path"`, `skip_serializing_if = "path"`,
`with = "module"`, `serialize_with` or `deserialize_with` is registered under
the Rust path the attribute names, and ported like any other function.

`serde_json::to_string(&v)?` is `serdejson.ToString(v)`; `from_str::<T>(s)?` is
`serdejson.FromStr[T](s)`; `json!({..})` is `serdejson.Json(..)`. Do not use
`encoding/json` in product packages: it escapes `<`, `>` and `&`, writes floats
and map keys differently, and reports different errors.

## 10. Conditional compilation

| Rust | Go |
| --- | --- |
| `#[cfg(unix)]`, `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` on an item | the item moves to `<file>_cfg_unix.go`, `<file>_cfg_windows.go`, `<file>_cfg_darwin.go`, each with the matching `//go:build` line |
| a `cfg` on a whole module | the whole Go file carries the `//go:build` line |
| `cfg!(windows)` in an expression | `rs.CfgWindows`, a constant |
| `#[cfg(test)]` | a `_test.go` file |
| `#[cfg(feature = "test-support")]`, `#[cfg(any(test, feature = "test-support"))]` | `<file>_feat_test_support.go` with `//go:build mj_test_support` |
| `feature = "test-hooks"`, `"test-identity"`, `"golden"`, `"github-history"` | the same with `mj_test_hooks`, `mj_test_identity`, `mj_golden`, `mj_github_history` |
| `#[cfg(debug_assertions)]`, `debug_assert!` | `rs.DebugAssertions` and `rs.DebugAssert`, on when built with the tag `mj_debug` |

Tests run with every feature tag the Rust tests run with:
`go test -tags 'mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden' ./go/...`.
`go/port/gotest.sh` supplies them.

`unsafe` blocks that call the operating system use `golang.org/x/sys/unix` and
`golang.org/x/sys/windows` with the same calls and flags. The Go build uses no
C: `CGO_ENABLED=0`.

## 11. Tests

Every Rust test is ported, with its name (rule 2), its assertions and its
fixtures. The Rust test suite is the specification; a test you cannot make
pass is a defect in the port, not in the test.

- `assert_eq!(a, b)`, `assert_ne!`, `assert!(c, "msg {}", x)` are
  `rstest.AssertEq(t, a, b)`, `rstest.AssertNe`, `rstest.Assert(t, c, "msg {}", x)`.
  They compare with `rs.Eq`, print both sides as Rust's `Debug` does, and stop
  the test as a Rust panic does.
- `#[should_panic(expected = "..")]` is `rstest.ShouldPanic(t, "..", func() {..})`.
- `#[ignore]` is `rstest.Ignored(t, "<the Rust reason>")` as the first line,
  which skips unless `MJ_RUN_IGNORED=1`.
- Golden files stay where they are. `mjcore.GoldenAssertGolden(t, "mj-core",
  name, actual)` reads `mj-core/tests/golden/<name>.txt`. The Go tests never
  rewrite a golden: the Rust tests own them, and a difference is a port bug.
- Fixtures are read from their Rust locations through the repository root
  (`rstest.RepoPath("mj-core/tests/fixtures/..")`) or, where Rust uses
  `include_str!`, through the embedded copy (`assets.MustString(..)`).
- `tempfile::tempdir()` is `tempfile.Tempdir()` with `defer dir.Drop()`.
- Do not call `t.Parallel()`. Rust tests that touch process-wide state
  (environment variables, the current directory) rely on locks the port keeps;
  running one at a time is always a valid schedule.
- A test that starts the real `mj` or `mj-worker` gets the Go binary from
  `rstest.Binary(t, "mj")`, which builds it once.
- Keep `// Hard-won:` comments above the tests that carry them.
- Do not add tests of your own for product code. If you believe a case is
  missing, say so in your report.

A test that passes for the wrong reason is worse than one that fails. If you
changed an expected value, loosened a comparison, or removed a case to get a
test to pass, you have not ported it.

## 12. What to hand back

Your report is read by the person merging the work, who has not seen your
session. Lead with the state: which files are fully ported, which tests pass
(the exact `go test` command and its summary line), and what is not done. Then
list, without padding:

- every `rs.Todo` you left, with the reason;
- every Rust test you did not port or that does not pass, with the reason;
- every construct this rulebook did not cover, and what you did;
- every place you found the Rust code wrong or surprising;
- every function you added to a runtime package.

Do not commit, and do not edit files outside the ones you were given, except to
add a missing function to a runtime package (in one edit that leaves the
package compiling, with a test).
