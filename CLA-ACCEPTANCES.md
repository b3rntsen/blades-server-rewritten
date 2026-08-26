# CLA acceptances

Who has accepted [`CLA.md`](CLA.md), and when.

Written by `.github/workflows/cla.yml`, which blocks a merge until the author
has accepted and then appends the row itself. Rows may also be added by hand;
the format is the same and the workflow will not duplicate one.

The authoritative record of any acceptance is the contributor's own comment on
their pull request, which GitHub attributes and timestamps. This file is the
index over those comments so nobody has to search for them, and it is kept in
this repository rather than with a third-party CLA service because it is the
evidence that the project has the right to relicense.

A contributor's acceptance is retroactive to everything they contributed before
signing (CLA section 1), so each person appears once.

| Contributor (GitHub) | CLA version | Accepted | Where |
|---|---|---|---|
| _none yet_ | | | |

## Before merging a first-time contributor

The `CLA` check does this for you: it fails until the author has commented the
acceptance line, then records them. Make it a required status check on `main`
so a merge cannot go around it.

If you are adding a row by hand — an acceptance given by email, say — cite
where it was given in the last column.

## People who do not need to be here

**Marius DAVID** — his 75 commits predate the fork and arrived under the MIT
licence, which already permits relicensing them under any terms. He is not
covered by this CLA and does not need to be; see `NOTICE`.

<!-- cla workflow smoke test, branch is deleted after -->
