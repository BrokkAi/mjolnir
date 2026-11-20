# Counted and weak references in the Rust code (`rs.Arc` and `rs.Weak`)

Reference for rule 4 of `translation-rules.md` ("Counted references"). It lists
every place where the Rust product code uses an `Arc` for more than shared
ownership, gives the concrete type at each site, and concludes which types must
be the port's counted `rs.Arc[T]` / `rs.Weak[T]` and how the remaining sites
translate.

Read at commit `6f77b633f` (the Rust sources as merged from `origin/master` at
`9827337ca`). "Product code" is every `*.rs` file under `mj-*/` and
`voice-worker/` (about 855,000 lines; `voice-worker/` has no hit for any
pattern below). Line numbers are those of that commit. The Go names used for
the counted type are the ones in `go/rs/arc.go` (`rs.ArcNew`, `a.Clone()`,
`a.Drop()`, `rs.ArcDowngrade`, `w.Upgrade()`, `rs.ArcStrongCount`,
`rs.ArcGetMut`, `rs.ArcMakeMut`, `rs.ArcPtrEq`, `w.PtrEq`).

## 0. Method and totals

The site list was built by one grep and every hit was then read in context to
work out the concrete type:

    grep -rnE '\bWeak\b|Arc::downgrade|\.upgrade\(\)|strong_count|weak_count|try_unwrap|into_inner\(|Arc::get_mut|Rc::get_mut|make_mut|ptr_eq|\bRc\b|new_cyclic|unwrap_or_clone|Arc::as_ptr|Arc::into_raw|Arc::from_raw|increment_strong_count|decrement_strong_count|ptr::eq|\.downgrade\(\)|same_channel|WeakSender|WeakUnboundedSender' \
        --include='*.rs' mj-* voice-worker

`Arc::downgrade`, `strong_count`, `weak_count`, `try_unwrap`, `into_inner`,
`get_mut`, `make_mut`, `ptr_eq`, `as_ptr` and `unwrap_or_clone` are associated
functions of `Arc` (they take `this`, not `self`), so they can only be written
`Arc::name(..)`; the pattern `Arc::name` also matches `std::sync::Arc::name`.
No file renames `Arc` on import (`grep -rnE 'Arc as |sync::Arc as'` has no hit).

| Pattern | Lines | Production | Test | Test-support | Notes |
| --- | --- | --- | --- | --- | --- |
| `Weak<..>` in a type, `Weak::new`, `use ..Weak` | 21 | 19 | 2 | 0 | 16 type positions, 5 `use` lines; all `std::sync::Weak` |
| `Arc::downgrade` | 32 | 24 | 8 | 0 | one of them shares a line with `Weak::ptr_eq` |
| `.upgrade()` / `Weak::upgrade` | 31 | 26 | 5 | 0 | 19 method calls, 12 as a function path |
| `Weak::strong_count` (`weak.strong_count()`) | 6 | 6 | 0 | 0 | all in registry pruning |
| `Weak::ptr_eq` | 2 | 2 | 0 | 0 | one as `std::sync::Weak::ptr_eq(..)`, one as the method call `token.ptr_eq(&lifetime)` on a `Weak<()>` (`mj-worker/src/acp/operator_away.rs:564`) |
| `Arc::strong_count` | 7 | 2 | 5 | 0 | |
| `Arc::weak_count` | 0 | | | | |
| `Arc::try_unwrap` | 2 | 2 | 0 | 0 | |
| `Arc::into_inner` | 0 | | | | it is an associated function, so it can only be written `Arc::into_inner(..)`; the 118 lines that call some `.into_inner(` are on `PoisonError`, `Mutex`, `SharedSnapshot` and I/O wrappers |
| `Arc::unwrap_or_clone` | 7 | 6 | 1 | 0 | |
| `Arc::get_mut` | 20 | 3 | 16 | 1 | of 1,078 lines containing `get_mut`; the other 1,058 are map, slice, `Mutex` and `OnceLock` calls (`sessions.get_mut(` 183, `form.get_mut(` 156, ...) |
| `Rc::get_mut` | 0 | | | | |
| `Arc::make_mut` | 77 | 37 | 39 | 1 | |
| `Arc::ptr_eq` | 71 | 43 | 28 | 0 | |
| other `.ptr_eq(` (wrappers and look-alikes) | 20 | 4 | 16 | 0 | `SharedSnapshot::ptr_eq`, `futures::Shared::ptr_eq`; section 8. The `Weak` method call above is not counted here |
| `Rc` | 3 | 0 | 3 | 0 | two test helpers |
| `Arc::as_ptr` | 2 | 2 | 0 | 0 | section 8 |
| `std::ptr::eq` | 6 | 3 | 3 | 0 | on plain borrows; section 8 |
| `same_channel` | 4 | 3 | 1 | 0 | tokio; section 8 |
| `Arc::new_cyclic`, `Arc::into_raw`, `Arc::from_raw`, `increment_strong_count`, `.downgrade()` (tokio `WeakSender`), `WeakSender`, `WeakUnboundedSender` | 0 | | | | |

"Test" means inside a `#[cfg(test)]` item, a `#[test]` function, or a file
that is only compiled for tests (`tests.rs`, `*_tests.rs`, `tests/`).
"Test-support" means behind `#[cfg(any(test, feature = "test-support"))]`.
The split was made by a script that tracks those attributes and was checked by
hand where a site sat near a boundary.

In the tables, `T` is the type inside the `Arc`. Sites in one file that share
type, kind and purpose are merged into one row with all their line numbers.

## 1. Weak references

