#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
reporter="$repository_root/scripts/macos-nightly-issue.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

fake_bin="$test_root/bin"
gh_log="$test_root/gh.log"
gh_state="$test_root/gh-state.json"
mkdir -p "$fake_bin"
: >"$gh_log"

# Issue 5 is an unrelated bot issue, issue 6 looks like a tracking issue but is
# a pull request. Neither may ever be touched.
cat >"$gh_state" <<'SEED'
[
  {
    "number": 5,
    "title": "An unrelated bot issue",
    "body": "unrelated body",
    "state": "open",
    "user": {"login": "github-actions[bot]"},
    "comments": []
  },
  {
    "number": 6,
    "title": "Nightly macOS CI is failing",
    "body": "<!-- mjolnir-macos-nightly-tracking -->",
    "state": "open",
    "user": {"login": "github-actions[bot]"},
    "pull_request": {"url": "https://example.test/pull/6"},
    "comments": []
  }
]
SEED

cat >"$fake_bin/gh" <<'FAKE_GH'
#!/usr/bin/env bash
set -euo pipefail

invocation="$*"
printf '%s\n' "$invocation" >>"$MJ_TEST_GH_LOG"

reject() {
    printf 'unexpected fake gh invocation: %s\n' "$invocation" >&2
    exit 1
}

[[ "${1:-}" == api ]] || reject
shift

method=GET
jq_expression=''
path=''
payload=()
while (($#)); do
    case "$1" in
        --paginate) shift ;;
        --jq) jq_expression="$2"; shift 2 ;;
        -X) method="$2"; shift 2 ;;
        -f) payload+=("$2"); shift 2 ;;
        -*) reject ;;
        *) path="$1"; shift ;;
    esac
done

field() {
    local wanted="$1" pair
    for pair in "${payload[@]}"; do
        if [[ "${pair%%=*}" == "$wanted" ]]; then
            printf '%s' "${pair#*=}"
            return 0
        fi
    done
    return 1
}

rewrite_state() {
    jq "$@" "$MJ_TEST_GH_STATE" >"$MJ_TEST_GH_STATE.tmp"
    mv "$MJ_TEST_GH_STATE.tmp" "$MJ_TEST_GH_STATE"
}

case "$method:$path" in
    GET:repos/*/issues\?*)
        command cat "$MJ_TEST_GH_STATE"
        ;;
    GET:repos/*/actions/runs/*/attempts/*/jobs\?*)
        printf '%s\n' "$MJ_TEST_GH_JOBS"
        ;;
    POST:repos/*/issues)
        number="$(jq '[.[].number] | max // 0' "$MJ_TEST_GH_STATE")"
        number=$((number + 1))
        rewrite_state --argjson number "$number" --arg title "$(field title)" --arg body "$(field body)" \
            '. + [{number: $number, title: $title, body: $body, state: "open",
                   user: {login: "github-actions[bot]"}, comments: []}]'
        if [[ "$jq_expression" == .number ]]; then
            printf '%s\n' "$number"
        else
            jq -c --argjson number "$number" '.[] | select(.number == $number)' "$MJ_TEST_GH_STATE"
        fi
        ;;
    PATCH:repos/*/issues/[0-9]*)
        issue="${path##*/}"
        rewrite_state --argjson number "$issue" --arg state "$(field state)" \
            'map(if .number == $number then .state = $state else . end)'
        jq -c --argjson number "$issue" '.[] | select(.number == $number)' "$MJ_TEST_GH_STATE"
        ;;
    POST:repos/*/issues/[0-9]*/comments)
        issue="${path%/comments}"
        issue="${issue##*/}"
        rewrite_state --argjson number "$issue" --arg body "$(field body)" \
            'map(if .number == $number then .comments += [$body] else . end)'
        jq -c --argjson number "$issue" '.[] | select(.number == $number) | {number: .number, body: .comments[-1]}' \
            "$MJ_TEST_GH_STATE"
        ;;
    *)
        reject
        ;;
esac
FAKE_GH
chmod +x "$fake_bin/gh"

