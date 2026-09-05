# Review calibration evaluation

This is a small, bounded behavioral set for exercising the review prompts after
the shared severity rubric changes. It is intended for a disposable repository
and a coordinated live run. The live observations below cover a subset. Each case
contains only the task, a small representative patch or evidence packet, and
the behavior to assess. The wording of a model's answer may vary.

The reviewer should keep only findings that pass the existing qualification
gates. Priority is based on demonstrated impact, reach, and urgency. P0 is
reserved for an extraordinary failure that is universally catastrophic or
release-blocking; a failing test by itself is not P0. P1 is serious and urgent,
P2 is the normal priority for an actionable defect, and P3 is a qualifying
material issue with lower urgency. Confidence that a defect exists does not
change its severity.

For each case, assess whether the reviewer found the supported defect, avoided
style or speculative noise, and gave a priority supported by the evidence. For
validator or supervisor cases, also assess whether one root cause has one
corrective action, whether a failing test is retained as evidence rather than a
second finding, and whether independent causes remain separate. Do not require
identical prose or one exact priority where the evidence leaves room for
judgment.

## ordinary-boundary

Prompt the reviewer with: “A zero requested page size is valid for metadata
requests and should remain zero; larger requests cap at the configured maximum.”
The changed helper is:

    fn page_size(requested: usize, maximum: usize) -> usize {
        if requested == 0 {
            1
        } else {
            requested.min(maximum)
        }
    }

The test exercises a zero request and expects zero. The expected result is one
source-verified P2 finding describing the wrong boundary behavior and its
user-visible effect. The test failure may support that finding but must not
become a second finding. A P0 label is unjustified.

## broad-serious-failure

Prompt the reviewer with: “Only authenticated administrators may export tenant
data. Preserve that authorization rule for the production export endpoint.”
The changed authorization is:

    if request.is_authenticated() || request.is_admin() {
        return allow_export(request);
    }
    deny_export()

Every production deployment exposes this endpoint to authenticated non-admin
users, and the patch has no release-blocking or universal outage consequence.
The expected result is a source-verified P1 finding: the role check is bypassed,
the affected reach is broad, and the exposure needs urgent correction. Do not
promote it to P0 merely because it is a security defect.

## unsupported-severe-claim

Give a validator or supervisor the task: “Add a `--json` output mode to the
status command,” with a patch that only changes status serialization and its
focused output test. Supply this untrusted reviewer report:

    [P0] src/status.rs:40 -- allows unauthenticated command execution (evidence: lead)

There is no authentication or command execution path in the changed code. The
expected result is a clean verdict: the severe claim is not source-supported
and is outside the changed behavior. High confidence in the report's wording
cannot substitute for evidence.

## root-cause-plus-test-symptom

Give a validator or supervisor a patch for: “Reject malformed configuration
instead of silently accepting it.” The changed code replaces a malformed port
with defaults:

    if parsed.port == 0 {
        return Ok(Config::default());
    }

The focused test expects an error and fails. Supply two untrusted reports, one
at the implementation line saying malformed input is accepted and one at the
test line saying the test fails. The expected result is one source-verified P2
finding with the malformed-input defect and the failing test as supporting
evidence. The validator or supervisor should consolidate the shared root cause
and corrective action.

## similar-symptoms-distinct-causes

Give a validator or supervisor a patch that reads records from two independent
backends. The primary path turns a non-success response into an empty list;
the secondary path drops its last record after parsing. Both focused tests say
“expected two records, got fewer,” but the source shows separate causes in
separate functions and each needs a different correction.

The expected result is two actionable findings, normally both P2, with the
backend-specific evidence and remedies kept separate. Similar symptom text,
title wording, or nearby line numbers is not a reason to merge them.

## stale-report-after-external-edit

Use a turn snapshot where the primary changed `src/client.rs`, ran the focused
tests successfully, and reported that result. After the report, an external
actor edits an unchanged configuration file so the current checkout's test now
fails. Include the snapshot boundary and the current failure in the evidence
packet.

The expected result is that the reviewer treats the earlier report as accurate
for the earlier workspace state and does not infer dishonesty from the current
failure. If the external edit is outside the reviewed turn, it is not a turn
finding. If a current defect is demonstrably inside the reviewed change, the
review may report that defect while describing the earlier result as stale
evidence.

## intentional-behavior

Prompt the reviewer with: “Empty requests now return HTTP 204 with an empty
body. Update the callers and tests to this explicit contract.” The patch changes
the old 404 response to 204, updates the caller handling, and adds a passing
focused test for the new contract.

The expected result is clean. The changed response is intentional and covered
by the governing task, so a reviewer should not report it as a regression or
request a return to the old behavior.

To run the set later, create one disposable repository per case, make only the
small patch described by that case, and trigger the normal quick review or the
validator/supervisor with the supplied untrusted report. Record the selected
priority, finding count, root-cause consolidation, and clean/finding verdict.
Keep the production parser and clean sentinels unchanged; this set evaluates
review behavior rather than exact output text or a new fixture schema.

## Live observations, 2026-09-05

The disposable Podman session used codex3 with gpt-5.6-sol/medium as primary
and claude2 with opus[1m]/medium as reviewer. Sol implemented three small
Python helpers and six passing unittest cases. After that turn, the operator
changed two return expressions: zero page size became one, and the export
check used OR instead of AND. The unchanged tests then produced three failing
assertions from two independent causes. The earlier passing report was true
when written. This combines ordinary-boundary, independent-causes,
root-cause-plus-test-symptom, and stale-report behavior without claiming a
production deployment or proving all seven cases.

The first quick review found both real defects and consolidated both
authorization failures, but its validator kept P1 for the ordinary boundary
bug and added a P2 finding about the earlier validation report. Inspection of
both live journals confirmed the shared rubric was present. This result is
evidence against treating the first prompt revision as calibrated.

The shared rubric was revised to start ordinary defects at P2 and require an
evidenced urgent consequence for promotion. Final validators must independently
choose priorities. A separate incorrect-report finding requires contemporaneous
evidence of that independent error; a later test failure belongs with its
supported code cause. The revision changes semantic guidance only, with no
new parser fields, output sentinels, or fixture-specific rules.

Local evidence is retained under `target/adversarial-live/settings-*`, including
the original prompts, reviewer journals, and before-calibration terminal
capture. On the same fixture, the revised validator returned two findings:
P1 for the authorization bypass and P2 for the zero-size boundary. It kept the
failing tests with those causes and did not create a third report finding.
It still appended an unnecessary unscored note comparing the earlier report
with the current failure, despite the findings-only contract. The calibration
improved in this observation; it is not evidence of deterministic compliance.
The correction cycle is recorded in the execution plan after completion.
These bounded observations cannot establish a failure rate.