Every `Weak` in the product code is `std::sync::Weak`. There is no
`std::rc::Weak`, no `Arc<dyn Trait>` is ever downgraded, and no generic code
downgrades an `Arc<T>` for an unknown `T`.

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-client/src/daemon/connector.rs:8, 16, 29, 41` | `use std::sync::{Arc, Weak};` (+3 more) | `mj_client::daemon::connector::DaemonConnector` | prod | Process-wide `static CONNECTOR: Mutex<Weak<..>>`. `shared()` returns the live connector if any caller still holds one, else loads placement again and builds a new one. The connector (and its ssh lease) dies with its last holder. |
| `mj-client/src/daemon/ssh.rs:331, 366, 370` | `let old_lease = std::sync::Arc::downgrade(connected.ssh_lease.as_ref().unwrap…` (+2 more) | `mj_client::daemon::ssh::SshLease` | test | Test (#1514) holds a Weak to the first client's lease and asserts it is alive while any client holds it and dead once both clients are dropped. The lease owns the ssh tunnel child, so "dead" means the tunnel was torn down. |
| `mj-controller/src/controller/worktree.rs:731, 732, 744, 746, 750` | `use std::sync::{Arc, Mutex, OnceLock, Weak};` (+4 more) | `std::sync::Mutex<()>` | prod | Function-local registry `LOCKS: BTreeMap<String, Weak<Mutex<()>>>` in `source_fetch_lock`. Entries whose lock nobody holds are pruned (`Weak::strong_count() > 0`), a live one is reused (`upgrade`), else a new lock is registered. The count is what makes two fetches of one repository share a lock. |
| `mj-core/src/jev.rs:8, 88, 106, 139, 336` | `use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};` (+4 more) | `mj_core::jev::Writer` | prod | Process-wide registry `HashMap<PathBuf, Weak<Writer>>`: one writer thread per directory while any `DecisionLog` (a `Clone` newtype over `Arc<Writer>`) is alive. Dropping the last one drops `tx`, which ends the writer thread; `read` (line 336) routes through the live writer when one exists so reads are ordered after writes, and reads the files directly otherwise. |
| `mj-daemon/src/daemon.rs:269` | `std::sync::OnceLock<std::sync::Weak<crate::server_runtime::api::ApiBackend>>,` | `mj_daemon::server_runtime::api::ApiBackend` | prod | Field `RuntimeState.report_backend: OnceLock<Weak<ApiBackend>>`, "weak so the backend's ExportRuntime reference does not form an Arc cycle". |
| `mj-daemon/src/daemon.rs:290` | `startup_delivery_gates: Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::M…` | `tokio::sync::Mutex<()>` | prod | Field `RuntimeState.startup_delivery_gates`. |
| `mj-daemon/src/daemon/delegation/policy.rs:170, 172, 176, 178` | `let weak_state = Arc::downgrade(&state);` (+3 more) | `mj_daemon::daemon::RuntimeState` | prod | Profile-catalog publisher and notice callbacks hold a Weak to the daemon state; the state holds the catalog (`state.profile_catalog`), so a strong reference would be a cycle. |
| `mj-daemon/src/daemon/feed.rs:1193, 1195` | `let weak = Arc::downgrade(&state);` (+1 more) | `mj_daemon::daemon::RuntimeState` | test | Test builds the same publisher callback with a Weak. |
| `mj-daemon/src/daemon/lifecycle.rs:409, 573` | `state: Arc::downgrade(self),` (+1 more) | `mj_daemon::daemon::RuntimeState` | prod | `LifecycleHold.state`: the upgrade gate's describer reads the lifecycle registry through a Weak so a registered hold does not keep the daemon state alive. |
| `mj-daemon/src/daemon/lifecycle.rs:1124, 1134` | `state: std::sync::Weak<RuntimeState>,` (+1 more) | `mj_daemon::daemon::RuntimeState` | prod | `LifecycleHold::describe`: upgrade failure yields an empty stage list. |
| `mj-daemon/src/daemon/process.rs:202, 207, 217` | `let failure_runtime = Arc::downgrade(&state);` (+2 more) | `mj_daemon::daemon::RuntimeState` | prod | `failure_runtime` Weak inside the harness-failure handler, and the `reviewer_runtime` OnceLock. Upgrade failure returns the error "daemon stopped before readiness failure could be applied". Inferred: the handler is stored in a manager the state owns, so a strong reference would be a cycle. |
| `mj-daemon/src/daemon/process.rs:826` | `if let Some(backend) = state.report_backend.get().and_then(std::sync::Weak::u…` | `mj_daemon::server_runtime::api::ApiBackend` | prod | `report_backend` Weak upgraded per use; nothing happens once the backend is gone. |
| `mj-daemon/src/daemon/state.rs:457, 458, 462` | `gates.retain(\|_, gate\| gate.strong_count() != 0);` (+2 more) | `tokio::sync::Mutex<()>` | prod | `startup_delivery_gates: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>`: per-session gate that exists only while somebody holds or waits for it; dead entries pruned with `Weak::strong_count`. |
| `mj-daemon/src/daemon/subagent_park.rs:21, 29, 33` | `self.report_backend.set(Arc::downgrade(backend)).is_ok(),` (+2 more) | `mj_daemon::server_runtime::api::ApiBackend` | prod | Installs and upgrades `report_backend`. |
| `mj-daemon/src/daemon/tests.rs:11022` | `state.reviewer_runtime.set(Arc::downgrade(&state)).unwrap();` | `mj_daemon::daemon::RuntimeState` | test | Test installs `reviewer_runtime` as production does. |
| `mj-daemon/src/github_watch/service.rs:95, 98` | `let notice_state = Arc::downgrade(&state);` (+1 more) | `mj_daemon::daemon::RuntimeState` | prod | Notice callback holds a Weak so the notices object does not keep the daemon state alive; notices are dropped once the state is gone. |
| `mj-daemon/src/review_host/controller_environment.rs:28` | `Arc<std::sync::OnceLock<std::sync::Weak<crate::daemon::RuntimeState>>>;` | `mj_daemon::daemon::RuntimeState` | prod | Type alias `SharedReviewerRuntime = Arc<OnceLock<Weak<RuntimeState>>>`. |
| `mj-daemon/src/review_host/controller_environment.rs:38` | `.upgrade()` | `mj_daemon::server_runtime::api::ApiBackend` | prod | Upgrade failure is the user-visible error "reviewer profile service stopped". |
| `mj-daemon/src/review_host/controller_environment.rs:53` | `.upgrade()` | `mj_daemon::daemon::RuntimeState` | prod | Upgrade failure is the user-visible error "reviewer runtime stopped". |
| `mj-github-relevance/src/github_student.rs:309, 322` | `State::Ready(ready) => (ready.permits.clone(), Arc::downgrade(&ready.engine)),` (+1 more) | `mj_github_relevance::github_student::inference::Engine` | prod | A queued request remembers the engine "epoch" as a Weak (so it does not keep model memory alive while it waits) and is refused if the engine was replaced: `Weak::ptr_eq`. |
| `mj-github-relevance/src/github_student.rs:347, 369` | `let epoch = Arc::downgrade(&engine);` (+1 more) | `mj_github_relevance::github_student::inference::Engine` | prod | After a failed blocking task (panic), the result is recorded only if the engine is still alive (`upgrade`); the task's own strong reference was dropped during unwind. |
| `mj-profile-catalog/src/profile_catalog.rs:908, 922, 934, 949` | `let catalog = Arc::downgrade(self);` (+3 more) | `mj_profile_catalog::profile_catalog::ProfileCatalog` | prod | The refresh loop and the home watch hold a Weak and return when the catalog is gone, so the background tasks do not keep the catalog alive. |
| `mj-profile-catalog/src/profile_config.rs:4, 239, 245, 280, 283, 285` | `use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};` (+5 more) | `mj_profile_catalog::profile_config::ProbeLock` | prod | Registry `PROBES: BTreeMap<String, Weak<ProbeLock>>`: one gate and cancellation token per profile while a probe holds or waits for it; `cancel_all` cancels the live ones; dead entries pruned with `Weak::strong_count`. |
| `mj-session-runtime/src/worker_client/credential_sync.rs:210, 233, 256, 260` | `type SessionLocks = BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;` (+3 more) | `tokio::sync::Mutex<()>` | prod | `SessionSteps`: `BTreeMap<String, Weak<tokio::sync::Mutex<()>>>`. `enter` reuses a live lock or registers a new one; `SessionTurn::drop` releases its guard and its strong reference, then removes the map entry if `Weak::strong_count() == 0` ("a waiter holds a strong reference, taken under this mutex"). A test-support accessor `tracked()` exposes the entry count. |
| `mj-targets/src/image_pull_gate.rs:44, 51, 65, 72` | `static LOCKS: std::sync::OnceLock<Mutex<BTreeMap<String, std::sync::Weak<Imag…` (+3 more) | `mj_targets::image_pull_gate::ImagePullLock` | prod | Registry `LOCKS: BTreeMap<String, Weak<ImagePullLock>>`: one download lock per host and image while anyone holds it; dead entries pruned. |
| `mj-worker-staging/src/worker_binary/store.rs:5, 43, 50, 51, 58` | `use std::sync::{Arc, Mutex, MutexGuard, Weak};` (+4 more) | `mj_worker_staging::worker_binary::store::StoreGate` | prod | Registry `GATES: HashMap<String, Weak<StoreGate>>`: one gate per store root while anyone holds it; dead entries pruned. |
| `mj-worker/src/acp/drive.rs:761` | `let owner = create_terminal_owner.as_ref().map(\|owner\| owner.upgrade().ok_or_…` | `mj_worker::relay::RelayCell` | prod | `terminal/create` upgrades the owner before spawning; failure is the error "the relay owner is gone; refusing to spawn an unregistered terminal". |
| `mj-worker/src/acp/goal.rs:6, 12` | `owner: Option<&std::sync::Weak<crate::relay::RelayCell>>,` (+1 more) | `mj_worker::relay::RelayCell` | prod | `record_resume_sent`: upgrade failure is the error "native resume relay owner disappeared". |
| `mj-worker/src/acp/goal.rs:668` | `terminal_owner: track_cut.then(\|\| Arc::downgrade(&relay)),` | `mj_worker::relay::RelayCell` | test | Test builds a launch spec with a weak terminal owner. |
| `mj-worker/src/acp/launch.rs:192` | `pub terminal_owner: Option<std::sync::Weak<crate::relay::RelayCell>>,` | `mj_worker::relay::RelayCell` | prod | Field `LaunchSpec.terminal_owner: Option<Weak<RelayCell>>`: "primary workers admit terminals and native resumes through the cut owner". |
| `mj-worker/src/acp/operator_away.rs:219, 518, 532, 555, 564` | `responder: PendingResponder::CommandLoop(Arc::downgrade(&lifetime)),` (+4 more) | `()` | prod | A command-loop question owns an `Arc<()>` "lifetime" (`CommandLoopQuestion.lifetime`, created in `register_command_loop`, line 208); the pending entry holds only a `Weak<()>`. `PendingResponder::is_alive` is `upgrade().is_some()`, and the question's `Drop` removes exactly its own entry by `Weak::ptr_eq`. |
| `mj-worker/src/acp/tests.rs:5430, 5436, 5466, 5482, 5600, 5659, 5718, 9203` | `terminal_owner: Option<std::sync::Weak<crate::relay::RelayCell>>,` (+7 more) | `mj_worker::relay::RelayCell` | test | Tests pass a weak owner to the scripted runtime; `a_disappeared_terminal_owner_refuses_spawn` drops the only `Arc<RelayCell>` first and expects "relay owner is gone". |
| `mj-worker/src/worker_runtime/unix.rs:396` | `terminal_owner: Some(Arc::downgrade(&relay)),` | `mj_worker::relay::RelayCell` | prod | The worker runtime hands its relay to the ACP launch spec as a Weak. |

The uses fall into three shapes, and the Go code needs the count for each:

1. **Registries of things that exist only while somebody holds them**
   (`worktree.rs` fetch locks, `image_pull_gate.rs`, `worker_binary/store.rs`,
   `profile_config.rs` probe locks, `state.rs` startup delivery gates,
   `credential_sync.rs` session locks, `jev.rs` writers, `connector.rs`). A map
   holds `Weak<T>`; a caller upgrades or creates; entries are pruned with
   `weak.strong_count() == 0`. Whether two callers share one lock, and when an
   entry disappears, is decided by the strong count. Every holder of the
   returned `Arc<T>` must therefore be an `rs.Arc[T]` that is dropped where
   Rust drops it. A holder that forgets its `Drop()` keeps the entry alive for
   ever, which here is a leak, not a wrong answer; a holder that drops early
   lets a second caller create a second lock for the same key, which breaks
   the mutual exclusion the registry exists for.
2. **Back references that must not keep their target alive**
   (`Weak<RuntimeState>`, `Weak<ApiBackend>`, `Weak<ProfileCatalog>`,
   `Weak<RelayCell>`). The upgrade fails once the daemon state, the API
   backend, the catalog or the worker's relay has been dropped, and the code
   then returns an error text, skips the work or ends a background loop. For
   `RelayCell` a test drops the only `Arc` and expects the refusal
   (`mj-worker/src/acp/tests.rs:5712-5729`).
3. **An identity that must not keep its target alive**
   (`github_student.rs`): a queued inference request remembers which engine it
   queued for as a `Weak` and compares it with `Weak::ptr_eq`; a failed task
   records its result only if `upgrade()` still finds the engine. The same
   shape with a unit value is in `mj-worker/src/acp/operator_away.rs`: a
   command-loop question owns an `Arc<()>`, its pending entry holds the
   `Weak<()>`, the entry counts as alive while the `Weak` upgrades, and the
   question's `Drop` removes its own entry by `Weak::ptr_eq`.

## 2. Counts

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-cli/src/dashboard/summary_reads.rs:175` | `while Arc::strong_count(&reads.permits) > 2 {` | `tokio::sync::Semaphore` | test | Test waits until only the owner and one held permit keep the semaphore alive: every queued read task must have dropped its `Arc::clone(&self.permits)` (lines 71, 77) when cancelled. `acquire_many_owned` keeps one clone inside the permit. |
| `mj-session-runtime/src/worker_client/credential_sync.rs:461` | `profiles.retain(\|profile_id, profile\| Arc::strong_count(profile) > 1` | `mj_session_runtime::worker_client::credential_sync::ProfileLogin` | prod | The sync loop keeps a profile's login owner while an in-flight pass still holds a clone (`strong_count > 1`) or a target still names the profile. |
| `mj-targets/src/aws_assume_role.rs:280` | `if Arc::strong_count(cell) > 1 {` | `std::sync::Mutex<Option<mj_targets::aws_assume_role::ScopedEntry>>` | prod | `sweep`: an expired or empty cache cell is removed only if nobody else holds it; "removing it would let that caller mint into a cell the map no longer knows". Runs under the map lock. |
| `mj-worker/src/worker_runtime/subagents/handback_tests.rs:117, 119, 133, 135` | `let retry_gate_refs = Arc::strong_count(&endpoint.handback_gate);` (+3 more) | `tokio::sync::Mutex<()>` | test | Test waits until a spawned call has cloned `endpoint.handback_gate` (the count rises), which is how it knows the call reached the gate. |

`Arc::weak_count` is not used.

Consequences:

- `summary_reads.rs`: the field is `permits: Arc<tokio::sync::Semaphore>` and
  the count includes the clone that tokio keeps inside an
  `OwnedSemaphorePermit` (`reads.permits.clone().acquire_many_owned(4)`, line
  141). The tokio package's `AcquireOwned`/`AcquireManyOwned` must therefore
  take and keep an `rs.Arc[tokio.SyncSemaphore]` clone, and release it when the
  permit is dropped, for this test to see `2`.
- `handback_tests.rs`: the test only needs the count to rise when a spawned
  call clones `endpoint.handback_gate` (`mj-worker/src/worker_runtime/subagents.rs:3298`).
  The field (`subagents.rs:982`) must be an `rs.Arc[tokio.SyncMutex[rs.Unit]]`
  and line 3298 a counted `Clone()`.
- `credential_sync.rs:461` and `aws_assume_role.rs:280` are production
  decisions made from the count.

## 3. Taking the value back

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-chat/src/chat/transcript.rs:387` | `let entry = std::sync::Arc::unwrap_or_clone(entry);` | `mj_client::web::BrowserTranscriptEntry` | prod | `browser_tail`: take each freshly built entry by value. |
| `mj-chat/src/chat/transcript/tests.rs:2475` | `.flat_map(\|entry\| std::sync::Arc::unwrap_or_clone(entry).lines)` | `mj_client::web::BrowserTranscriptEntry` | test | Test takes entries by value. |
| `mj-core/src/snapshot_map.rs:42` | `Arc::unwrap_or_clone(self.0)` | `generic T of mj_core::snapshot_map::SharedSnapshot<T>` | prod | `into_inner`: take the value, cloning if shared. |
| `mj-core/src/snapshot_map.rs:124, 136, 181, 310` | `.map(Arc::unwrap_or_clone)` (+3 more) | `generic V of mj_core::snapshot_map::SnapshotMap<K, V>` | prod | `insert`, `remove`, `into_values`, `into_iter`: hand the value back, cloning if another snapshot still shares it. |
| `mj-session-runtime/src/worker_client/credential_sync.rs:2503` | `Arc::try_unwrap(login).unwrap_or_else(\|shared\| shared.as_ref().clone());` | `mj_session_runtime::worker_client::credential_sync::CanonicalLogin` | prod | Take the login by value to set `read_under_lock`, cloning if shared. |
| `mj-transcript/src/projection/materialize.rs:344` | `Ok(match Arc::try_unwrap(item) {` | `mj_core::transcript::TranscriptItem` | prod | Convert an item to its canonical form by move when unshared, by reference when shared. Both branches build the same value. |

None of these needs a count. `Arc::unwrap_or_clone(a)` and
`Arc::try_unwrap(a).unwrap_or_else(|shared| (*shared).clone())` return the same
value whether or not the `Arc` was shared; only the copy is saved. With a plain
pointer the port cannot know that it is the only holder, so it must always
take the shared path: `v := rs.Clone(*a)`.

`materialize.rs:344` has two different branches (`canonical_item_owned(item)`
when unshared, `canonical_item_shared(&shared)` otherwise). They were compared
arm by arm (`materialize.rs:387-436` and `438-487`): each arm builds the same
`CanonicalTranscriptItem`, one by moving fields and one by cloning them. The
port takes the `Err(shared)` branch always. Do not translate the `Ok` branch as
"use the fields without copying": other holders of the same pointer may exist.

## 4. `Arc::get_mut`

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-client/src/transcript.rs:1238` | `let entry = Arc::get_mut(entry).expect("unpublished browser row");` | `mj_client::web::BrowserTranscriptEntry` | prod | Rows were built by `entries.into_iter().map(Arc::new)` (line 181) in the call just above and are published only after this loop. |
| `mj-daemon/src/daemon/auto_afk.rs:234, 318` | `Arc::get_mut(&mut state).unwrap().committed =` (+1 more) | `mj_daemon::daemon::RuntimeState` | test | Test replaces a field of a `RuntimeState` it has just built. |
| `mj-daemon/src/daemon/serve.rs:981` | `Arc::get_mut(&mut state).unwrap().wiki = mj_controller::sessionwiki::WikiInde…` | `mj_daemon::daemon::RuntimeState` | test | Test replaces a field of a `RuntimeState` it has just built. |
| `mj-daemon/src/daemon/support.rs:389` | `Arc::get_mut(&mut state).unwrap().committed =` | `mj_daemon::daemon::RuntimeState` | test | Test replaces a field of a `RuntimeState` it has just built. |
| `mj-daemon/src/daemon/tests.rs:763, 4490, 6828` | `Arc::get_mut(&mut state).unwrap().committed =` (+2 more) | `mj_daemon::daemon::RuntimeState` | test | Test replaces a field of a `RuntimeState` it has just built. |
| `mj-profile-catalog/src/profile_catalog.rs:302` | `Arc::get_mut(&mut catalog).unwrap().config_path = Some(config_path);` | `mj_profile_catalog::profile_catalog::ProfileCatalog` | prod | `new_with_config_path`: `Self::new` (line 280) is a bare `Arc::new`, nothing else holds it yet. |
| `mj-profile-catalog/src/profile_catalog.rs:1927` | `Arc::get_mut(&mut catalog).unwrap().probe = Some((probe, model_probe));` | `mj_profile_catalog::profile_catalog::ProfileCatalog` | test-support | Test-support constructor `build`: sets `probe` on the Arc `Self::new` just returned. |
| `mj-profile-catalog/src/profile_catalog.rs:3097, 3155, 3201, 3364, 3409, 3850, 3980, 4115, 4154` | `Arc::get_mut(&mut catalog).unwrap().home_edit_retry_wait = Duration::from_mil…` (+8 more) | `mj_profile_catalog::profile_catalog::ProfileCatalog` | test | Test sets a field on the catalog a constructor just returned. |
| `mj-transcript/src/projection.rs:426` | `if let Some(owned) = Arc::get_mut(existing) {` | `mj_core::transcript::TranscriptItem` | prod | Upsert of an existing item: mutate in place if no published snapshot or index shares it, else publish a fresh Arc "so snapshots taken earlier keep the value they were given". |

`Arc::get_mut` returns `Some` only when there is no other strong reference
and no `Weak`. Classification is in section 9 (c).

## 5. `Arc::make_mut`

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-chat/src/chat/active/tests.rs:4257, 4383` | `Arc::make_mut(&mut item).body = mj_core::transcript::TranscriptBody::User {` (+1 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-chat/src/chat/tests.rs:1183` | `Arc::make_mut(&mut session.transcript[0]).body = TranscriptBody::User {` | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-chat/src/chat/transcript/tests.rs:1862, 3938, 3961` | `let streaming = Arc::make_mut(&mut session.transcript[5]);` (+2 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-client/src/transcript.rs:1380, 1415, 1441, 1512, 1574, 1689, 1711` | `let TranscriptBody::Message { event } = &mut Arc::make_mut(&mut session.trans…` (+6 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-controller/src/controller/container_upgrade.rs:401` | `.and_then(\|r\| std::sync::Arc::make_mut(r).container_settings.as_mut())` | `mj_core::state::target_runtime::TargetRuntimeSettings` | prod | Mutates one record's runtime settings; records interned to the same Arc must not change. |
| `mj-controller/src/controller/provisioning.rs:931, 1089` | `std::sync::Arc::make_mut(runtime).tool_cache = tool_cache;` (+1 more) | `mj_core::state::target_runtime::TargetRuntimeSettings` | prod | Mutates one record's runtime settings; records interned to the same Arc must not change. |
| `mj-controller/src/controller/resume.rs:454` | `let project = std::sync::Arc::make_mut(project);` | `mj_core::repository::ProjectBundleSnapshot` | prod | Works on a clone of the record's `Option<Arc<..>>` (count is at least 2), so Rust always copies here. |
| `mj-controller/src/controller/worktree.rs:919` | `std::sync::Arc::make_mut(project)` | `mj_core::repository::ProjectBundleSnapshot` | prod | Mutates one session record's project snapshot; other records interned to the same Arc must not see it. |
| `mj-core/src/relay/snapshot/apply.rs:75, 105` | `let stack = Arc::make_mut(&mut snapshot.notification_stack);` (+1 more) | `mj_core::attention::NotificationStack` | prod | Edit the notification stack of the snapshot being folded. |
| `mj-core/src/relay/snapshot/apply.rs:155` | `Arc::make_mut(assessment).question_message = target.map(str::to_owned);` | `mj_core::assessment::TurnAssessment` | prod | Edit the assessment of the snapshot being folded. |
| `mj-core/src/relay/snapshot/apply.rs:449` | `let stack = Arc::make_mut(&mut snapshot.notification_stack);` | `mj_core::attention::NotificationStack` | prod | Edit the notification stack. |
| `mj-core/src/relay/snapshot/apply.rs:713, 720` | `Arc::make_mut(context).set_aside_replies(false);` (+1 more) | `mj_core::assessment::ContextHistory` | prod | Edit the assessment context history. |
| `mj-core/src/relay/snapshot/apply.rs:731` | `let stack = Arc::make_mut(&mut snapshot.notification_stack);` | `mj_core::attention::NotificationStack` | prod | Edit the notification stack. |
| `mj-core/src/relay/snapshot/apply.rs:752` | `let a = Arc::make_mut(a);` | `mj_core::assessment::TurnAssessment` | prod | Edit the assessment. |
| `mj-core/src/relay/snapshot/apply.rs:2353, 2383` | `Arc::make_mut(context).user(id, &prompt);` (+1 more) | `mj_core::assessment::ContextHistory` | prod | Append a user prompt to / set aside replies in the context history. |
| `mj-core/src/relay/snapshot/apply.rs:2396` | `Arc::make_mut(context).assistant(` | `mj_core::assessment::ContextHistory` | prod | Append assistant text to the context history: runs for every streamed agent text chunk. |
| `mj-core/src/relay/snapshot/apply.rs:2460, 2469` | `Arc::make_mut(context).open_assistant_id = None;` (+1 more) | `mj_core::assessment::ContextHistory` | prod | Close the open assistant message in the context history. |
| `mj-core/src/relay/snapshot/apply.rs:2496, 2534` | `let a = std::sync::Arc::make_mut(a);` (+1 more) | `mj_core::assessment::TurnAssessment` | prod | Edit the assessment. |
| `mj-core/src/snapshot_map.rs:63` | `Arc::make_mut(&mut self.0)` | `generic T of mj_core::snapshot_map::SharedSnapshot<T>` | prod | `DerefMut for SharedSnapshot<T>`: EVERY mutable access to a snapshot goes through this `make_mut`. It is one line of source and an unknown number of implicit call sites. |
| `mj-core/src/snapshot_map.rs:111` | `self.0.get_mut(key).map(Arc::make_mut)` | `generic V of mj_core::snapshot_map::SnapshotMap<K, V>` | prod | `SnapshotMap::get_mut` (also behind `IndexMut`, `entry().or_insert_with`, `first_value_mut`): copy the record if a published snapshot shares it. |
| `mj-core/src/state/tests.rs:186` | `&mut Arc::make_mut(&mut pending.transcript[1]).body` | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-daemon/src/controller_tests/move_session.rs:63` | `std::sync::Arc::make_mut(source.target_runtime.get_or_insert_with(\|\| {` | `mj_core::state::target_runtime::TargetRuntimeSettings` | test | Test edits a record's runtime settings. |
| `mj-daemon/src/daemon/continuation/tests.rs:1762, 2131, 2753` | `let assessment = std::sync::Arc::make_mut(` (+2 more) | `mj_core::assessment::TurnAssessment` | test | Test edits a snapshot's assessment in place. |
| `mj-daemon/src/server_runtime/api/tests.rs:7594, 7659` | `let assessment = std::sync::Arc::make_mut(snapshot.operational.assessment.as_…` (+1 more) | `mj_core::assessment::TurnAssessment` | test | Test edits a snapshot's assessment in place. |
| `mj-daemon/src/server_runtime/snapshot.rs:336, 343, 357, 364, 371, 376, 383, 393, 433` | `std::sync::Arc::make_mut(&mut commands).push(command(` (+8 more) | `Vec<mj_server::server::viewer_types::ViewerMjCommand>` | prod | `commands` is a clone of a cached `Arc<Vec<..>>` (static command list); pushes must not reach the cached list. First push copies, later pushes are in place. |
| `mj-session-runtime/src/session_manager/model_policy.rs:33` | `let Some(option) = std::sync::Arc::make_mut(&mut state.config_options)` | `Vec<agent_client_protocol::v1::schema::SessionConfigOption>` | prod | Filter the model option of an operational state. |
| `mj-store/src/database/materialized.rs:837` | `} = &mut Arc::make_mut(item).body` | `mj_core::transcript::TranscriptItem` | prod | `hydrate_terminal_outputs`: fill terminal output into a loaded tool item. |
| `mj-store/src/database/tests.rs:2108, 6659, 6952, 6957, 6963, 7007, 7056, 7277, 7396` | `} = &mut Arc::make_mut(item).body` (+8 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-store/src/database/tests.rs:7544` | `std::sync::Arc::make_mut(record.target_runtime.as_mut().unwrap()).tool_cache =` | `mj_core::state::target_runtime::TargetRuntimeSettings` | test | Test edits a record's runtime settings. |
| `mj-store/src/database/tests.rs:8455, 8563` | `} = &mut Arc::make_mut(&mut owner).body` (+1 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-transcript/src/github_item.rs:273` | `Arc::make_mut(session.transcript.last_mut().unwrap()).stable_id =` | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-tui/src/ingest/tests.rs:364, 365, 559, 605` | `Arc::make_mut(&mut updated).latest_content_event_ordinal = Some(2);` (+3 more) | `mj_core::transcript::TranscriptItem` | test | Test edits a transcript item. |
| `mj-tui/src/resume.rs:1434` | `let profiles = Arc::make_mut(&mut dialog.profiles);` | `Vec<mj_tui::dialogs::ImportProfileOption>` | prod | Resume dialog state shared with render snapshots. |
| `mj-tui/src/resume.rs:1566` | `Arc::make_mut(&mut dialog.previews).insert(wiki_id, markdown);` | `BTreeMap<String, String>` | prod | Resume dialog previews. |
| `mj-tui/src/resume.rs:1586` | `Arc::make_mut(&mut dialog.hits).insert(key, transcript.unwrap_or_default());` | `BTreeMap<(String, String), mj_client::daemon::WikiHitTranscript>` | prod | Resume dialog hits. |
| `mj-tui/src/test_support.rs:748` | `Arc::make_mut(&mut item).stable_id = format!(` | `mj_core::transcript::TranscriptItem` | test-support | Test-support builder edits a transcript item. |
| `mj-worker/src/relay.rs:2610` | `Arc::make_mut(&mut snapshot.notification_stack).entries[index].completed_at_m…` | `mj_core::attention::NotificationStack` | prod | `seed_missing_notification_time`: backfill one entry's time on load. |
| `mj-worker/src/relay/capacity_tests.rs:405` | `let stored = std::sync::Arc::make_mut(relay.snapshot.assessment.as_mut().unwr…` | `mj_core::assessment::TurnAssessment` | test | Test edits the stored assessment. |
| `mj-worker/src/relay/snapshot_tests.rs:277` | `std::sync::Arc::make_mut(&mut next.available_commands).clear();` | `Vec<agent_client_protocol::v1::schema::AvailableCommand>` | test | Test then edits the shared list to show the copy is independent. |

Classification and the cost notes are in section 9 (b).

## 6. `Arc::ptr_eq`

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-chat/src/chat/active.rs:728` | `Arc::ptr_eq(&self.instance.0, &instance.0)` | `()` | prod | `ChatInstance(Arc<()>)`: identity of one owned chat view, "independent of the session it displays"; a new token is minted on reset (active/io.rs:368). |
| `mj-cli/src/dashboard/drains.rs:47` | `if Arc::ptr_eq(&entry.get().cancelled, &update.operation) =>` | `std::sync::atomic::AtomicBool` | prod | Identity of one lifecycle operation: its cancellation flag. A completion is "current" only if it carries the same flag as the registered operation. |
| `mj-cli/src/dashboard/drains.rs:1051` | `assert!(Arc::ptr_eq(&operations["session"].cancelled, &current));` | `std::sync::atomic::AtomicBool` | test | Identity of one lifecycle operation: its cancellation flag. A completion is "current" only if it carries the same flag as the registered operation. |
| `mj-cli/src/dashboard/io.rs:1090, 1846` | `if self.lifecycle_operations.get(&session_id).is_some_and(\|current\|Arc::ptr_e…` (+1 more) | `std::sync::atomic::AtomicBool` | prod | Same identity test on the operation's cancellation flag. |
| `mj-client/src/runtime_feed.rs:1493` | `Arc::ptr_eq(before, now),` | `mj_core::transcript::TranscriptItem` | test | Test: after every feed step, an item whose content did not change is the same allocation as before ("unchanged item kept its pointer"). |
| `mj-client/src/transcript.rs:1496, 1517` | `assert!(Arc::ptr_eq(entry, &projector.published[&entry.identity()]));` (+1 more) | `mj_client::web::BrowserTranscriptEntry` | test | Test: the projector publishes the very Arc it returned. |
| `mj-core/src/snapshot_map.rs:36` | `Arc::ptr_eq(&self.0, &other.0)` | `generic T of mj_core::snapshot_map::SharedSnapshot<T>` | prod | `SharedSnapshot::ptr_eq`, and `PartialEq` using it as a shortcut before the deep comparison. |
| `mj-core/src/snapshot_map.rs:217` | `if !self.0.get(key).is_some_and(\|held\| Arc::ptr_eq(held, value)) {` | `generic V of mj_core::snapshot_map::SnapshotMap<K, V>` | prod | `copy_entry`: skip the insert when the entry already shares `source`'s Arc, so the tree path stays shared. |
| `mj-core/src/targets.rs:166` | `Arc::ptr_eq(&self.0, &other.0)` | dyn Fn(Option<&[u8]>) + Send + Sync (`StreamReplyObserver`) | prod | `PartialEq for StreamReply`: two command specs are equal only if they carry the very same observer closure. |
| `mj-core/src/targets.rs:204` | `Arc::ptr_eq(&self.0, &other.0)` | `dyn Fn(CommandActivity) + Send + Sync` | prod | `PartialEq for CommandObserver`: same closure object. |
| `mj-core/src/transcript.rs:757` | `.is_some_and(\|source\| Arc::ptr_eq(source, item))` | `mj_core::transcript::TranscriptItem` | prod | `TranscriptSource::is`: a chat entry remembers the item Arc it was rendered from; pointer equality replaces re-reading and re-parsing the item. Requires that a changed item is a NEW allocation. |
| `mj-core/src/worker_build.rs:551` | `assert!(Arc::ptr_eq(&original, &executable_contents(&path).unwrap()));` | `mj_core::worker_build::ExecutableContents` | test | Test (unix): two reads of an unchanged executable return the memoized Arc. |
| `mj-daemon/src/daemon/lifecycle.rs:315` | `.is_some_and(\|front\| Arc::ptr_eq(front, &ticket));` | `()` | prod | Lifecycle intent ticket: `VecDeque<Arc<()>>` per session; the caller proceeds when its ticket is at the front. |
| `mj-daemon/src/daemon/lifecycle.rs:1169` | `queue.retain(\|ticket\| !Arc::ptr_eq(ticket, &self.ticket));` | `()` | prod | Drop of the intent guard removes its own ticket from the queue by identity. |
| `mj-daemon/src/daemon/owner.rs:191, 202` | `Arc::ptr_eq(&self.pollable_worker_inputs(), inputs) && self.close_requested =…` (+1 more) | `mj_daemon::daemon::owner::PollableWorkerInputs` | prod | Same identity test as process.rs; `pollable_worker_inputs()` returns the cached Arc while its inputs are unchanged. Tests assert the cached Arc is reused or replaced. |
| `mj-daemon/src/daemon/owner.rs:782, 805, 819, 832, 935` | `assert!(Arc::ptr_eq(&inputs, &owner.pollable_worker_inputs()));` (+4 more) | `mj_daemon::daemon::owner::PollableWorkerInputs` | test | Same identity test as process.rs; `pollable_worker_inputs()` returns the cached Arc while its inputs are unchanged. Tests assert the cached Arc is reused or replaced. |
| `mj-daemon/src/daemon/process.rs:1331, 1351, 1380, 1404` | `.is_some_and(\|current\| Arc::ptr_eq(current, &inputs))` (+3 more) | `mj_daemon::daemon::owner::PollableWorkerInputs` | prod | Identity of the published inputs snapshot: "is this still the publication I prepared/staged/installed for?". Pointer equality stands in for a deep comparison of ids, config and records. |
| `mj-daemon/src/daemon/state.rs:1140, 1175` | `.is_some_and(\|queue\| Arc::ptr_eq(&queue.identity, &identity))` (+1 more) | `()` | prod | Identity token of one startup-prompt drain (`StartupPromptQueue.identity`, daemon.rs:562): "is the queue still the one this drain created?". |
| `mj-daemon/src/server_runtime/project_sources.rs:200` | `if entry.key != resolved.key \|\| !Arc::ptr_eq(&entry.cancelled, &resolved.canc…` | `std::sync::atomic::AtomicBool` | prod | Identity of one resolution attempt: a result is applied only if its cancellation flag is the entry's current one. |
| `mj-daemon/src/server_runtime/tests.rs:55` | `assert!(Arc::ptr_eq(` | `std::sync::atomic::AtomicBool` | test | Test: a web action and its lifecycle control share one cancellation flag. |
| `mj-github-relevance/src/github_student.rs:161` | `if !matches!(&machine.state, State::Ready(ready) if Arc::ptr_eq(&ready.engine…` | `mj_github_relevance::github_student::inference::Engine` | prod | A completion is applied only if the machine still runs the engine it was computed on. |
| `mj-github-relevance/src/github_student.rs:451, 452, 507, 508, 577, 578, 685, 686` | `&& Arc::ptr_eq(&ready.engine, engine)` (+7 more) | `mj_github_relevance::github_student::inference::Engine, mj_github_relevance::github_student::inference::Gpu, tokio_util::sync::CancellationToken` | prod | Engine, GPU session and preparation request are compared by identity before a state transition is applied (line 686: `Device::Preparing(Arc<CancellationToken>)`). |
| `mj-github-relevance/src/github_student/tests.rs:309, 337, 379, 400, 403` | `assert!(Arc::ptr_eq(&engine, &ready.engine));` (+4 more) | `mj_github_relevance::github_student::inference::Engine / mj_github_relevance::github_student::inference::Gpu` | test | Tests assert the machine kept or replaced the engine and GPU objects. |
| `mj-server/src/server/api/wait.rs:101, 129` | `&& (Arc::ptr_eq(&decided.session, &session) \|\| *decided.session == *session)` (+1 more) | `mj_server::server::viewer_types::ViewerSession` | prod | Shortcut before a deep comparison (`ptr_eq \|\| ==`). |
| `mj-session-runtime/src/session_manager/channels.rs:372, 475, 511` | `.is_some_and(\|current\| Arc::ptr_eq(current, &self.identity))` (+2 more) | `()` | prod | Identity token of one update producer in `BTreeMap<String, Arc<()>>`: a producer acts only while the registry still names its token. |
| `mj-store/src/database/committed.rs:398` | `.is_none_or(\|old\| !std::sync::Arc::ptr_eq(old, value))` | mj_core::repository::ProjectBundleSnapshot and mj_core::state::target_runtime::TargetRuntimeSettings (generic `fresh<T>`) | prod | Did the reloaded record get a new shared value, or the one the previous record already held? |
| `mj-store/src/database/committed.rs:675, 684` | `Arc::ptr_eq(&before, &owner.writer.committed_state().unwrap()),` (+1 more) | `mj_store::database::committed::CommittedState` | test | Test: an irrelevant or rolled-back write leaves the published committed state untouched. |
| `mj-store/src/database/committed.rs:701` | `assert!(Arc::ptr_eq(` | `mj_store::database::committed::CommittedTurn (a SnapshotMap value)` | test | Test: an unrelated turn keeps its shared record. |
| `mj-store/src/database/committed.rs:962` | `assert!(Arc::ptr_eq(&before, &after));` | `mj_store::database::committed::CommittedState` | test | Test: a no-op transaction does not republish. |
| `mj-store/src/database/tests.rs:7606, 7610` | `assert!(std::sync::Arc::ptr_eq(` (+1 more) | `mj_core::repository::ProjectBundleSnapshot / mj_core::state::target_runtime::TargetRuntimeSettings` | test | Test: a refreshed record reuses the interned project and runtime Arcs. |
| `mj-store/src/recovery_gate.rs:387` | `holders.retain(\|holder\| !Arc::ptr_eq(&holder.identity, identity));` | `()` | prod | Identity of one foreground hold. |
| `mj-store/src/recovery_gate.rs:458` | `holders.retain(\|holder\| !Arc::ptr_eq(&holder.cancelled, identity));` | `std::sync::atomic::AtomicBool` | prod | Identity of one recovery holder: its cancellation flag. |
| `mj-store/src/recovery_gate.rs:771, 802` | `.find(\|holder\| Arc::ptr_eq(&holder.identity, &admission.0.identity))` (+1 more) | `()` | prod | Identity of one admission. |
| `mj-store/src/recovery_gate.rs:816` | `.find(\|h\| Arc::ptr_eq(&h.cancelled, &a.0.cancelled))` | `std::sync::atomic::AtomicBool` | prod | Identity of one recovery holder: its cancellation flag. |
| `mj-store/src/recovery_gate.rs:859` | `.find(\|holder\| Arc::ptr_eq(&holder.identity, &admission.identity))` | `()` | prod | Identity of one admission. |
| `mj-targets/src/image_pull_gate.rs:247, 250, 253` | `assert!(Arc::ptr_eq(&first, &again));` (+2 more) | `mj_targets::image_pull_gate::ImagePullLock` | test | Test: same key gives the same lock, another image or host a different one. |
| `mj-tui/src/ingest.rs:168` | `.take_while(\|(cached, current)\| Arc::ptr_eq(cached, current))` | `mj_core::transcript::TranscriptItem` | prod | `unchanged_prefix`: how many leading transcript items the cache shares by pointer with the new transcript; only the rest is re-rendered. |
| `mj-worker/src/acp/guardian/reviewer.rs:225` | `.is_some_and(\|current\| Arc::ptr_eq(current, host))` | `mj_worker::acp::guardian::reviewer::ReviewerHost` | prod | `discard`: forget the reviewer host only if it is still the current one. |
| `mj-worker/src/relay/snapshot_tests.rs:265, 269, 273` | `assert!(std::sync::Arc::ptr_eq(` (+2 more) | `mj_core::assessment::TurnAssessment, Vec<acp AvailableCommand>, Vec<acp SessionConfigOption>` | test | Test: `share_unchanged_fields` makes a decoded state reuse the previous state's Arcs for equal fields. |

Classification is in section 9 (d).

## 7. `Rc`

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-controller/src/controller/backend/tests.rs:170, 172` | `fn virtual_clock() -> (std::rc::Rc<std::cell::Cell<Instant>>, Instant) {` (+1 more) | `std::cell::Cell<std::time::Instant>` | test | `Rc` shared between a test and its injected sleep hook. Plain sharing; no count or weak use. |
| `mj-tui/src/setup/tests/navigation.rs:416` | `struct Output(std::rc::Rc<RefCell<Vec<u8>>>);` | `std::cell::RefCell<Vec<u8>>` | test | `Rc` shared between a test and the terminal backend writer it hands over. Plain sharing. |

Both are test helpers that share one cell between the test body and a closure
or writer on the same thread. Neither counts, downgrades or unwraps. They are
plain pointers (`*rs.Cell[rstime.Instant]`, `*rs.RefCell[[]byte]`).

## 8. Look-alikes the greps match that are not `std::sync::Arc` or `Weak`

| Site | Code | `T` | Kind | What the code relies on |
| --- | --- | --- | --- | --- |
| `mj-controller/src/controller/environment.rs:52` | `.any(\|(entry, location)\| std::ptr::eq(entry, worker) && location == target),` | mj_controller::controller::environment::EnvironmentWorker (a `&` borrow, not an Arc) | prod | Look-alike: `std::ptr::eq` on two `&EnvironmentWorker` to check the borrow is an element of `self.workers`. Address identity of a slice element; in Go compare `&slice[i]` pointers. |
| `mj-controller/src/controller/session_runtime_tests.rs:465` | `handle.commands.same_channel(&same.commands),` | tokio::sync::mpsc::Sender (`handle.commands`) | test | Look-alike: `Sender::same_channel`. Owned by the tokio package. |
| `mj-core/src/snapshot_map.rs:35, 48` | `pub fn ptr_eq(&self, other: &Self) -> bool {` (+1 more) | `generic T of mj_core::snapshot_map::SharedSnapshot<T>` | prod | `SharedSnapshot::ptr_eq`, and `PartialEq` using it as a shortcut before the deep comparison. |
| `mj-daemon/src/daemon/attention.rs:1444, 1559` | `assert!(std::ptr::eq(` (+1 more) | mj_core::attention::SessionAttention inside `SnapshotMap<String, SessionAttention>` | test | Look-alike (test): `std::ptr::eq` on `map.get(k)` of two snapshots asserts an untouched record is the same allocation in both. The Go map type (`mjcore.SnapshotMap_SnapshotMap`) must return the stored `*V` from `Get`, not a copy. |
| `mj-daemon/src/daemon/capacity.rs:1614` | `assert!(projection.ptr_eq(&publisher.project(&state, &sessions.sessions)));` | `generic T of mj_core::snapshot_map::SharedSnapshot<T> (capacity projection)` | test | Test: `SharedSnapshot::ptr_eq`: 20 title edits must not republish the capacity projection. |
| `mj-daemon/src/daemon/feed.rs:1398, 1399, 1400, 1405, 1411, 1435` | `assert!(before.metadata.config.ptr_eq(&after.metadata.config));` (+5 more) | `generic T of mj_core::snapshot_map::SharedSnapshot<T> (Config, QuotaSnapshot, Vec<TargetStorageView>, ProfileCapabilitiesSnapshot, BTreeMap<String, RuntimeContainerCapacity>)` | test | Test: 20 republications keep every unchanged metadata section pointer-identical, and a measured change replaces only `storage`. |
| `mj-daemon/src/daemon/lifecycle.rs:1066, 1113` | `.retain(\|_, active\| !active.result.same_channel(&completed_channel));` (+1 more) | tokio::sync::watch::Receiver (`LifecycleWatch`, daemon.rs:783) | prod | Look-alike: `same_channel` on a tokio watch handle to find the lifecycle entry a completion belongs to. Owned by the tokio package. |
| `mj-daemon/src/daemon/views.rs:116` | `active.result.same_channel(&pending) && active.control.cancel_accepted()` | tokio::sync::watch::Receiver (`LifecycleWatch`, daemon.rs:783) | prod | Look-alike: `same_channel`. Owned by the tokio package. |
| `mj-daemon/src/server_runtime/snapshot.rs:658` | `.filter(\|weekly\| std::ptr::eq(*weekly, window))` | a quota window (`&` borrow) | prod | Look-alike: `std::ptr::eq` on two borrows to ask "is this window the master weekly window". Address identity of an element inside one snapshot. |
| `mj-github-relevance/src/github_relevance.rs:325` | `assert!(std::ptr::eq(received, &evidence));` | evidence (`&` borrow) | test | Look-alike (test): `std::ptr::eq`: the callback received the caller's own value. |
| `mj-import/src/compaction.rs:245` | `.is_some_and(\|candidate\| std::ptr::eq(candidate, prior))` | a prior summary (`&` borrow) | prod | Look-alike: `std::ptr::eq` on two borrows: "is the generic candidate the prior summary itself?". |
| `mj-profile-catalog/src/profile_catalog.rs:158` | `} => current.ptr_eq(attempt),` | futures Shared future (`Attempt = Shared<BoxFuture<..>>`) | prod | Look-alike: `futures::future::Shared::ptr_eq`: "does this catalog entry still own that probe attempt?". Owned by the futures package. |
| `mj-profile-catalog/src/profile_catalog.rs:1192` | `.is_some_and(\|current\| current.ptr_eq(&attempt))` | futures Shared future (`Attempt`) | prod | Look-alike: `Shared::ptr_eq`. Owned by the futures package. |
| `mj-profile-catalog/src/profile_catalog.rs:2437` | `assert!(held.ptr_eq(&catalog.snapshot()));` | `generic T of mj_core::snapshot_map::SharedSnapshot<T> (ProfileCapabilitiesSnapshot)` | test | Test: `SharedSnapshot::ptr_eq`: an options read does not republish the catalog snapshot. |
| `mj-session-runtime/src/session_manager/tests.rs:1883, 1887, 1892, 1907, 1911` | `assert!(held.ptr_eq(&published.operational));` (+4 more) | `generic T of mj_core::snapshot_map::SharedSnapshot<T> (RelayOperationalState, Vec<SubagentToolRequest>, Vec<SubagentCompletion>)` | test | Test: `SharedSnapshot::ptr_eq`: a snapshot shares the connection's publication, a changed copy does not, an empty catch-up keeps it. |
| `mj-session-runtime/src/worker_client/tests.rs:4718, 4734, 4741` | `assert!(held.0.ptr_eq(&unchanged.0) && held.1.ptr_eq(&unchanged.1));` (+2 more) | `generic T of mj_core::snapshot_map::SharedSnapshot<T> (Vec<SubagentToolRequest>, Vec<SubagentCompletion>)` | test | Test: `SharedSnapshot::ptr_eq`: an unchanged poll returns the same publication. |
| `mj-store/src/database/state_io.rs:21, 28` | `&& projects.insert(std::sync::Arc::as_ptr(value))` (+1 more) | `mj_core::repository::ProjectBundleSnapshot and mj_core::state::target_runtime::TargetRuntimeSettings` | prod | Look-alike: `Arc::as_ptr` inserted in a function-local `HashSet` to serialize each distinct shared value once while building the intern pool. The pointers do not outlive the call. |

- `SharedSnapshot::ptr_eq` (`mj-core/src/snapshot_map.rs:35`) is Mjolnir's own
  wrapper around `Arc::ptr_eq`; its callers are listed above so that all
  pointer-identity assertions are in one place.
- `futures::future::Shared::ptr_eq` and tokio's `same_channel` belong to the
  `futures` and `tokio` runtime packages, which must provide them.
- tokio `watch::Sender::receiver_count()` is also count-based logic. It is
  used only in tests, six times (`mj-daemon/src/daemon/tests.rs:7765`, `7784`,
  `7795`, `7796`, `10039`, `10058`), to wait until a task has subscribed. The
  tokio package must count receivers exactly (`Subscribe`/`Clone` adds one,
  `Drop` removes one).
- tokio `mpsc::WeakSender`, `Sender::downgrade`, `Sender::strong_count` and
  `sender_count` are not used anywhere.
- `std::ptr::eq` is applied to two plain borrows of the same collection
  element. In Go the borrow is a pointer to the element (`&items[i]`, or the
  pointer the map's `Get` method returns), and `==` on those pointers is the
  translation. It breaks if either side was copied to a local value first.
- `Arc::as_ptr` in `state_io.rs` only deduplicates within one function call.
  A Go `collections.HashSet[*T]` keyed by the pointer does the same.

## 9. Conclusions

### (a) Types whose `Arc<T>` must be `rs.Arc[T]`

These are the types for which a count or a weak upgrade is program logic.
"Everywhere" means every binding, field, parameter and return value that holds
the `Arc<T>`, with `Clone()` for each Rust `Arc::clone`/`.clone()` and `Drop()`
where Rust drops the handle, because one uncounted holder makes the count
wrong for all the others.

| # | `T` (full path) | Sites that force it | Forced by | Size of the change |
| --- | --- | --- | --- | --- |
| 1 | `mj_daemon::daemon::RuntimeState` | `Weak`: `process.rs:202-217`, `review_host/controller_environment.rs:28,53`, `github_watch/service.rs:95-98`, `daemon/lifecycle.rs:409,573,1124-1134`, `daemon/delegation/policy.rs:170-178`; tests `feed.rs:1193`, `tests.rs:11022`; `Arc::get_mut` in 7 tests | production | The largest: 56 lines spell `Arc<RuntimeState>` in 26 files, and 119 methods under `mj-daemon/src/daemon*` take `self: &Arc<Self>` or `self: Arc<Self>`; nearly every daemon task captures a clone |
| 2 | `mj_daemon::server_runtime::api::ApiBackend` | `Weak`: `daemon.rs:269`, `daemon/subagent_park.rs:21-33`, `daemon/process.rs:826`, `review_host/controller_environment.rs:38` | production | 20 lines in 5 files spell `Arc<..ApiBackend>`; 29 `Arc<Self>` receivers under `server_runtime/` |
| 3 | `mj_profile_catalog::profile_catalog::ProfileCatalog` | `Weak`: `profile_catalog.rs:908-949`; `Arc::get_mut`: `302`, `1927` and 9 tests | production | 15 lines in 7 files |
| 4 | `mj_profile_catalog::profile_config::ProbeLock` | registry `profile_config.rs:239-285` | production | local to `serialized_until` and `cancel_all` |
| 5 | `mj_core::jev::Writer` | registry `jev.rs:88-139`, `336` | production | reached only through `mj_core::jev::DecisionLog(Arc<Writer>)`, which derives `Clone`: every `DecisionLog` value (20 mentions in 9 files) needs `Clone()` and `Drop()` |
| 6 | `mj_client::daemon::connector::DaemonConnector` | static `connector.rs:16-41` | production | returned by `DaemonConnector::shared()` (3 callers) and stored in `DaemonClient.connector` |
| 7 | `mj_client::daemon::ssh::SshLease` | `Weak` in the ignored e2e test `ssh.rs:331-370`; production relies on the last holder's drop to stop the tunnel (section 10) | test, and production through `Drop` | `connector.rs` lease slot and `DaemonClient.ssh_lease` |
| 8 | `mj_github_relevance::github_student::inference::Engine` | `Weak` and `Weak::ptr_eq`: `github_student.rs:309-369` | production | 8 lines, one file plus its tests |
| 9 | `mj_worker_staging::worker_binary::store::StoreGate` | registry `store.rs:43-58` | production | `store_gate()` and its callers (11 lines in 5 files) |
| 10 | `mj_targets::image_pull_gate::ImagePullLock` | registry `image_pull_gate.rs:44-72` | production | `image_pull_mutex()` and its callers (10 lines in 5 files) |
| 11 | `std::sync::Mutex<()>` returned by `mj_controller::controller::worktree::source_fetch_lock` | registry `worktree.rs:731-750` | production | one function and its two call sites |
| 12 | `tokio::sync::Mutex<()>` in three roles: `RuntimeState::startup_delivery_gate` (`state.rs:450-464`, field `daemon.rs:290`); the `SessionSteps` locks (`credential_sync.rs:210-260`); `handback_gate` (`mj-worker/src/worker_runtime/subagents.rs:982`) | registries (production); `Arc::strong_count` in `handback_tests.rs` (test) | production and test | `startup_delivery_gate()` has 5 call lines in 3 files; `SessionSteps`/`SessionTurn` 25 lines in 3 files; `handback_gate` 9 lines in 2 files. Other `Arc<tokio::sync::Mutex<..>>` values in the code base stay plain pointers |
| 13 | `mj_session_runtime::worker_client::credential_sync::ProfileLogin` | `Arc::strong_count`: `credential_sync.rs:461` | production | 7 lines in 2 files |
| 14 | `std::sync::Mutex<Option<mj_targets::aws_assume_role::ScopedEntry>>` (the cache cells) | `Arc::strong_count`: `aws_assume_role.rs:280` | production | one file |
| 15 | `tokio::sync::Semaphore` held in `mj_cli::dashboard::summary_reads` (`permits`, line 21) | `Arc::strong_count`: `summary_reads.rs:175` | test only | one file, but the tokio package's owned permits must hold a counted clone (section 2). The other 15 lines that spell `Arc<Semaphore>` are not counted by any code |
| 16 | `mj_worker::relay::RelayCell` | `Weak`: field `mj-worker/src/acp/launch.rs:192` (`terminal_owner`), `acp/goal.rs:6-12`, `acp/drive.rs:761`, `worker_runtime/unix.rs:396`; tests `acp/goal.rs:668`, `acp/tests.rs:5430-5718`, `9203` | production | large: 56 lines spell `Arc<RelayCell>` across `mj-worker` |
| 17 | `()` as the lifetime of a command-loop question (`mj-worker/src/acp/operator_away.rs:208`, field `:543`) | `Weak<()>`: `operator_away.rs:219`, `518`, `532`, `555`, `564` | production | one file. The other `Arc<()>` identity tokens (section (d)) are compared only by pointer and have no `Weak` |

There is no generic function that counts or downgrades an `Arc<T>` for a type
parameter `T`, and no trait object is downgraded, so the list is closed over
concrete types.

Rows 11, 12, 14, 15 and 17 name a standard or tokio type. For those, "everywhere"
means every holder of the values created at the named sites, not every
`Arc<Mutex<..>>` in the program.

Section 10 lists further types that need the same treatment for a different
reason (a destructor that must run when the last reference goes), although no
count API is called on them.

### (b) `Arc::make_mut` sites: copy, then mutate

All 77 sites translate to `a = rs.ClonePtr(a)` followed by the mutation, and
for all of them always copying gives the same observable result as Rust.

The argument, checked per type: Rust mutates in place only when the `Arc` is
unique (one strong reference, no `Weak`). In that case no other holder exists
that could observe either the old value or the pointer, so replacing the
pointer with a fresh copy cannot be told apart, unless the same allocation is
also reachable through a `Weak`, a saved raw pointer or a count. The types at
the 77 sites are:

| `T` | Sites | Also downgraded, counted or address-saved? |
| --- | --- | --- |
| `mj_core::transcript::TranscriptItem` | 32 (30 tests, 1 test support, 1 production: `mj-store/src/database/materialized.rs:837`) | No. Its pointer identity is compared (`ptr_eq`), which a fresh copy handles correctly: see below |
| `mj_core::assessment::ContextHistory` | 7 (`relay/snapshot/apply.rs`) | No |
| `mj_core::assessment::TurnAssessment` | 4 production (`apply.rs:155,752,2496,2534`) and 6 tests | No |
| `mj_core::attention::NotificationStack` | 4 in `apply.rs`, 1 in `mj-worker/src/relay.rs:2610` | No |
| `Vec<mj_server::server::viewer_types::ViewerMjCommand>` | 9 (`server_runtime/snapshot.rs`) | No |
| `mj_core::state::target_runtime::TargetRuntimeSettings` | 3 production, 2 tests | `Arc::as_ptr` in `state_io.rs:28`, transient within one call: not affected |
| `mj_core::repository::ProjectBundleSnapshot` | 2 production | `Arc::as_ptr` in `state_io.rs:21`, transient: not affected |
| `Vec<agent_client_protocol::v1::schema::SessionConfigOption>` | 1 (`model_policy.rs:33`) | No |
| `Vec<agent_client_protocol::v1::schema::AvailableCommand>` | 1 test | No |
| `Vec<ImportProfileOption>`, `BTreeMap<String, String>`, `BTreeMap<(String, String), WikiHitTranscript>` | 3 (`mj-tui/src/resume.rs`) | No |
| generic `T` of `SharedSnapshot<T>` (`snapshot_map.rs:63`) | 1 line, implicit at every mutable use | No instantiation is downgraded or counted |
| generic `V` of `SnapshotMap<K, V>` (`snapshot_map.rs:111`) | 1 line, behind `get_mut`, `IndexMut`, `entry`, `first_value_mut` | No |

None of these types appears in list (a), and none has a hand-written `Clone`
with side effects (they derive `Clone`; `rs.ClonePtr` must still share their
`Arc` fields, per the `rs:"..,arc"` tag).

Pointer identity after the copy. Several of these types are compared with
`Arc::ptr_eq` to detect "unchanged" (`TranscriptItem` in `mj-tui/src/ingest.rs:168`
and `mj-core/src/transcript.rs:757`; every `SharedSnapshot`). Always copying
keeps those comparisons right: a comparison needs a second holder, a second
holder makes the Rust `Arc` shared, and a shared `Arc` is copied by Rust too.
The converse matters more: **never translate `make_mut` as mutation through
the existing pointer.** That would change the value under every other holder
and make "unchanged" caches return stale renderings.

Three things a person porting must know:

1. `SharedSnapshot<T>` implements `DerefMut` as `Arc::make_mut`
   (`snapshot_map.rs:61-65`). Rust inserts that call wherever a snapshot is
   used mutably (`snapshot.operational.native_session_id = ..`,
   `metadata.config.profiles.insert(..)`, a `&mut` method call), and inserts
   plain `Deref` wherever it is only read. The Go code must make the same
   distinction by hand at every use: a read must not copy. A read translated
   as a mutable access replaces the pointer, and the tests in section 6 and 8
   that assert a publication is retained (`feed.rs:1398-1411`,
   `session_manager/tests.rs:1883-1911`, `capacity.rs:1614`,
   `profile_catalog.rs:2437`, `worker_client/tests.rs:4718-4741`) fail; in
   production the same mistake makes every revision resend unchanged sections
   to clients. Instantiations: `RelayOperationalState`, `Config`,
   `QuotaSnapshot`, `ProfileCapabilitiesSnapshot`, `Vec<SubagentToolRequest>`,
   `Vec<SubagentCompletion>`, `Vec<TargetStorageView>`, `Vec<LaunchRecency>`,
   `BTreeMap<String, RuntimeContainerCapacity>`.
2. Cost. Rust copies at most once between two publications and then mutates
   in place; always copying copies on every mutation. That is the same result
   but not the same cost where a large value is edited repeatedly:
   - `apply.rs:2396` appends to `ContextHistory` for every streamed agent text
     chunk, and `apply.rs:2353-2469` edit it on every prompt and turn end. The
     history grows with the conversation, so always copying makes folding a
     turn quadratic in its length.
   - `NotificationStack` and `TurnAssessment` are edited on notification and
     assessment events.
   - `SharedSnapshot<RelayOperationalState>` is edited field by field as relay
     events arrive; `SharedSnapshot<Config>` holds the whole configuration.
   - `SnapshotMap::get_mut` copies one record (`SessionRecord`,
     `SubagentMetadata`, `MoveOperation`, ...) per call; the source has 183
     `sessions.get_mut(..)` calls (not all of them on a `SnapshotMap`), often
     several in one operation.
   - `server_runtime/snapshot.rs:336-433` pushes up to nine commands onto a
     clone of a cached list: nine copies of a short vector instead of one.
   If that cost is not acceptable, the remedy that keeps behaviour is to make
   these particular `Arc`s counted (`rs.Arc[T]`; `rs.ArcMakeMut` copies only
   when shared, as Rust does), which then requires their holders to drop
   correctly. That is a decision for the plan; this document only records
   that correctness does not require it.
3. `imbl::OrdMap` does the same thing internally: `OrdMap::get_mut`,
   `insert` and `remove` copy the tree path only when a node is shared. The
   Go `imbl` package can always copy the path (it is logarithmic) and stay
   correct; see `library-usage.md`.

### (c) `Arc::get_mut` sites

| Site | `T` | Class | Reason |
| --- | --- | --- | --- |
| `mj-client/src/transcript.rs:1238` (production) | `mj_client::web::BrowserTranscriptEntry` | provably unique: mutate directly | The rows come from `browser_transcript` three lines above, which ends in `entries.into_iter().map(Arc::new).collect()` (`transcript.rs:181`); they are published (cloned into `self.published`) only after the loop. The `.expect("unpublished browser row")` cannot fire |
| `mj-profile-catalog/src/profile_catalog.rs:302` (production) | `ProfileCatalog` | provably unique: mutate directly | `Self::new` (`:280`) is a bare `Arc::new(Self { .. })`; no task has been started and no `Weak` exists yet. `ProfileCatalog` is in list (a), so the Go spelling is `rs.ArcGetMut(catalog).Unwrap()` and it behaves as Rust does |
| `mj-profile-catalog/src/profile_catalog.rs:1927` (test support) | `ProfileCatalog` | provably unique | same, in `build` |
| `mj-profile-catalog/src/profile_catalog.rs:3097, 3155, 3201, 3364, 3409, 3850, 3980, 4115, 4154` (tests) | `ProfileCatalog` | provably unique | each follows the constructor call (`with_probe`, `build`) in the same test; the `.unwrap()` proves it on every run of the Rust test |
| `mj-daemon/src/daemon/tests.rs:763, 4490, 6828`, `daemon/support.rs:389`, `daemon/auto_afk.rs:234, 318`, `daemon/serve.rs:981` (tests) | `RuntimeState` | provably unique | each follows `test_runtime_state()` or `test_runtime_state_loading_the_store()` directly. `RuntimeState` is in list (a); `rs.ArcGetMut(state).Unwrap()` |
| `mj-transcript/src/projection.rs:426` (production) | `mj_core::transcript::TranscriptItem` | depends on the count, but does not need `rs.Arc` | `if let Some(owned) = Arc::get_mut(existing) { *owned = item } else { *existing = Arc::new(item) }`. The two branches differ only in whether the allocation is reused. Take the `else` branch always (`*existing = &item`). Reusing the allocation when another holder exists is the one thing that must not happen: `TranscriptSource::is` and `unchanged_prefix` treat an unchanged pointer as unchanged content |

So no `Arc::get_mut` site adds a type to list (a). The two types that are in
list (a) for other reasons use `rs.ArcGetMut`, which returns `None` when
another strong or weak reference exists, as in Rust.

### (d) `Arc::ptr_eq` sites

All 71 `Arc::ptr_eq` sites (and the `SharedSnapshot::ptr_eq` wrapper) are
identity tests that a pointer comparison answers, with these qualifications:

| `T` | Sites | Plain `==` on the Go pointer? |
| --- | --- | --- |
| `()` (identity tokens: `ChatInstance`, lifecycle tickets, startup-drain identity, recovery-gate identity, update-producer identity) | `mj-chat/src/chat/active.rs:728`; `mj-daemon/src/daemon/lifecycle.rs:315, 1169`; `mj-daemon/src/daemon/state.rs:1140, 1175`; `mj-store/src/recovery_gate.rs:387, 771, 802, 859`; `mj-session-runtime/src/session_manager/channels.rs:372, 475, 511` | **No.** `Arc<()>` must not become a pointer to a zero-size Go value (`*struct{}`, `*rs.Unit`): the Go specification allows two distinct zero-size variables to have the same address, so every token could compare equal. In Rust each `Arc::new(())` is a distinct allocation because of its counters. Use a type with size: `rs.Arc[rs.Unit]` (its box holds the counts) or a dedicated identity type. The tokens are created at `active.rs:659`, `active/io.rs:368`, `lifecycle.rs:267`, `state.rs:1102`, `recovery_gate.rs:239`, `channels.rs:439`, and (with a `Weak`, list (a) row 17) `mj-worker/src/acp/operator_away.rs:208` |
| `dyn Fn(Option<&[u8]>) + Send + Sync`, `dyn Fn(CommandActivity) + Send + Sync` | `mj-core/src/targets.rs:166, 204` | **No.** Rule 3 maps `Arc<dyn Fn..>` to a Go `func`, and Go functions cannot be compared. `StreamReply` and `CommandObserver` need a pointer to a box holding the function (or `rs.Arc[func(..)]`), compared by that pointer. Their `PartialEq` feeds `CommandSpec` equality |
| `std::sync::atomic::AtomicBool` (cancellation flags used as operation identity) | `mj-cli/src/dashboard/drains.rs:47, 1051`; `dashboard/io.rs:1090, 1846`; `mj-daemon/src/server_runtime/project_sources.rs:200`; `server_runtime/tests.rs:55`; `mj-store/src/recovery_gate.rs:458, 816` | Yes, as `*rssync.AtomicBool`. The flag must be held by pointer everywhere, never copied by value |
| `tokio_util::sync::CancellationToken` | `mj-github-relevance/src/github_student.rs:686` | Yes if `Arc<CancellationToken>` is a pointer to one token value and an `Arc` clone copies the pointer; `token.Clone()` would make a different object that is still the same token |
| `mj_github_relevance::github_student::inference::Engine` | `github_student.rs:161, 451, 507, 577, 685`; tests `github_student/tests.rs:309, 337, 403` | Yes. **Also in list (a)**: compare with `rs.Arc`'s pointer-equality method |
| `mj_github_relevance::github_student::inference::Gpu` | `github_student.rs:452, 508, 578`; tests `github_student/tests.rs:379, 400` | Yes |
| `mj_targets::image_pull_gate::ImagePullLock` | tests `image_pull_gate.rs:247, 250, 253` | Yes. **Also in list (a)** |
| `mj_core::transcript::TranscriptItem` | `mj-core/src/transcript.rs:757`; `mj-tui/src/ingest.rs:168`; test `mj-client/src/runtime_feed.rs:1493` | Yes, given (b) and (c): a changed item is always a new allocation |
| `mj_client::web::BrowserTranscriptEntry` | tests `mj-client/src/transcript.rs:1496, 1517` | Yes |
| `mj_daemon::daemon::owner::PollableWorkerInputs` | `daemon/owner.rs:191, 202` and tests `782-935`; `daemon/process.rs:1331-1404` | Yes |
| `mj_core::repository::ProjectBundleSnapshot`, `mj_core::state::target_runtime::TargetRuntimeSettings` | `mj-store/src/database/committed.rs:398` (generic helper `fresh<T>`, these two instantiations); tests `database/tests.rs:7606, 7610` | Yes; the values are `Option<Arc<T>>`, so unwrap the option first |
| `mj_store::database::committed::CommittedState`, `CommittedTurn` | tests `committed.rs:675, 684, 701, 962` | Yes; `CommittedTurn` comes from `SnapshotMap::get_shared`, which must return the stored pointer |
| `mj_server::server::viewer_types::ViewerSession` | `mj-server/src/server/api/wait.rs:101, 129` | Yes; only a shortcut before `==` |
| `mj_core::worker_build::ExecutableContents` | test `worker_build.rs:551` (unix) | Yes |
| `mj_core::assessment::TurnAssessment`, `Vec<acp AvailableCommand>`, `Vec<acp SessionConfigOption>` | tests `mj-worker/src/relay/snapshot_tests.rs:265-273` | Yes; `Arc<Vec<T>>` must be `*[]T`, a pointer to the slice header, not the slice |
| `mj_worker::acp::guardian::reviewer::ReviewerHost` | `mj-worker/src/acp/guardian/reviewer.rs:225` | Yes |
| generic `T` of `SharedSnapshot<T>`; generic `V` of `SnapshotMap<K, V>` | `snapshot_map.rs:36, 217` | Yes |

No `Arc::ptr_eq` site has `T = str` or `T = [U]` (which rule 3 maps to `string`
and a slice, where identity is not a pointer comparison).

`Weak::ptr_eq` is used twice: `github_student.rs:322` on `Engine` and
`mj-worker/src/acp/operator_away.rs:564` on the unit lifetime token, both in
list (a) (`w.PtrEq(other)`).

## 10. Related hazard outside the requested patterns: the last drop

This section is triage, not a complete audit. It is here because it enlarges
list (a).

The port turns `Arc<T>` into a plain pointer, and nothing runs when the last
plain pointer goes away. Rust runs `T`'s destructor at exactly that moment.
Where `T` has a destructor with an effect, "the last holder dropped it" is
program logic even though the code never asks for a count, and the `Arc` has
to be counted so that dropping the last handle runs the value's `Drop()`
(`rs.Arc` does this: `go/rs/arc.go`, `dropArcValue`).

Verified by reading the definition and the `Drop` body:

| `T` | Held as | What the last drop does |
| --- | --- | --- |
| `mj_store::upgrade::Registration` (`mj-store/src/upgrade.rs:274`, `Drop` at `:581`) | `_registration: Arc<Registration>` inside the activity guard (`:271`, built `:496`) | Removes the activity from the upgrade gate. This is the handoff admission described in `AGENTS.md`: a clone that is never dropped blocks every daemon handoff, one dropped early lets a handoff start under running work |
| `mj_store::recovery_gate::ForegroundHolder` (`recovery_gate.rs:47`, `Drop` at `:55`) | `ForegroundAdmission(Arc<ForegroundHolder>)`, a `Clone` newtype | `gate.release(session, identity, origin)` |
| `mj_store::recovery_gate::RecoveryAdmission` (`:683`, `Drop` at `:714`) | `RecoveryAttempt(Arc<RecoveryAdmission>)`, a `Clone` newtype | `gate.finish(session, cancelled)` |
| `mj_session_runtime::session_manager::channels::UpdateProducer` (`channels.rs:357`, `Drop` at `:364`) | `producer: Option<Arc<UpdateProducer>>` in the update sender | Removes the producer from the registry if it is still the current one |
| `mj_session_runtime::session_manager::delegation::DelegationPublisher` (`delegation.rs:104`, `Drop` at `:150`) | `Arc<DelegationPublisher>` (`channels.rs:352`, `delegation.rs:110`) | Clears the session's entry in the delegation mailbox |
| `mj_worker::acp::operator_away::PendingBridge` (`operator_away.rs:169`, `Drop` at `:180`) | `PendingElicitations(Arc<PendingBridge>)` | Removes the bridge and its pending elicitations |
| `mj_github_relevance::github_student::Student` (`github_student.rs:205`, `Drop` at `:396`) | `Option<Arc<Student>>` in `mj-daemon/src/github_watch/service.rs:118` | Cancels the student and releases its state |
| `mj_client::daemon::ssh::SshLease` (`ssh.rs:78`) | `Arc<SshLease>` in the connector slot and in each `DaemonClient` | Owns a `SupervisedChild`, whose `Drop` (`mj-core/src/subprocess.rs:332`) stops the ssh tunnel. Already in list (a) |
| `mj_core::jev::Writer` (`jev.rs:81`) | `DecisionLog(Arc<Writer>)` | Owns the `mpsc::Sender` whose drop ends the writer thread. Already in list (a) |

Candidates found by a name-based script and not verified one by one
(`/tmp`-side script; it lists product structs held in an `Arc` whose fields
include a channel endpoint, a child process, a socket or a product type with
`impl Drop`; it does not resolve paths, so same-named types in different
modules are confused, and it looks one level deep only):

`ApiBackend` (a channel receiver and sender; already in list (a)),
`RuntimeState` (already in list (a)), `ProfileCatalog` (a `watch::Sender`;
already in list (a)), `ProfileLogin` (a `watch::Sender` and an
`UnboundedSender`; already in list (a)), `mj_store::recovery_gate::RecoveryGate`
(19 `Arc<RecoveryGate>` lines; a sender), `mj_worker::relay::RelayCell`
(56 lines; a sender; already in list (a)), `mj_store::upgrade::Gate` (17 lines),
`mj_targets::target_storage::Board`, `CredentialWakes`,
`mj_client::operations::CriticalOperationTrackerInner`,
`mj_server::server::api::turns::HeldPrompts`,
`mj_sessionwiki::sessionwiki::Indexer`,
`mj_worker::worker_runtime::unix::PreparedConnectionServices` and
`ProjectMemoryIoBlock`, `mj_daemon::web_viewer::ViewerControl`,
`mj_core::mailbox::Sender`,
`mj_github_relevance::github_student::assets::Source` (child pipes).

What was not checked: types whose droppable field is two or more levels down;
`Arc<T>` where `T` is a standard, tokio or third-party type with a destructor
(`Arc<TempDir>`, `Arc<OwnedSemaphorePermit>`, `Arc<JoinSet<..>>`,
`Arc<tokio::sync::watch::Sender<..>>` and so on); closures in
`Arc<dyn Fn..>` that capture a droppable value; and the 141 product types with
`impl Drop` that are not held in an `Arc` directly but inside a struct that
is. The product code has 1,331 `Arc<` type positions and 1,461 `Arc::new(`
calls; the script saw 140 distinct simple type names inside `Arc<..>`.

The rule this suggests for the port: before making an `Arc<T>` a plain
pointer, check that dropping a `T` does nothing but free memory. If it does
more, the `Arc` is counted.
