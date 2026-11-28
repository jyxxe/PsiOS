# Dual-line synchronization

PsiOS work is exchanged between an upstream source line and a downstream
Forgejo line. The two lines must keep a shared, append-only history so that
reviewers, issue mirrors and release evidence point at commits that still
exist. This guide describes the manual, auditable synchronization procedure; it
does not merge either line's pull requests. See
[Agent development](agent-development.md), [independent review](independent-review.md),
[quality gates](quality-gates.md) and [issue lifecycle](issue-lifecycle.md).

## Invariants

- Published commits are immutable. Synchronize with merge commits, not rebase,
  amend, reset or force-push.
- Every upstream commit that downstream claims to contain must be reachable from
  the downstream branch. Verify with `git merge-base --is-ancestor <sha> <branch>`.
- Line-specific commits (mirror metadata, local documentation fixes) are allowed,
  but they must not overwrite upstream intent or rewrite shared files silently.
- Stable issue and mirror IDs, not titles or timestamps, identify synchronized
  work. A re-import must update the same record, not create a duplicate.
- A synchronization is not a release approval. Tests and review still apply to
  the exact resulting commit.

## Inspect both lines before touching them

Use a dedicated operator workspace with push access to the downstream line and
read access to upstream. Never use an author's sandbox or embed a token in a
remote URL. Record both branch tips before starting:

```bash
git rev-parse <upstream-branch>
git rev-parse <downstream-branch>
git log --graph --oneline --decorate -20 <upstream-branch> <downstream-branch>
```

If the tips already share history, confirm the direction of the missing commits.
If they do not, stop and investigate before merging; never fabricate a shared
ancestor or start the line from an unrelated root.

## Bring upstream changes into the downstream line

Merge upstream into downstream with an explicit merge commit:

```bash
git switch <downstream-branch>
git fetch <upstream-remote>
git merge --no-ff --no-commit <upstream-remote>/<upstream-branch>
git status --short
```

Resolve conflicts, stage the result, rerun the relevant tests, then commit with a
message that names the source branch and source tip SHA. Do not squash the merge
and do not replay upstream commits with new hashes. If nothing needs to change,
prefer a fast-forward only when policy permits it; otherwise keep the merge
commit so the synchronization is visible and auditable. Downstream-to-upstream
propagation follows the same rule in the opposite direction.

## Find conflict candidates before resolving

Merge conflicts are legal signals, not noise. Treat these as candidates for
close review:

- the same source lines or generated documentation changed on both lines;
- competing schema, migration or format-version numbers;
- release notes, manifest or validation-report changes;
- renames or deletions on one line against edits on the other.

Trial the merge on a scratch branch and list unresolved paths:

```bash
git switch -c sync-trial <downstream-branch>
git merge --no-ff <upstream-remote>/<upstream-branch>
git diff --name-only --diff-filter=U
```

Resolve by preserving both lines' invariants. Regenerate derived files with the
generator documented in `docs/v1/tools/README.md` instead of hand-editing two
different copies. If the correct semantic result is unclear, leave the merge
uncommitted, open an issue with the conflicting paths and both commits, and do
not guess. Delete the trial branch only after the real merge is complete.

## Handle concurrent upstream changes

Fetch again immediately before pushing. If the downstream push is rejected as
non-fast-forward, the upstream tip advanced while you worked: merge the new
upstream tip into downstream, resolve, rerun the same tests and review the new
diff. Do not force-push, do not discard the newly arrived commits and do not push
a merge based on a stale upstream tip. Repeat until the push is a fast-forward
of the downstream branch.

## Deduplicate round trips

- Key issue mirrors by the stable upstream ID and store the source revision or
  content digest. On re-import, update the existing mirror only when the source
  revision is newer; otherwise leave it unchanged.
- Key imported comments and events by their source identifiers. A retry after a
  timeout must not create a second comment or a second issue.
- When normalized content is identical, do not create an empty synchronization
  commit. Record the source tip SHA and the resulting merge SHA in the operator's
  sync note.
- Do not mirror transient CI statuses, chat messages, credentials or private
  deployment details. Manual closure propagation is covered in the issue
  lifecycle guide.

## Verify the result

```bash
git merge-base --is-ancestor <upstream-sha> <downstream-branch>
git log --graph --oneline --decorate -10
git diff --stat <upstream-sha>..<downstream-branch>
git diff --check
```

The ancestor command must exit successfully; the diff should show only intended
line-specific work. Rerun the tests applicable to the changed paths and record
the exact merge SHA in the synchronization evidence.

Limitations: the repository does not ship an automatic bidirectional
synchronization service. This procedure is manual and its quality depends on the
operator. Text merging cannot prove semantic compatibility, and conflicts in
generated files must be regenerated rather than trusted. A leaked secret is an
incident: stop, report it, and follow owner-directed revocation rather than
rewriting shared history unilaterally.
