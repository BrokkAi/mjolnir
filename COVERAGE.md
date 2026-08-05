# Code coverage

GitHub Actions runs LLVM coverage in a separate `LLVM coverage` job. The job
executes the complete Rust workspace test suite, publishes a Markdown summary,
and retains both JSON and LCOV reports as the `llvm-coverage` artifact.

Run the same collection locally with:

```sh
cargo llvm-cov --workspace --no-report
cargo llvm-cov report --summary-only
```

The regression policy lives in `.github/coverage-baseline.json`. Aggregate line
coverage may move by less than the recorded material-regression tolerance. Each
production Rust module has a 70% line target unless the baseline contains a
reviewed exception with a lower temporary minimum and a concrete integration
boundary. New modules therefore start with the 70% expectation automatically.

The report includes colocated `#[cfg(test)]` bodies because LLVM attributes
those lines to the same source files as production code. Percentages are useful
for regression detection, but are not production-only measurements and should
not be raised by tests that lack behavioral value. Generated and vendor code is
not excluded; any future exclusion must be explicit in the workflow and review.

When behavior or tests intentionally move the baseline, collect a green full
workspace profile, update the recorded aggregate or reviewed exception, and
include the before/after evidence in the pull request.
