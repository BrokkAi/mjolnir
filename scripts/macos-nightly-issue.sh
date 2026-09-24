#!/usr/bin/env bash
# Keep one stable tracking issue in sync with the nightly macOS lane result.
# Inputs (env): MACOS_RESULT (success, failure, cancelled, or skipped),
# GITHUB_REPOSITORY, GITHUB_SERVER_URL, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT,
# GITHUB_SHA. Every GitHub call goes through `gh api`, JSON through `jq`.
set -euo pipefail

: "${MACOS_RESULT:?MACOS_RESULT must be set}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"
: "${GITHUB_SERVER_URL:?GITHUB_SERVER_URL must be set}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID must be set}"
: "${GITHUB_RUN_ATTEMPT:?GITHUB_RUN_ATTEMPT must be set}"
: "${GITHUB_SHA:?GITHUB_SHA must be set}"

title='Nightly macOS CI is failing'
marker='<!-- mjolnir-macos-nightly-tracking -->'
api_root="repos/$GITHUB_REPOSITORY"

# Only a pass or a failure changes the tracking issue; a cancelled or skipped
# lane tells us nothing about the code.
if [[ "$MACOS_RESULT" != success && "$MACOS_RESULT" != failure ]]; then
    printf "macOS lane result is '%s'; leaving the tracking issue unchanged.\n" "$MACOS_RESULT"
    exit 0
fi

# Collect the bot-created, non-pull-request issues that carry our title and
# marker. `gh api --paginate` without `--jq` streams one JSON array per page.
matches="$(
    gh api --paginate "$api_root/issues?state=all&creator=github-actions%5Bbot%5D&per_page=100" |
        jq -r --arg title "$title" --arg marker "$marker" '
            .[] | select(.pull_request == null and .title == $title)
            | select((.body // "") | contains($marker))
            | "\(.number)\t\(.state)"
        ' | sort -n
)"

issue_number=''
issue_state=''
if [[ -n "$matches" ]]; then
    issue_number="$(printf '%s\n' "$matches" | head -n1 | cut -f1)"
    issue_state="$(printf '%s\n' "$matches" | head -n1 | cut -f2)"
    extra_issues="$(printf '%s\n' "$matches" | tail -n +2 | cut -f1 | paste -sd, -)"
    if [[ -n "$extra_issues" ]]; then
        printf 'warning: duplicate tracking issues %s ignored; using #%s\n' "$extra_issues" "$issue_number" >&2
    fi
fi

timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
run_url="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"
commit_url="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/commit/$GITHUB_SHA"

failing_job_field=''
if [[ "$MACOS_RESULT" == failure ]]; then
    jobs="$(gh api "$api_root/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT/jobs?per_page=100")"
    failing_job_field="$(
        printf '%s' "$jobs" | jq -r '
            def bullet(job):
                "[\(job.name)](\(job.html_url))"
                + ([(job.steps // [])[] | select(.conclusion == "failure") | .name] as $steps
                    | if ($steps | length) > 0 then " (failed step: \($steps | join(", ")))" else "" end);
            [.jobs[] | select(.conclusion == "failure" or .conclusion == "timed_out")] as $failed
            | if ($failed | length) == 0 then
                "- Failing job: no failed job was reported; see the run"
            elif ($failed | length) == 1 then
                "- Failing job: " + bullet($failed[0])
            else
                "- Failing job:\n" + ($failed | map("  - " + bullet(.)) | join("\n"))
            end
        '
    )"
fi

result_word=failed
if [[ "$MACOS_RESULT" == success ]]; then
    result_word=passed
fi
details="$(
    printf -- '- Result: %s\n' "$result_word"
    printf -- '- Run: %s (attempt %s)\n' "$run_url" "$GITHUB_RUN_ATTEMPT"
    printf -- '- Commit: [%s](%s)\n' "${GITHUB_SHA:0:12}" "$commit_url"
    if [[ -n "$failing_job_field" ]]; then
        printf '%s\n' "$failing_job_field"
    fi
    printf -- '- Time: %s\n' "$timestamp"
)"

if [[ "$MACOS_RESULT" == failure ]]; then
    if [[ -z "$issue_number" ]]; then
        body="$(
            cat <<EOF
$marker

The scheduled macOS lane in \`.github/workflows/macos-nightly.yml\` failed on the default branch.
This issue stays open until a nightly run passes; the workflow closes it then, and later failures reopen it.

Latest failure:

$details
EOF
        )"
        created="$(gh api -X POST "$api_root/issues" -f title="$title" -f body="$body" --jq .number)"
        printf 'Created #%s\n' "$created"
    elif [[ "$issue_state" == closed ]]; then
        gh api -X PATCH "$api_root/issues/$issue_number" -f state=open >/dev/null
        gh api -X POST "$api_root/issues/$issue_number/comments" \
            -f body="$(printf 'The nightly macOS lane failed again.\n\n%s\n' "$details")" >/dev/null
        printf 'Reopened #%s\n' "$issue_number"
    else
        gh api -X POST "$api_root/issues/$issue_number/comments" \
            -f body="$(printf 'The nightly macOS lane is still failing.\n\n%s\n' "$details")" >/dev/null
        printf 'Commented on #%s\n' "$issue_number"
    fi
elif [[ -n "$issue_number" && "$issue_state" == open ]]; then
    gh api -X POST "$api_root/issues/$issue_number/comments" \
        -f body="$(printf 'The nightly macOS lane passed. Closing.\n\n%s\n' "$details")" >/dev/null
    gh api -X PATCH "$api_root/issues/$issue_number" -f state=closed -f state_reason=completed >/dev/null
    printf 'Closed #%s\n' "$issue_number"
else
    printf 'No open tracking issue; nothing to close.\n'
fi
