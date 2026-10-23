#!/usr/bin/env bash
# Fail a pull request that changes what runs in CI or in a contributor's editor,
# unless a maintainer has reviewed it and said so with a label.
#
# Run by the `workflow-guard` job in .github/workflows/ci.yml on
# `pull_request_target`, so it is ALWAYS this copy from the base branch. A pull
# request cannot delete or weaken the guard that is judging it. That is the
# point; under plain `pull_request` the PR's own ci.yml runs, and a PR that adds
# a malicious workflow could simply remove the guard in the same commit.
#
# It never checks out or executes pull-request code. File names and diffs from
# the PR are attacker-controlled DATA: they go through jq and grep, never eval,
# and are printed only with workflow-command processing switched off.
#
# Flags, by path (old or new name, any status including deletion):
#   .github/workflows/  .github/actions/  .github/scripts/   (what CI executes)
#   any .vscode/ directory, at any depth                    (what the editor executes)
# and by content:
#   tasks.json / settings.json / *.code-workspace anywhere whose diff ADDS
#   `runOn` (e.g. "runOn": "folderOpen" — code that runs when the folder opens).
#
# Override: the label `workflow-change-reviewed`, counted only when the person
# who most recently applied it is an admin or maintainer of the repository.
# Anyone with triage rights can add a label; only those roles count. A push to
# the PR after review removes the label, because the reviewed code is no longer
# the code being merged.
#
# Env: GH_TOKEN, REPO (owner/name), PR_NUMBER, EVENT_ACTION, LABEL (optional).
# Tested by .github/scripts/test-workflow-change-guard.sh.
set -euo pipefail

LABEL="${LABEL:-workflow-change-reviewed}"
ACTION="${EVENT_ACTION:-}"
: "${REPO:?REPO is required}" "${PR_NUMBER:?PR_NUMBER is required}"
[[ "$PR_NUMBER" =~ ^[0-9]+$ ]] || { echo "::error::bad PR_NUMBER"; exit 1; }
[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "::error::bad REPO"; exit 1; }

fail() { echo "::error title=Workflow change guard::$*"; exit 1; }
api() { gh api "$@"; }

# Any API failure is a FAIL, not a pass: a guard that waves things through when
# it cannot see them is not a guard.
pr_json="$(api "repos/$REPO/pulls/$PR_NUMBER")" || fail "could not read pull request #$PR_NUMBER"
files_json="$(api --paginate "repos/$REPO/pulls/$PR_NUMBER/files?per_page=100")" \
  || fail "could not list the files of pull request #$PR_NUMBER"

expected="$(jq -r '.changed_files // 0' <<< "$pr_json")"
listed="$(jq -s 'add // [] | length' <<< "$files_json")"
# The files API stops at 3000 entries. Anything it did not list was not checked.
[[ "$listed" -ge "$expected" ]] \
  || fail "the API listed $listed of $expected changed files; the rest cannot be checked"

# One line per finding: "<path>\t<why>". Paths are JSON-encoded so a newline or
# tab in a file name cannot forge a second finding or hide one.
findings="$(jq -r -s '
  def guarded: test("^\\.github/(workflows|actions|scripts)/") or test("(^|/)\\.vscode/");
  def editor_config: test("(^|/)(tasks|settings)\\.json$") or test("\\.code-workspace$");
  add // [] | .[]
  | . as $f
  | ([.filename, (.previous_filename // empty)] | map(select(guarded))) as $hit
  | if ($hit | length) > 0 then
      "\($hit[0] | tojson)\t\($f.status): executed by CI or by the editor"
    elif (.filename | editor_config) and .status != "removed" then
      if (.patch // null) == null then
        "\(.filename | tojson)\tno diff available, so a runOn trigger cannot be ruled out"
      elif (.patch | split("\n") | any(startswith("+") and test("runOn"))) then
        "\(.filename | tojson)\tadds a runOn trigger (code that runs when the folder opens)"
      else empty end
    else empty end
' <<< "$files_json")"

if [[ -z "$findings" ]]; then
  echo "No workflow, CI-script or editor-automation changes."
  exit 0
fi

# File names are PR-controlled. A name like "::add-mask::x" printed at the start
# of a line would be obeyed by the runner; stop-commands makes it inert text.
stop="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
echo "This pull request changes what CI or a contributor's editor executes:"
echo "::stop-commands::$stop"
printf '%s\n' "$findings" | sed 's/^/  /'
echo "::$stop::"

labels="$(api --paginate "repos/$REPO/issues/$PR_NUMBER/labels?per_page=100")" \
  || fail "could not read the labels of pull request #$PR_NUMBER"
has_label="$(jq -r -s --arg l "$LABEL" '[add // [] | .[].name] | index($l) != null' <<< "$labels")"

if [[ "$has_label" != "true" ]]; then
  fail "A maintainer must review these changes and apply the '$LABEL' label."
fi

# New commits after the review: the label vouched for code that is no longer
# what would be merged. Take it off so the reviewer has to look again.
if [[ "$ACTION" == "synchronize" ]]; then
  enc="$(jq -rn --arg l "$LABEL" '$l | @uri')"
  if ! api -X DELETE "repos/$REPO/issues/$PR_NUMBER/labels/$enc" > /dev/null; then
    echo "::warning::could not remove the stale '$LABEL' label — remove it by hand."
  fi
  fail "New commits were pushed after '$LABEL' was applied. The label was removed; re-apply it after reviewing the new changes."
fi

events="$(api --paginate "repos/$REPO/issues/$PR_NUMBER/events?per_page=100")" \
  || fail "could not read the label history of pull request #$PR_NUMBER"
labeler="$(jq -r -s --arg l "$LABEL" '
  [add // [] | .[] | select(.event == "labeled" and .label.name == $l) | .actor.login] | last // ""
' <<< "$events")"
[[ "$labeler" =~ ^[A-Za-z0-9-]+$ ]] \
  || fail "could not tell who applied '$LABEL' (got '${labeler:0:40}'); a maintainer must re-apply it."

perm="$(api "repos/$REPO/collaborators/$labeler/permission")" \
  || fail "could not read @$labeler's role on $REPO"
role="$(jq -r '.role_name // .permission // ""' <<< "$perm")"
case "$role" in
  admin|maintain)
    echo "Reviewed: '$LABEL' applied by @$labeler ($role)."
    ;;
  *)
    fail "'$LABEL' was applied by @$labeler, whose role is '${role:-none}'. Only an admin or maintainer's label counts; one of them must remove and re-apply it."
    ;;
esac
