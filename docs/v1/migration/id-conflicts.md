# 旧编号冲突、范围变化与历史勘误

## 1. 工作项编号不是跨来源全局 ID

以下差异直接来自[Space 35.16](../sources/archive/space-unified-v0.6.md)及[Project 实施路线](../sources/archive/project-v06-roadmap.md)。Project v0.6 合集还内嵌了同一 Project 路线，因此同一内容存在两份来源记录；不能把这两份副本当成独立完成证据。

| 原编号 | Space v0.6 | Project 路线/导入 v0.6 | v1 处置 |
|---|---|---|---|
| P0-01 | 核验 PsiOS 当前提交；建立 Task/Event/Broker/OCI/Identity/Storage/SDK/WebUI 模块接口矩阵 | 以 PsiOS 源码、组件版本、依赖许可证和验收证据为准 | 方案的生效取决于 PsiOS 当前实现证据与审查记录 |
| P0-02 | 建立 docs/architecture、docs/adr、api/schema、tests/contracts 骨架 | 同类骨架与 CI 证据 | 可合并目标；“目录存在”不等于 CI 或产品验收完成 |
| P0-03 | Principal/Grant/Delegation/Approval/Operation/**Resource/Event** 样例及统一错误码 | Principal/Grant/Delegation/Approval/Operation、**Task/Run/WorkflowRun/Conversation/Message** | 范围重叠但不相同；共同 Schema 工作必须取完整并集，不能选一边而遗漏另一边 |
| **P0-04** | **Task/Run、Event outbox、WorkflowRun、Message、WorkItem 的权威/状态机 ADR** | **SecurityState 受限灾难恢复、无可信撤权记录时隔离** | 同号不同工作，禁止按裸 ID 合并；分别归共同契约/编排和恢复安全工作 |
| **P0-05** | **最小宿主、离线恢复、OCI + Volume + 独立 Backup 闭环** | **Workflow→Task DispatchIntent/outbox/接受去重/ACK 丢失 PoC** | 同号不同工作；Space 主题更接近 Project P0-08，仍不能据相似就标已完成 |
| P0-06 | 无该编号工作项 | Conversation WriterEpoch、存储端 fencing 与分区确认 | 保留 Project 来源工作项；不捏造 Space 同号目标 |
| P0-07 | 无该编号工作项 | 用户目的/允许效果及 RevocationMode 撤权测试 | 保留并按 v1 集合式撤权能力、Accepted/Enforced 分离修订 |
| P0-08 | 无该编号工作项 | M1 引导/恢复及 M2 OCI + Volume + Backup | 保留；可在正式工作登记中与 Space P0-05 建立显式合并关系 |

历史工作项的引用键例如 `SRC-SPACE-V06::P0-04` 与 `SRC-IMPORT-ROADMAP::P0-04`。完整原句与来源行号保存在 [legacy-requirements.json](../registries/legacy-requirements.json) 的 `work_item_references`。同一来源中的多个引用仍指向该来源定义；新 Issue 必须分配独立当前 ID 并写明这些旧键，不能只贴一个 P0-04。

Space 中还存在 P1-01～07、P2-01～02；Project 路线对应主题有未编号列表。没有确切旧编号时不替它们补造编号。旧“建议按顺序开 Issue”不表示已创建 Issue、已有 owner、代码已实现或有人批准实施选型。

## 2. ADR-21/22 的编号纠错与 ADR-22/23 的范围拆分

[Space 风险台账 §0](../sources/archive/space-risk-v0.6.md) 明确记录“Space 旧 Page 对二者的反向编号作废”，并统一 **ADR-21 = Agent Chat，ADR-22 = PsiFlow**。取得的 v0.5、Project v0.6、合并 Space v0.6 已采用该编号。合并前 Page 只在这些来源中有链接，本包没有取得其独立全文；“曾反向编号”是来自已归档风险台账的明确陈述，不冒充本次直接逐字检查原 Page 的结果。

| 当前编号 | v1 主题权威 | 历史边界与处理 |
|---|---|---|
| ADR-21 | Conversation、Room/DM、成员、消息、投递、唤醒及相应后端 | 旧反向引用改回本主题；不根据数字相同把 Workflow 决策迁进来 |
| ADR-22 | PsiFlow 领域/DSL、WorkflowRun、步骤依赖、持久恢复、审批门及补偿 | v0.5 的同号条目还包括 Project 对象和外部仓库集成；此部分在 v1 分解到 ADR-23，保持依赖 |
| ADR-23 | Project/WorkItem 与 Forgejo/GitHub/CI 的字段权威、修订、冲突、Webhook 和对账 | 不覆盖 WorkflowRun 权威；外部真实 PR/CI/合并状态仍来自供应系统 |
| ADR-24 | 跨域提交、OperationPlan/审批消费、Saga、补偿及未知效果 | 不能把 outbox 当成通用跨服务 ACID 或任意外部 exactly-once |
| ADR-25 | Workflow/Chat 等持久状态、容量、保留/GC、扩展和跨域恢复 | 不另建安全事实权威，不在文档修订中批准 SQLite/PostgreSQL/Broker 选型 |

ADR-01～20 保留原主题连续性；所有 ADR 的实际批准状态必须来自当前决策登记，不能因为本包列出了编号而提升为 Accepted。v1 对旧 ADR-22 范围的拆分是责任澄清，旧 Project/Repo/CI 功能目标没有删除。

## 3. 原文里的计数和交叉引用错误

| 位置 | 原文问题 | 本包修正 |
|---|---|---|
| Space 35.15 尾段 | “v0.5 现有 29 个跨模块场景” | 实际 v0.5 有 XAT-01～37；v0.6 又补到45。保留全部45及12个WF-AT |
| Space 35.16 末段 | “可并行开展 ADR-22/22 的纯接口草案” | 应为 Chat 的 ADR-21 与 Workflow 的 ADR-22；保留原句以便查证，不把错误变成两个同号ADR |
| Space 第35章小节 | 从35.9开始，继承合并前结构 | v1按领域章节重排；旧section作为溯源，不虚构35.1～35.8正文 |
| Project 导入版标题v0.6 | 容易被当作Space完整逐字导出 | 明确它包含v0.5全文和v0.6风险/路线前言；独有Space章节另行取得并合并 |
| 原风险“SpecResolved/风险闭合” | 容易被理解成漏洞已修复、测试通过 | 只表示设计层有处理约束；实现NotVerified、验收NotRun |
| 模块命名与实现状态 | 模块名称不证明软件功能已经实现 | 以 PsiOS 自身源码、集成测试及功能验收结果为准 |

## 4. 版本变体与真正冲突分开

登记表按同一旧规范 ID 聚合多个版本，但保留每个定义块、出处和原句。`historical_definition_variant_count` 只表示逐字文本不同：修复链接、扩充验收、调整措辞都会让它增加，不能自动解释为逻辑矛盾。反过来，P0-04 两个来源即使优先级相同也确属不同工作，必须 source-qualified。

迁移状态 `Mapped` 只代表已经明确说明去向。对时间、权限、身份、未知效果和数据面保证的强化以 v1 为准；历史示例、旧状态名或较弱说明不能成为绕过新约束的兼容理由。不可自动转换的旧持久数据应进入受控迁移/恢复计划，而不是被静默丢弃。
