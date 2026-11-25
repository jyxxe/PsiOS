# PsiOS v1｜完整规范与工程交付基线

**文档版本1.0.0 · 2026-11-22 · 设计基线，待工程验证**

本包整合v0.1～v0.6、Project导入材料、完整Space规划与风险台账，统一当前规则并保留历史来源。推荐先用[离线阅读器](html/index.html)浏览，或阅读[完整Markdown合订本](PsiOS-v1-complete.md)。精确文件目录见[DIRECTORY.md](DIRECTORY.md)。

v1是文档版本；当前没有宣称软件已发布、最新仓库已审计、硬件已通过或技术选型已经批准。

当前 Shell 方向为类似 macOS 的浏览器桌面；规范见[第 07 章第 5 节](docs/07-applications-packages-gui.md#5-psios-shell浏览器桌面工作环境)。新增桌面能力尚待实现与验收，修订历史见[CHANGELOG](CHANGELOG.md)。

## 本包包含什么

| 内容 | 实际数量/范围 |
|---|---|
| 独立规范章节 | 17章，含架构、共同合同、安全、恢复、存储、应用、Agent、Chat、Workflow、集群、发布、路线和词典 |
| v1规范条款 | 257条，唯一ID与验证计划可追踪 |
| 问题修正记录 | 89项：P0=40、P1=45、P2=4；均为规范层修正 |
| 机器合同 | 33份JSON Schema；关键请求严格拒绝未知字段 |
| 正反样例 | 35个正例、35个反例，另有摘要向量 |
| 产品验证计划 | 150个具体场景，全部NotRun；文档/治理与真实运行验证区分 |
| ADR / 工作项 / 操作手册 | 25份Proposed ADR、27个WI工作项、6份可执行顺序明确的runbook |
| 历史迁移 | 245个旧要求、141个旧测试均有文档去向 |
| 规划资料 | PsiOS 设计草案、Space 规划及验收记录；公开技术规范另见参考资料清单 |

映射率只表示有明确去向，Schema检查只验证结构和列明的局部关系；两者都不能当作产品通过率。

## 建议阅读顺序

1. **确定边界：** 00阅读与权威 → 01产品架构 → 02共同合同。

2. **开发具体模块：** 对应领域章节，同时阅读04安全、05恢复和16接口词典。

3. **安排工程：** 14路线 → adr → work-items → acceptance；先完成可恢复的M1/M2闭环。

4. **审查改动：** migration/findings → 旧要求映射 → 来源；不将历史源作为并列当前规范。

## 全部分章

| 文件 | 内容 |
|---|---|
| [00-reading-and-authority.md](docs/00-reading-and-authority.md) | 00｜阅读规则、权威与版本 |
| [01-product-and-architecture.md](docs/01-product-and-architecture.md) | 01｜产品边界与统一架构 |
| [02-common-contracts.md](docs/02-common-contracts.md) | 02｜身份、资源、接口与数据编码共同契约 |
| [03-host-updates-network-devices.md](docs/03-host-updates-network-devices.md) | 03｜宿主、安装、更新、网络与设备 |
| [04-security-identity.md](docs/04-security-identity.md) | 04　身份、授权、审批、宿主能力与撤权 |
| [05-recovery-consistency.md](docs/05-recovery-consistency.md) | 05　跨域一致性、备份恢复与安全重建 |
| [06-storage-data-tablefs.md](docs/06-storage-data-tablefs.md) | 06 存储、Dataset、Knowledge 与 TableFS |
| [07-applications-packages-gui.md](docs/07-applications-packages-gui.md) | 07 应用、软件包兼容、Computer Use 与 GUI Bridge |
| [08-tasks-events.md](docs/08-tasks-events.md) | 08｜Task、Run、Operation 与持久事件 |
| [09-agents-models-protocols.md](docs/09-agents-models-protocols.md) | 09｜原生 Agent、模型、Skills 与第三方协议 |
| [10-conversations.md](docs/10-conversations.md) | 10｜Conversation、群聊、私聊与 Agent 路由 |
| [11-workflows-projects.md](docs/11-workflows-projects.md) | 11｜PsiFlow、Project 与外部事实同步 |
| [12-cluster-coordination.md](docs/12-cluster-coordination.md) | 12｜集群、租约、资源fencing与节点生命周期 |
| [13-operations-release-governance.md](docs/13-operations-release-governance.md) | 13｜运维、观测、发行与开发治理 |
| [14-roadmap-and-decisions.md](docs/14-roadmap-and-decisions.md) | 14｜实施路线、决策门槛与工作项 |
| [15-acceptance-and-evidence.md](docs/15-acceptance-and-evidence.md) | 15｜验收方法、证据与发布判据 |
| [16-interface-catalog-and-glossary.md](docs/16-interface-catalog-and-glossary.md) | 16｜接口目录、字段词典与合同适用范围 |

## 关键修正

- 明确SecurityState、Task、Workflow、Conversation、Project与外部系统各自事实权威。

- 将审批请求、不可变决定、CAS使用账本及执行许可分开；批准不能自动代表效果已完成。

- 将生命周期、结果知识、结果可取性、健康和补偿分开；取消/失败可以仍有未知外部效果。

- 定义Task可靠提交、取消墓碑、Chat发送身份/单写日志、固定快照到watch和旧备份世代恢复。

- 保留Dataset通用JSON和原有业务字段；明确TableFS双入口、缓存、POSIX子集、GC与独立退出。

- 校正MCP代际/发现职责、A2A幂等能力边界、Portal版本与组合会话语义、包兼容证据集合。

- 合并编号与旧源冲突，保留全部旧验收目标，把未定的实现方案留在有具体实验的ADR中。


## 如何验证本包

安装tools/requirements.txt所列文档依赖后，从包根运行：

```bash
python tools/validate_package.py
```

无需联网即可解析 Schema，此验证不等于实际运行验收。实际静态检查结果见[validation-report.json](validation-report.json)；MANIFEST.sha256覆盖除自身外的所有交付文件。

完整生成/封装步骤见[工具说明](tools/README.md)。

## 实施起点与保留事项

下一工程入口为WI-001固定真实仓库HEAD、WI-003共同合同、WI-004/005授权与恢复原型、WI-006/007宿主与有状态应用恢复闭环。基底、数据库、默认文件系统、图形后端、性能数值和维护周期须按ADR取得真实证据。

三版定位、KDE方向、Agent/基础协作和可选数据服务等产品约束继续保留。此交付不自动创建外部Issue、改动Space页面或推送代码。
