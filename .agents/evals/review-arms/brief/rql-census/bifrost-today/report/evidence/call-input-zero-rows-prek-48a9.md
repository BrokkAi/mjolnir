# call-input returns 0 rows from proven call sites (prek 48a92521630cf77759a814fd7c47ebfd4cbff005)

Bifrost: bifrost-dev 683ae81b014143806991cac731edbfd7523dfe1f, `--profile measure` via mbx, binary sha256 14a9d9d51d6133d6e1f6d27d9079cb77160cdcabc66a5c251b66607497f96d47.
Repo: github.com/j178/prek at 48a92521630cf77759a814fd7c47ebfd4cbff005 (side worktree, private cold cache).
Invocation: `bifrost --root <worktree> --query-file .rql-census/<q>.rql` (default interactive limits; `--workspace-scaled-limits` gives the same answer).

## Query 1: the call sites (11 rows, complete, no diagnostics)

    (call-sites-from (enclosing-decl (function :name "get_staged_files")))

| call line | callee line | call_kind | callee | completeness | proof |
|---|---|---|---|---|---|
| 139 | 147 | method | prefligit.process.Cmd.output `impl Cmd::pub async fn output(&mut self) -> Result<Output> { ... }` | partial | unproven |
| 139 | 146 | method | prefligit.process.Cmd.check `impl Cmd::pub fn check(&mut self, checked: bool) -> &mut Self { ... }` | partial | unproven |
| 139 | 145 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 144 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 143 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 142 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 141 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 140 | method | prefligit.process.Cmd.arg `impl Cmd::pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self ` | partial | unproven |
| 139 | 139 | function | prefligit.git.git_cmd `(summary: &str)` | complete | proven |
| 149 | 149 | function | None `` | partial | unproven |
| 149 | 149 | function | prefligit.git.zsplit `(s: &[u8])` | complete | proven |

Source (src/git.rs:139-147):

    git_cmd("get staged files")?
        .arg("diff")
        .arg("--staged")
        .arg("--name-only")
        .arg("--diff-filter=ACMRTUXB") // Everything except for D
        .arg("--no-ext-diff")
        .arg("-z")
        .check(true)
        .output()

## Query 2: the labelled census query (0 rows)

    (call-input :parameter-index 0 (call-sites-from (enclosing-decl (function :name "get_staged_files"))))

Expected: at least one expression_site row per call with a formal at index 0 (receiver excluded, per reference/code-query.md:667). In particular, `"get staged files"` for the `prefligit.git.git_cmd(summary: &str)` call at line 139 is `complete`/`proven`, and `"--diff-filter=ACMRTUXB"` is the argument to `Cmd::arg(&mut self, arg: S)` at line 143, the census decisive row.

Actual: `results: []`, `truncated: true`, diagnostics:
- `semantic_results_omitted` [incomplete]: call_input omitted 1 input because a retained call site had no exact formal-parameter binding layout
- `step_produced_no_rows` [advisory]: step `call_input` produced 0 rows from 11 input row(s) after incomplete step execution

The step reports omitting 1 input but emits nothing for the other 10, including the complete, proven project-function call. `:parameter-name "summary"`/`"arg"` behaves the same (0 rows from 11).

The same 0-rows-from-N shape occurs on every prek `call-input` census row: N = 8, 11, 12, 32, 65.

## Related: call-sites-to panics

    (call-input :parameter-index 0 (call-sites-to (enclosing-decl (function :name "git_cmd"))))
    (call-sites-to (enclosing-decl (function :name "git_cmd")))

Both return `{"error":"--tool query_code panicked: dependency fact retains its source value","isError":true}`, panic at crates/bifrost-rust-analysis/src/analyzer/rust/route_kinds.rs:973:26, reached from `RustUnitDependencyIdentities::compute` <- `RustAnalyzer::selected_dependency_package` <- `candidate_macro_admission::admit_candidate_macro_in_scopes`. Backtrace: panic-route_kinds-973-prek-48a9-call-sites-to-git_cmd.backtrace.txt.

