# Wizard-of-Oz site tool: do reviewers fetch the decisive fact when they must ask?

Feasibility step 1 for Bifrost as an agentic review tool (bifrost priority 2,
feasibility phase; MVP definition adopted by the engine coordinator on
2026-11-16). The ablation (`PLAN-ablation.md`) showed that the decisive fact
lifts catches when it is handed to the reviewer; the dilution arm
(`PLAN-dilution.md`) measures whether it survives among every sibling fact in
a brief. This arm hands the reviewer nothing: the same facts sit behind a
tool, and the reviewer gets one only by asking about the right site. It
answers whether an agentic reviewer selects the decisive fact by querying,
before engine work is sized for a query tool. Registered before any session
runs.

## Diffs

The ablation's 10 diffs, in its order. Chosen by outcome; arms compare with
each other on these diffs only.

## Arm

**Q (query)**, `QT`/`QS`: the no-brief reviewer prompt plus a short note that
a `site_context` tool is available, and no brief. 3 Terra/high and 3 Sol/high
reviewers per diff: 10 × 6 = 60 sessions, in a second `REVIEW_ARMS_DIR`
instance so that no other arm's reviewers see the tool.

### The stand-in tool

- One MCP tool, `site_context`, taking a qualified function name or a
  `path:line`. It returns every fixture fact anchored at that function
  (any anchor inside its body) or at that line.
- The fixture for a diff is exactly what arm W adds to arm D's brief: the
  truth-checked sibling facts that W kept, plus arm D's decisive item(s)
  (`briefs/bifrost-diluted/<diff>/meta.json`). So Q and W carry the same
  facts and differ only in delivery: pushed in a brief, or pulled by query.
- Rows: each fact's anchor site rendered in the provisional SiteContext line
  form (`<path>:<line>:<column> <role> \`<text>\``, design owner 479392b9),
  then the fact. The anchor's JSON is validated against a schema dumped with
  schemars from `SiteContextView` on bifrost-dev master, never a copy.
- Completeness once per answer: facts of the six kinds are complete for sites
  in the dilution scope (functions the diff changes, call sites the Bifrost
  brief cites); any other site answers `incomplete: not computed`, never an
  empty complete answer.
- Blinding: the fixture lives only on the server side, reached through the
  forced-command SSH delivery helpers H5 and H2 agreed (allowlisted diffs
  only). No reviewer workspace or tool can read it; a probe from a reviewer
  environment that tries to read the fixture path must fail before any
  session runs.
- Every call is logged server-side with session, arguments and the fact IDs
  returned.

### Retirement

The stand-in (its server script and fixtures) is deleted when an arm with the
real toolset exists behind the same interface. It is not a product path.

## Judging

Round I: `brief_score.py build --arms QT,QS,WT,WS,DT,DS,FT,FS --bugs-from
oz-jobs.json --name brief-judge-i`, which includes the stored no-brief
sessions. Two gemini-4-carbon/high judges (`verify_driver.py --kind judge`),
hand check of every caught or partial verdict and every disagreement, as in
rounds G and H. Round H's labels for D, F, W and no-brief are reported beside
round I's (more than 10 points apart for any arm and model is a caveat).

## Reading

Terra/high is primary; Sol/high is reported beside it. Catch rates are means
over the 10 diffs, from round I.

- **Retrieval:** share of Q sessions that received the decisive item from
  `site_context` (from the call log), per model.
- **Catches:** Q against no brief (N), arm D and arm W, with paired 95%
  bootstrap intervals over the 10 diffs.

Outcomes, first that holds (Terra):

1. **Querying works:** Q ≥ D − 10 points. A reviewer that must ask gets the
   fact about as well as one that is handed it; engine facts behind a query
   tool are the path to test next with real Bifrost.
2. **Querying fails:** Q ≤ N + 10 points. Reviewers do not ask for, or do
   not use, the fact; a query tool alone is not enough.
3. **Partly:** otherwise.

Retrieval is reported with each outcome: low retrieval with a low Q means
reviewers did not ask; high retrieval with a low Q means they asked but did
not use the answer. With 10 diffs the intervals are wide; the outcome is a
direction, not a measurement.
