// Package similar stands in for the Rust crate `similar` 2.7.0, a diffing
// library, built as Mjolnir builds it: with the default `text` feature and
// without `bytes`, `unicode`, `inline` and `serde`.
//
// Mjolnir uses it for the patches it shows (mj-core/src/diff.rs), for
// reconciling edits to project memory (mj-core/src/project_memory.rs) and for
// the difference a failed golden test prints (mj-core/src/golden.rs). All
// three depend on the exact edit script: which of several equally short
// scripts the algorithm picks, how the clean-up passes move and merge the
// edits, where the hunks are cut. The package is therefore a port of the
// crate's source, file by file, and not a wrapper around another diff
// library. For any pair of inputs it gives the ops, the changes and the
// unified diff text the crate gives.
//
// # Names
//
// Names follow the path a Rust caller writes:
//
//	similar::TextDiff::from_lines(a, b)       similar.TextDiffFromLines(a, b)
//	similar::TextDiff::configure()            similar.TextDiffConfigure()
//	    .timeout(d).diff_lines(a, b)              .Timeout(d).DiffLines(a, b)
//	diff.unified_diff().header(a, b)          diff.UnifiedDiff().Header(a, b)
//	    .to_string()                              .String()
//	similar::ChangeTag::Insert                similar.ChangeTag_Insert
//	similar::DiffTag::Equal                   similar.DiffTag_Equal
//	similar::DiffOp::Equal { .. }             *similar.DiffOp_Equal
//	similar::udiff::UnifiedDiff               similar.UdiffUnifiedDiff
//	similar::algorithms::Capture              similar.AlgorithmsCapture
//	similar::algorithms::myers::diff          similar.AlgorithmsMyersDiff
//
// Constructors and builder methods return pointers, so that a chain of calls
// reads as it does in Rust. An iterator (`iter_all_changes`, `iter_changes`)
// is a value with Next, as the translation rules prescribe, and also has All,
// a sequence to range over, and Collect. `iter_hunks`, whose Rust type is an
// unnamed `impl Iterator`, returns the sequence directly:
//
//	for hunk := range unified.IterHunks() { text := hunk.String(); ... }
//
// # Covered
//
//   - src/types.rs: Algorithm, ChangeTag, Change, DiffOp, DiffTag and their
//     methods.
//   - src/text/mod.rs: TextDiffConfig and TextDiff (lines, words, chars and
//     slices; algorithm, deadline, timeout and newline_terminated settings;
//     ops, grouped_ops, iter_changes, iter_all_changes, ratio,
//     unified_diff).
//   - src/text/abstraction.rs: `DiffableStr` for `str`, as the functions
//     DiffableStrTokenizeLines and so on.
//   - src/udiff.rs: UnifiedDiff, UnifiedDiffHunk, UnifiedHunkHeader, with
//     their Display output and to_writer.
//   - src/iter.rs: ChangesIter and AllChangesIter.
//   - src/common.rs: capture_diff and its variants, get_diff_ratio,
//     group_diff_ops.
//   - src/algorithms: the Myers, patience and LCS algorithms with their
//     deadline forms, the DiffHook trait, and the Capture, Replace, Compact
//     and NoFinishHook hooks, and IdentifyDistinct.
//
// Mjolnir only ever selects Myers. Patience and LCS are ported because
// Algorithm names them and a text differ configured with either must not
// panic; they are checked against the crate like everything else.
//
// # Differences in shape
//
//   - The crate diffs anything that implements `Index<usize>`, and lets the
//     two sides have different element types. Here both sides are slices of
//     one comparable type. A text diff is over []string.
//   - `core::ops::Range<usize>` is this package's Range, because go/rs has no
//     range type yet.
//   - `std::time::Instant` is rstime.Instant, in TextDiffConfig.Deadline and
//     in the `deadline` arguments of the algorithms.
//   - A DiffOp is `Copy` in Rust and a pointer to a variant struct here. The
//     package never changes a variant struct after building it.
//   - `Change<T>` has text methods (as_str, to_string_lossy, missing_newline,
//     Display) only when T is a string type. Go declares them for every T;
//     on anything but Change[string] they panic.
//   - GroupDiffOps uses up the slice it is given, as the Rust function takes
//     its vector by value. TextDiff.GroupedOps copies first, as Rust does.
//
// # Deadlines
//
// A deadline makes the result depend on how fast the machine is: when it
// passes, Myers stops searching and reports the rest of the range it was
// working on as one deletion and one insertion. That is the crate's
// behaviour and it is kept; what cannot be kept is the exact point at which
// a given machine runs out of time. Mjolnir gives its patches 250ms.
//
// # Faults kept from the crate
//
// The Go code does what the Rust code does, including where the Rust code
// looks wrong. None of these is on a path Mjolnir takes.
//
//   - Compact never moves a deletion up or down: it measures the overlap
//     against the deletion's (empty) range in the new sequence. Only
//     insertions are moved.
//   - LCS builds its table from the first elements of both sequences rather
//     than from the part between the common prefix and suffix, and reports
//     an unchanged range as starting at index 0 whatever range was asked
//     for.
//   - LCS with a deadline that has passed reports the same deletion twice.
//     Behind Compact that usually ends in an out-of-bounds read, which
//     panics here with Rust's message. Behind Replace alone it trips a
//     `debug_assert_eq!`: a panic in a debug build (the Go tests, built with
//     the mj_debug tag) and nothing in a release build.
//
// # Not covered
//
//   - `similar::get_close_matches` and src/text/utils.rs.
//   - src/utils.rs: TextDiffRemapper and the diff_chars, diff_words,
//     diff_lines, diff_slices shortcuts.
//   - `similar::udiff::unified_diff`, the free function. Its Go name would be
//     that of the type UdiffUnifiedDiff. Write
//     `TextDiffConfigure().Algorithm(alg).DiffLines(old, new).UnifiedDiff().ContextRadius(n).Header(a, b).String()`.
//   - Everything behind the features Mjolnir leaves off: byte-slice diffs
//     (`bytes`), grapheme and Unicode-word tokenizers (`unicode`), inline
//     changes (`inline`), serialization (`serde`), `wasm32_web_time`.
//
// # Tests
//
// The crate's own tests for the covered parts are ported with their
// snapshots (testdata/snapshots, copied from the crate). The differential
// tests in oracle_test.go compare tokens, ops, changes, grouping, hunks and
// unified text with the real crate over fixed and generated inputs: repeated
// lines, moved and duplicated blocks, empty inputs, missing final newlines,
// CR and CRLF line ends, inputs on both sides of the 100-item threshold at
// which the text differ first maps lines to integers, each algorithm behind
// each hook, and deadlines that have already passed.
package similar