# Canned jobs response: one success, one failure with a failed step, and the
# in-progress reporter job that triggered this run.
export MJ_TEST_GH_JOBS
MJ_TEST_GH_JOBS="$(
    jq -cn '{
        jobs: [
            {name: "macOS / macOS (x86_64-apple-darwin)", conclusion: "success",
             html_url: "https://example.test/job/1", steps: [{name: "Run tests", conclusion: "success"}]},
            {name: "macOS / macOS (aarch64-apple-darwin)", conclusion: "failure",
             html_url: "https://example.test/job/2", steps: [
                 {name: "Checkout", conclusion: "success"},
                 {name: "Run tests", conclusion: "failure"},
                 {name: "Upload logs", conclusion: "skipped"}
             ]},
            {name: "Report nightly macOS CI result", conclusion: null,
             html_url: "https://example.test/job/3", steps: []}
        ]
    }'
)"

export MJ_TEST_GH_LOG="$gh_log"
export MJ_TEST_GH_STATE="$gh_state"

sha1='1111111111111111111111111111111111111111'
sha2='2222222222222222222222222222222222222222'
sha3='3333333333333333333333333333333333333333'
sha4='4444444444444444444444444444444444444444'
sha5='5555555555555555555555555555555555555555'

run_reporter() {
    env \
        PATH="$fake_bin:$PATH" \
        MACOS_RESULT="$1" \
        GITHUB_REPOSITORY=BrokkAi/mjolnir \
        GITHUB_SERVER_URL=https://github.com \
        GITHUB_RUN_ID="$2" \
        GITHUB_RUN_ATTEMPT=1 \
        GITHUB_SHA="$3" \
        bash "$reporter"
}

assert_contains() {
    local expected="$1" filename="$2"
    grep -F -- "$expected" "$filename" >/dev/null || {
        printf 'expected %s to contain %q\n' "$filename" "$expected" >&2
        exit 1
    }
}

assert_lacks() {
    local unexpected="$1" filename="$2"
    if grep -F -- "$unexpected" "$filename" >/dev/null; then
        printf 'expected %s to lack %q\n' "$filename" "$unexpected" >&2
        exit 1
    fi
}

assert_text_contains() {
    local text="$1" expected="$2" description="$3"
    [[ "$text" == *"$expected"* ]] || {
        printf 'expected %s to contain %q\n' "$description" "$expected" >&2
        exit 1
    }
}

