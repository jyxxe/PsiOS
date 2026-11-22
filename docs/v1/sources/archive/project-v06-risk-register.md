# PsiOS — ADR 与风险闭合台账（v0.6 项目导入版）

文档日期：2026-11-22。性质：设计规范与实施待办，不是已实现或已通过验收的声明。

## 权威与术语

- 统一规划（Space）：https://chatgpt.com/space/page_e72f183122b08191ac04e6990c6c35b1
- ADR / 风险台账（Space）：https://chatgpt.com/space/page_c1cd937805a0819191a370c09f64f5ae
- ADR-21 = Agent Chat；ADR-22 = PsiFlow；ADR-23 = Project 外部事实同步；ADR-24 = 跨服务事务/审批/Saga；ADR-25 = Workflow/Chat 持久数据与容量。
- WorkflowRun = 一个工作流实例；Run = 一个 Task 的一次执行尝试；WorkflowInstance 是历史名称别名，不新建权威数据库。
- 风险分类：`SpecResolved`（已有设计级约束）≠ `ADRAccepted`（技术方案获批）≠ `ImplementationPassed`（源码与实测通过）。本文件所有工程项默认 NotVerified/NotRun。

## R-01 / P0 — 旧备份恢复与权限撤销防回滚

无法凭早于撤权事件的唯一一份备份证明撤权已发生。无外部可信安全状态时，系统不得宣称能够恢复最新撤销记录。设计 `RecoverySecurityMode = VerifiedCurrent / Quarantined / ReEnrolled`：安全状态可信新鲜时恢复现有身份；不可信时只允许最小本地认证与诊断，不自动重新启用旧 Agent 授权、服务令牌、集群节点身份、远端会话、自动化写任务；系统所有者重新登记并轮换凭据。可选独立可信存储或见证服务用于核验 epoch，但不能成为本机救援依赖。旧远端凭据的撤销必须联系相应远端授权权威完成。

验收：系统盘全损、唯一旧备份、无网络/见证时进入隔离模式；确认旧授权无法继续发起新高风险操作，本地数据合法恢复通道仍在。

## R-02 / P0 — Workflow 与 Task 跨服务提交

Workflow 在权威存储中持久写入 DispatchIntent+outbox，字段包括 workflow_run_id、step_key、attempt_generation、source_revision、input_digest、delegation、idempotency_key。Task Broker 以 `(origin_ref,idempotency_key)` 去重并返回已持久接受的同一个 task_id；请求摘要不同拒绝；结果回执遗失时 Workflow 必须先查重对账，不能直接创建新的 Task。跨服务没有通用 ACID 保证；外部不可幂等副作用失败时保持 OutcomeUnknown 并核实，而不是盲目重试。

验收：outbox 提交前/后崩溃、Task Accepted ACK 丢失、Workflow 链接写入失败、重复消息、去重保留期结束仍不会无声重放危险操作。

## R-03 / P0 — 跨节点群聊唯一写入权威

首版每个 Conversation 单权威写入者，权威持久存储保存 writer_epoch、conversation_seq 和提交日志。写入者变更需存储端 fencing 或证明旧写入者不能继续写；ACK 只能在消息/引用/ACL 处理持久化后作为 `Accepted(seq)` 发送。分区侧可暂存 Pending，不得分配已确认的永久序号；没有可证 fencing 则停止新的确认写入。单个 Conversation 内可排序，跨 Conversation 不承诺全序。

验收：双节点同时成为候选写者、分区、旧 writer 回连、ACK 丢失、客户端重试、离线堆积；不能双主确认或丢失已确认消息。

## R-04 / P0 — Prompt Injection 与有权代理的误用（Confused Deputy）

仅检查 Tool Capability 不足以保证行为符合用户目的。高影响操作绑定不可变 OperationPlan（真实发起者、允许目标、操作目的、效果类型、数据外发限制、权限代次、审批、制品摘要和成本预算）。Issue、README、网页、聊天、Skills、模型输出等标记为来源明确的不可信输入，不能自行扩权或覆盖计划。超出目标范围的操作需拒绝或重新审批；自然语言语义匹配不能绝对保证免受所有诱导，残余风险依靠沙箱、最小权限、人工复核和日志控制。

