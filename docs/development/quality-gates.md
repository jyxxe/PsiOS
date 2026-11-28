# Quality gates for Agent changes

A pull request is eligible for the controlled merge step only when the exact
candidate has complete, current evidence. Local checks are useful preliminary
signals; they are not the repository's required CI. The authoritative list of
required checks is the protected-branch configuration for the target branch,
with the workflow files showing which jobs exist. See
[Agent development](agent-development.md), [independent review](independent-review.md),
[dual-line synchronization](dual-line-sync.md) and [issue lifecycle](issue-lifecycle.md).

## Know the repository workflows

At the time of writing, `.github/workflows/core.yml` runs for pull requests and
for pushes to `main`, and defines the `linux`, `early-format-migration`,
`work-item-migration`, `oci-archive`, `host-package`, `guest-boot`,
`native-virtio-install`, `package-provenance` and `verify-provenance` jobs.
`.github/workflows/portable.yml` also runs for pull requests and `main`, and
defines `portable-core`, `rescue-disk`, `local-inference`, `attest` and `verify`.
Read the current files instead of copying this list blindly: jobs and branch
protection can change, and only the protected-branch configuration decides which
checks are required for a merge.

## Preliminary checks versus complete CI

Run the fast local checks before opening or updating a PR:

```bash
git diff --check
cargo +1.100.0 fmt --all --check
cargo +1.100.0 clippy --locked --workspace --all-targets -- -D warnings
cargo +1.100.0 test --locked --workspace
python3 docs/releases/restore_v1.py --verify-current
python3 -m venv /tmp/psios-docs
/tmp/psios-docs/bin/pip install -r docs/v1/tools/requirements.txt
/tmp/psios-docs/bin/python docs/v1/tools/validate_package.py
```

These commands are preliminary. The ordinary 1 CPU/1 GiB/10 GiB sandbox cannot
execute the workflows' privileged and environment-specific steps, such as
namespaces, OCI engines, systemd service replacement, network trials, browser
tests, installers, provenance and guest boot. Complete CI means every required
job for the exact head SHA has run in the configured runner and concluded
success. Do not describe unpublished local checks as equivalent.

## Treat skipped, cancelled and missing as not green

Only a job conclusion of success counts. A skipped, neutral, cancelled, failed,
timed-out, queued or absent required job fails the gate. A repeated run does not
retroactively make an older failed run pass; the evidence must be the run for the
candidate being merged. If a job is skipped because its `if` condition or path
filter did not match, verify whether policy still requires its coverage rather
than assuming the skip is harmless. Infrastructure outages are reported as
blocked, not converted into exceptions.

## Check candidate head and base freshness

1. Confirm the PR head equals the commit you reviewed and tested:
   `git rev-parse HEAD` in the candidate checkout must match the PR head SHA.
2. Confirm the required-check run points at that same head SHA. A run for a
   previous commit is not evidence for a newer one.
3. Confirm the target base you reviewed has not moved. Fetch the target branch
   from the remote that holds it, compare `git rev-parse
   <target-remote>/<target-branch>` with the reviewed base SHA, and require that
   fetched tip to be contained in the candidate with
   `git merge-base --is-ancestor <target-remote>/<target-branch> HEAD`. If either
   check differs, bring the current base into the branch with a merge commit,
   rerun tests, and ask for a fresh review. Never force-push to fake freshness;
   an ancestor check against a stale `<reviewed-base-sha>` stays true after the
   target advances.
4. If workflow files, the test baseline or branch protection changed, treat the
   approval and check set as invalidated until re-reviewed.

## Fail closed

Merge only when all of the following hold for one immutable candidate:

- the author's branch was not rewritten after review;
- the independent reviewer approved this head SHA and base SHA;
- every required check for that head SHA concluded success;
- the target base is current and its protected-branch rules are satisfied;
- no unresolved secret exposure, scope violation or fabricated evidence remains;
- the PR body records commands, results and checks not run honestly.

If any condition is unknown, the result is no merge. Do not use administrator
overrides, rerun a red job until it flakes green, delete a failing test, edit a
workflow to skip coverage, or merge with an "unrelated failure" note. Those
changes would weaken the gate and belong in a separately reviewed change, not in
the feature PR. Merging is a separate controlled operation; the developer and
reviewer must not merge their own work.

## Record the evidence

Use placeholders in notes, never real credentials or private infrastructure IDs:

| Field | Example value |
|---|---|
| PR | `#<number>` |
| Candidate head / base | `<head-sha>` / `<base-sha>` |
| Local checks | commands and outcomes, or `NotRun` |
| Required CI | every required job's conclusion for the same head SHA |
| Limitations | privileged/hardware checks not run in the sandbox |

Limitations: this checkout can show workflow definitions but not the live
protected-branch configuration, so the operator must read the current required
checks before merging. Workflow text may drift from the job names listed above.
Static documentation validation and a green CI run remain limited evidence, not
product acceptance or release approval.
