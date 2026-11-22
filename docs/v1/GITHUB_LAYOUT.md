# GitHub mirror layout for the PsiOS v1.0.0 documentation release

The authoritative delivered ZIP is `PsiOS-v1.0.0-docs-2026-11-22.zip`, SHA-256:

`b6f2c5510063c1304b11bd623cd159e1155f706dd1c6b8398c1753b1763127c3`

All source ZIP relative paths are mirrored under `docs/v1/`, preserving the original links and original bytes. GitHub connector upload limits required two oversized, non-normative source/registry entries to be **stored within an exact byte-split archive** instead of as individually browsable blobs:

- `docs/v1/registries/legacy-requirements.json` (5,591,545 bytes)
- `docs/v1/sources/archive/xelab-package-history.zip` (3,174,454 bytes)

Every other v1 source ZIP entry, including all 17 normative chapters, 25 ADRs, 33 JSON Schemas, acceptance material, 35 positive and 35 negative examples, full Markdown compilation and the offline HTML reader, is directly committed to GitHub. The two missing entries are not omitted from the delivered **data**: they reside in the split original ZIP and can be recovered without changes to content.

Run from the repository root:

```sh
python3 docs/releases/restore_v1.py --verify-all --write-zip
```

This verifies all five archive parts, reconstructs the exact original ZIP, checks ZIP CRC, safely restores the two oversized entries without overwriting modified files, and verifies the 211-file SHA-256 manifest. Recovery itself does not execute historical source code.

**Authority:** read `docs/v1/docs/00-reading-and-authority.md` first. Historical source files under `sources/archive/` are for provenance and do not override v1 normative constraints or approve any technology/implementation.

**Status:** all actual PsiOS product acceptance tests remain `NotRun` / `NotVerified`. This repository import certifies the document bytes only, not runtime behavior.