验收：有合法文件写权限的 Agent 被恶意 README 诱导改动别的项目或向外部网络传输保密数据；计划约束阻止该类越界效果。

## R-05 / P0 — 撤权落实到实际数据面

按后端暴露 `RevocationMode = GateOnly / StreamCut / SandboxTerminate / ResourceFence / ExternalNonRevocable`。撤权返回按对象分列的 RevocationResult；关闭 Token 不等于撤销已打开的文件描述符、卷挂载、设备句柄或远端外部副作用。实际不支持即时撤权时报告 Partial/RecoveryRequired、禁止危险继续执行；资源端 fencing 或隔离沙箱终止后才可声明真正完成。

验收：持有现存 fd、GUI Viewer、挂载与设备访问的工作负载被撤权，报告与强制机制一致。

## R-06 / P1 — MCP 现代规范兼容

采用协议能力发现矩阵，分别测试现代 2026-07-28（无状态请求和更新的协议元信息/Tasks 扩展）与历史 2025-11-25 连接模式。检查授权服务、audience、issuer、token/连接边界。第三方 MCP Tasks 不是自动等价于 PsiOS Task，需显式映射。

验收：旧/新客户端成功协商或准确拒绝，不以旧会话的认证结果隐式授权新代际请求。

## R-07～15 / P1–P2 — 其他模块进入发布前的门槛

| ID | 修订设计 | 验收 |
|---|---|---|
| R-07 | Run 绑定模型/工具/Skill/Schema/输入摘要，恢复不兼容时新建 Run | 断点与模型版本切换 |
| R-08 | 受管理 HostExtension 提供宿主驱动/模块升级，校验内核 ABI、签名、许可证与恢复 | 驱动中断升级与回滚 |
| R-09 | PackageEnvironment 跟踪上游依赖与安全修复，重建不可变 OCI 制品，旧用户数据独立 | CVE 更新/脚本失败 |
| R-10 | 外部 Forgejo/GitHub/CI 字段级权威、revision、webhook 去重、周期对账，禁止盲双写 | 乱序/重复/丢失 webhook |
| R-11 | Workflow DSL 版本化 AST、纯表达式、稳定步骤 ID、显式有界循环和定义升级策略 | 动态子任务、重启、取消 |
| R-12 | Helium 轻量服务 Profile 和资源/容量硬限，基础救援不依赖重型服务 | 小内存/满盘/脱机 |
| R-13 | TableFS 与传统文件系统+Catalog/Index 同负载比较，达不到收益/可靠性门槛不得默认启用 | POSIX、WAL、导出和百万文件基准 |
| R-14 | Dataset 的 tag/score/vector/extra_data 类型与 NULL/版本规则在 ADR-13 冻结 | JSON 路径精确查询、巨型记录 |
| R-15 | KDE Portal/Wayland 实际授权协议、PipeWire serial、一次性恢复 token 与会话代次绑定 | 分辨率、锁屏、重连、权限撤销 |

## 跨模块验收扩展

原 v0.5 已覆盖 XAT-01～37。新增 XAT-38～45：旧备份无可信新鲜度；Task 接受 ACK 丢失；Conversation 双主分区；恶意数据触发有权代理越权；直接持有 fd/设备撤权；新旧 MCP 代际隔离；Agent Run/Workflow 版本兼容恢复；KDE Portal 流/恢复 token。每个测试必须记录 `source_sha, artifact_digest, edition, profile, test_environment, input, fault_point, expected, actual, log, data_checksum, reviewer, status`。

**未完成的工作**：选定实现后端、验证 RPO/RTO、落实容量数字、建立真实自动化和故障注入测试、审查本地 Forgejo HEAD。以上均不得在没有证据时标记为 Passed。
