# PsiOS documentation

**Latest documentation baseline:** v1.0.0 (2026-11-22), design-only. This GitHub repository does not yet demonstrate a deployed or tested operating system.

## Start here

- [v1 complete documentation index](v1/README.md)
- [Complete Markdown specification](v1/PsiOS-v1-complete.md)
- [17 current specification chapters](v1/docs/00-reading-and-authority.md)
- [ADRs (proposals)](v1/adr/README.md)
- [Machine-readable schemas](v1/schemas/README.md)
- [Acceptance plans and evidence](v1/acceptance/README.md)
- [Full file inventory](v1/DIRECTORY.md)
- [Source and errata migration](v1/migration/findings.md)
- [Documentation validation report](v1/validation-report.json)
- [Offline HTML reader](v1/html/index.html)

The v1 package is preserved under `docs/v1/` with its **original internal relative links**.

## Getting the original ZIP and two oversized source files

The exact original 5,566,788-byte ZIP is stored byte-for-byte as five Git blobs in [releases](releases/README.md). It can be rebuilt offline. Two unusually large **historical/registry files** (`registries/legacy-requirements.json` and `sources/archive/xelab-package-history.zip`) are stored inside that ZIP rather than as ordinary single GitHub files. All current normative chapters, ADRs, schemas, tests, examples and the complete Markdown/HTML reader are directly browsable in GitHub.

From the repository root:

```bash
python3 docs/releases/restore_v1.py --verify-all --write-zip
```

This restores the two large files to their original paths, verifies the full package manifest and creates the original ZIP in `docs/releases/`. See [GitHub layout notes](v1/GITHUB_LAYOUT.md). No network access, models, containers or agent are needed for reconstruction.

Historical materials in `docs/v1/sources/archive/` are **provenance-only**, not current approved API or code reuse decisions. Implementation, hardware acceptance and fault injection remain `NotVerified / NotRun` unless accompanied by actual reproducible evidence.
