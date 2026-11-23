# PsiOS v1.0.0 original delivery archive

The exact original `PsiOS-v1.0.0-docs-2026-11-22.zip` (5,566,788 bytes; SHA-256 `b6f2c5510063c1304b11bd623cd159e1155f706dd1c6b8398c1753b1763127c3`) is stored as **five consecutive raw binary parts**:

| Part | Bytes | SHA-256 |
|---|---:|---|
| `.part01` | 1,200,000 | `b78eb179b2bc68ba1323446f34c3ecd919d25dad21de43fa0f87feac1ca973e8` |
| `.part02` | 1,200,000 | `8a9876bc1cea61ec943d7cea2dc8efb902dc0861305a59c035370ebc95970722` |
| `.part03` | 1,200,000 | `0c49015765c430c0267a9ea9795e6402b1d78070e057192cdc2fa4d8c947f9ed` |
| `.part04` | 1,200,000 | `f13f1910983a34824fa152f2f83e4f5d2d1c95537a688ab73e678c049adca88b` |
| `.part05` | 766,788 | `d71f45e79c9c1c8b2800a23efd282b3d5e50b32fdfe99d414a44faaa2d54bb4c` |

Rebuild and restore the two large files into `docs/v1/` by running from the repository root:

```bash
python3 docs/releases/restore_v1.py --verify-current --write-zip
```

The operation is **offline** and uses only Python's standard library. It checks each part and the complete ZIP. With `--verify-current` it validates the original files inside the ZIP against the sealed archive manifest and the checked-out documents against the manifest in the selected Git revision. Use `--verify-all` when a checkout must match every original delivery entry byte for byte. It refuses to overwrite existing files whose bytes differ. It never runs the archived XeLab code.

For a manual reconstruction without Python, concatenate the files **as binary bytes in numerical order**, then compare the reconstructed ZIP SHA-256 against the value above. Do not append text newlines.