tracking_issue() {
    local numbers
    numbers="$(
        jq -r '.[] | select(.pull_request == null and .title == "Nightly macOS CI is failing")
            | select((.body // "") | contains("<!-- mjolnir-macos-nightly-tracking -->"))
            | .number' "$gh_state"
    )"
    if [[ -z "$numbers" || "$numbers" == *$'\n'* ]]; then
        printf 'expected exactly one tracking issue, found: %s\n' "${numbers:-none}" >&2
        exit 1
    fi
    printf '%s' "$numbers"
}

issue_field() {
    jq -r --argjson number "$1" --arg field "$2" '.[] | select(.number == $number) | .[$field]' "$gh_state"
}

last_comment() {
    jq -r --argjson number "$1" '.[] | select(.number == $number) | .comments[-1]' "$gh_state"
}

comment_count() {
    jq -r --argjson number "$1" '.[] | select(.number == $number) | .comments | length' "$gh_state"
}

# 1. A passing run without a tracking issue changes nothing.
state_before="$(command cat "$gh_state")"
run_reporter success 1001 "$sha1" >"$test_root/step1.out"
[[ "$(command cat "$gh_state")" == "$state_before" ]]
assert_contains 'No open tracking issue; nothing to close.' "$test_root/step1.out"
assert_lacks '-X POST' "$gh_log"
assert_lacks '-X PATCH' "$gh_log"

# 2. The first failure creates the tracking issue.
run_reporter failure 2002 "$sha2" >"$test_root/step2.out"
issue="$(tracking_issue)"
assert_contains "Created #$issue" "$test_root/step2.out"
[[ "$(issue_field "$issue" state)" == open ]]
issue_body="$(issue_field "$issue" body)"
assert_text_contains "$issue_body" '<!-- mjolnir-macos-nightly-tracking -->' 'the new issue body'
assert_text_contains "$issue_body" 'https://github.com/BrokkAi/mjolnir/actions/runs/2002' 'the new issue body'
assert_text_contains "$issue_body" '222222222222' 'the new issue body'
assert_text_contains "$issue_body" 'macOS / macOS (aarch64-apple-darwin)' 'the new issue body'
assert_text_contains "$issue_body" 'Run tests' 'the new issue body'
[[ "$(comment_count "$issue")" == 0 ]]

# 3. A second failure comments on the same issue and creates no new one.
run_reporter failure 3003 "$sha3" >"$test_root/step3.out"
[[ "$(tracking_issue)" == "$issue" ]]
[[ "$(comment_count "$issue")" == 1 ]]
assert_text_contains "$(last_comment "$issue")" 'still failing' 'the second comment'
assert_text_contains "$(last_comment "$issue")" 'https://github.com/BrokkAi/mjolnir/actions/runs/3003' 'the second comment'

# 4. A passing run comments and closes the issue.
run_reporter success 4004 "$sha4" >"$test_root/step4.out"
[[ "$(tracking_issue)" == "$issue" ]]
[[ "$(issue_field "$issue" state)" == closed ]]
[[ "$(comment_count "$issue")" == 2 ]]
assert_text_contains "$(last_comment "$issue")" 'passed' 'the pass comment'
assert_text_contains "$(last_comment "$issue")" 'https://github.com/BrokkAi/mjolnir/actions/runs/4004' 'the pass comment'

# 5. A later failure reopens the same issue.
run_reporter failure 5005 "$sha5" >"$test_root/step5.out"
[[ "$(tracking_issue)" == "$issue" ]]
[[ "$(issue_field "$issue" state)" == open ]]
[[ "$(comment_count "$issue")" == 3 ]]
assert_text_contains "$(last_comment "$issue")" 'failed again' 'the reopen comment'
assert_text_contains "$(last_comment "$issue")" 'https://github.com/BrokkAi/mjolnir/actions/runs/5005' 'the reopen comment'

# 6. Cancelled and skipped lanes leave the issue alone.
state_before="$(command cat "$gh_state")"
run_reporter cancelled 6006 "$sha1" >"$test_root/step6-cancelled.out"
[[ "$(command cat "$gh_state")" == "$state_before" ]]
assert_contains "macOS lane result is 'cancelled'; leaving the tracking issue unchanged." "$test_root/step6-cancelled.out"
run_reporter skipped 7007 "$sha1" >"$test_root/step6-skipped.out"
[[ "$(command cat "$gh_state")" == "$state_before" ]]
assert_contains "macOS lane result is 'skipped'; leaving the tracking issue unchanged." "$test_root/step6-skipped.out"

# 7. The decoys stay untouched.
[[ "$(issue_field 5 title)" == 'An unrelated bot issue' ]]
[[ "$(issue_field 5 state)" == open ]]
[[ "$(comment_count 5)" == 0 ]]
[[ "$(issue_field 6 title)" == 'Nightly macOS CI is failing' ]]
[[ "$(issue_field 6 state)" == open ]]
[[ "$(comment_count 6)" == 0 ]]
assert_lacks 'issues/5' "$gh_log"
assert_lacks 'issues/6' "$gh_log"

# 8. A missing invocation variable fails clearly instead of guessing.
if env -u GITHUB_RUN_ID \
    PATH="$fake_bin:$PATH" \
    MACOS_RESULT=failure \
    GITHUB_REPOSITORY=BrokkAi/mjolnir \
    GITHUB_SERVER_URL=https://github.com \
    GITHUB_RUN_ATTEMPT=1 \
    GITHUB_SHA="$sha1" \
    bash "$reporter" >"$test_root/step8.out" 2>&1; then
    printf '%s\n' 'the reporter unexpectedly accepted a missing GITHUB_RUN_ID' >&2
    exit 1
fi
assert_contains 'GITHUB_RUN_ID' "$test_root/step8.out"

printf '%s\n' 'macOS nightly tracking-issue tests passed'
