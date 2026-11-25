# PsiOS documentation

The v1.0.0 release dated 2026-11-22 records the design baseline. The [implementation and verification matrix](implementation-status.md) tracks the running software, installation targets, limits and evidence.


## 同版本自主编排增补

GitHub `main` 上 v1.0.0 的 Agent/Task/Chat/PsiFlow 分章已增补 Router–Worker、自主触发、订阅关联与恢复约束，版本号 `1.0.0` 保持不变。请参阅 [Agent 第 9.10 节](v1/docs/09-agents-models-protocols.md)。原始分段 ZIP 为增补前封存快照；当前 GitHub 文档不同于该 ZIP。此前 validation-report.json 仅记录增补前的检查；本次 Shell 修订重新生成当前文档的静态报告，其结果不代表产品验收。原始规范包的历史验收标记保留。当前软件的验证记录随实现单独更新。

## Shell 浏览器桌面（2026-11-25）

PsiOS Shell 定位为类似 macOS 的浏览器桌面工作环境：菜单栏、Dock、应用启动器、多窗口、工作区和全局搜索构成主要交互，系统管理通过独立应用提供。详见[第 07 章第 5 节](v1/docs/07-applications-packages-gui.md#5-psios-shell浏览器桌面工作环境)、[实施路线](v1/docs/14-roadmap-and-decisions.md)和[实际实现状态](implementation-status.md)。当前本机查询界面不等于完整桌面已实现。

本次同步当前分章、需求与测试登记表、合订本、HTML 和校验清单。`docs/releases/` 中的原始分段 ZIP 保持原始交付身份；当前 GitHub 树按当前 MANIFEST 与静态检查报告核验，产品新增验收仍为 NotRun。

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
python3 docs/releases/restore_v1.py --verify-current --write-zip
```

This restores the two large files to their original paths, verifies the full package manifest and creates the original ZIP in `docs/releases/`. See [GitHub layout notes](v1/GITHUB_LAYOUT.md). No network access, models, containers or agent are needed for reconstruction.

Historical materials in `docs/v1/sources/archive/` are **provenance-only**, not current approved API or code reuse decisions. Implementation, hardware acceptance and fault injection remain `NotVerified / NotRun` unless accompanied by actual reproducible evidence.
