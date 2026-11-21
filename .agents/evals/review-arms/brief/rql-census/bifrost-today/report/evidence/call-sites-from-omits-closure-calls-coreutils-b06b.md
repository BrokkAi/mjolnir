# call-sites-from omits calls inside a closure argument and reports the answer complete (coreutils b06b1e4ea8)

Bifrost: bifrost-dev 683ae81b014143806991cac731edbfd7523dfe1f, `--profile measure` via mbx (binary sha256 14a9d9d51d6133d6e1f6d27d9079cb77160cdcabc66a5c251b66607497f96d47).
Repo: github.com/uutils/coreutils at b06b1e4ea8 (side worktree, private cold cache). Default interactive limits; `--workspace-scaled-limits` gives the same answer.

Source (src/uucore/src/lib/mods/clap_localization.rs:167-174):

    fn try_get_matches_from_localized<I, T>(self, itr: I) -> ArgMatches
    where ...
    {
        self.try_get_matches_from(itr)
            .unwrap_or_else(|err| handle_clap_error_with_exit_code(err, crate::util_name(), 1))
    }

Query:

    (call-sites-from (enclosing-decl (method :name "try_get_matches_from_localized")))

Actual: 2 rows, `truncated: false`, **no diagnostics**:
- 172-173 method call (unwrap_or_else), callee unresolved (external), partial/unproven
- 172 method call (try_get_matches_from), callee unresolved (external), partial/unproven

Missing, with nothing reported: the call `handle_clap_error_with_exit_code(err, crate::util_name(), 1)` at line 173 (a project function, src/uucore/src/lib/mods/clap_localization.rs:28) and `crate::util_name()`, both written inside the closure argument within the declaration's body. call_sites_from is documented as "Call sites written inside the declaration".

Effect on the census row (coreutils-printenv-localized-clap-exit-code-1-not-2, set 0 fact 1):

    (call-input :parameter-index 2 (call-sites-from (enclosing-decl (method :name "try_get_matches_from_localized"))))

returns 0 rows with only advisory diagnostics (`call-input has no indexed declaration for an external or unresolved call target`; `step call_input found no rows for 2 input row(s)`), so the result reads as a complete empty answer. The decisive row is the literal `1` bound to `exit_code` at line 173.
