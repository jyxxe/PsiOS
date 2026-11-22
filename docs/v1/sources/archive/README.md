# 原始材料归档

这里保存本次实际取得的输入原字节。文件名改为 ASCII，内容没有编辑；原上传中文名、source_file_id、Space page_id/sequence、字节数和 SHA-256 均在[来源清单](../source-inventory.json)。这些归档是历史来源，不覆盖当前 v1 规范。

| 文件 | 来源 |
|---|---|
| [project-v01-2026-11-21.md](project-v01-2026-11-21.md) | v0.1 总体规划 |
| [project-v02-2026-11-21.md](project-v02-2026-11-21.md) | v0.2 架构补全 |
| [project-v03-tablefs-2026-11-22.md](project-v03-tablefs-2026-11-22.md) | v0.3 TableFS |
| [project-v04-packages-webgui-2026-11-22.md](project-v04-packages-webgui-2026-11-22.md) | v0.4 多软件包与WebGUI |
| [project-v05-chat-workflow-2026-11-22.md](project-v05-chat-workflow-2026-11-22.md) | v0.5 Agent Chat与PsiFlow |
| [project-v06-unified-import.md](project-v06-unified-import.md) | v0.6 原生Project导入合集 |
| [project-v06-risk-register.md](project-v06-risk-register.md) | v0.6 ADR与风险台账导入版 |
| [project-v06-roadmap.md](project-v06-roadmap.md) | Project实施路线与验收清单 |
| [project-import-guide.md](project-import-guide.md) | 项目说明与导入指南 |
| [project-instructions.txt](project-instructions.txt) | 原Project Instructions |
| [space-unified-v0.6.md](space-unified-v0.6.md) | Space完整合并规划的已取得正文导出 |
| [space-unified-v0.6.metadata.json](space-unified-v0.6.metadata.json) | 同次Space页面/序列/块摘要元数据 |
| [space-risk-v0.6.md](space-risk-v0.6.md) | Space ADR与风险台账正文导出 |
| [space-risk-v0.6.metadata.json](space-risk-v0.6.metadata.json) | 对应Space元数据 |
| [xelab-package-history.zip](xelab-package-history.zip) | 用户提供的历史XeLab/XenonLab-Old模块化归档，不是PsiOS v1软件发行包 |

原文件中的外部链接、旧编号、历史测试数字、未批准选型及说明保持原样，以便独立追溯。旧Project Instructions作为来源数据保存；当前v1工作约束以本包治理与用户实际授权为准。源码归档中的许可证和权利标识仍属于其各自原作者/依赖，不能因为与v1文档一起打包而被改写。

对代码ZIP仅做静态结构、清单及元数据检查；没有运行其脚本、测试、安装器或容器，也没有验证当前Forgejo HEAD。详见[复用边界](../../migration/xelab-reuse-boundary.md)。
