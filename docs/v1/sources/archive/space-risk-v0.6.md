# PsiOS｜ADR 与风险关闭台账

**修订基线：** 已保存 v0.5（2026-11-22）+ Space 统一规划 Page。**状态含义：** `SpecResolved` = 已提出足以实施/检验的规范约束；`ADRPending` = 关键实现选型待批准；`PoCRequired` = 尚未实测；`Verified` = 绑定明确代码提交、测试产物、硬件/环境及复核人方可填写。本页所有工程验证均为 **NotRun / NotVerified**，不将文档更正冒充系统漏洞修复。

## 0. 规范一致性修正

| 条目 | 统一决定 | 处置 |
|---|---|---|
| ADR-21/ADR-22 | **ADR-21 = Agent Chat；ADR-22 = PsiFlow**。Space 旧 Page 对二者的反向编号作废 | SpecResolved，统一引用 |
| Workflow 名称 | **WorkflowRun** 是持久编排实例；`Run` 是 Task 的执行尝试；旧 `WorkflowInstance` 只作迁移别名，不生成第二份权威状态 | SpecResolved，Schema 待 M0 验证 |
| 消息模型 | 以 v0.5 CHAT-004 基础字段为准，增补 `idempotency_key`、`delivery_id`、`conversation_seq`、`sender_epoch` 和服务器认证上下文，不可信请求体不能自报身份 | SpecResolved，Schema 待 M0 |
| WFL-014 交叉引用 | 消息后端引用 ADR-21；PsiFlow/Project 后端引用 ADR-22，交叉资源提交引用 ADR-24 | SpecResolved |
| 原 Page 版本 | v0.5 是合并前最新完整规范；v0.6 是本轮合并设计稿，不是实现版本 | SpecResolved |
| MCP 文档 | 新兼容基线评估 `2026-07-28`，并在适配器声明是否支持旧 `2025-11-25`；不能将两种 wire era 伪装成同一会话 | ADR-09 待兼容测试 |

## R-01｜旧备份恢复与 SecurityState 回滚（P0）

**问题：** 备份 T1、撤权 T2、彻底丢失全部可信状态 T3 时，恢复 T1 无法凭本地备份推断 T2 的撤销。旧稿不得无条件承诺这种场景下撤销自动保持。

**规定 `RecoverySecurityMode`：** `VerifiedCurrent`（经独立可信锚/见证或连续可信存储验证策略代次）；`Quarantined`（不能证明新鲜度）；`ReEnrolled`（所有者重新建立新信任代次）。默认从旧备份恢复到 Quarantined：仅本地受控救援账户可用于诊断/重新登记；旧 Agent Grant、OIDC/服务令牌、远程会话、集群证书和自动化写任务均暂停或失效。必须显式重新建立 Node/Principal 凭据与新 `SecurityRecoveryEpoch`，其范围外的旧凭据是否仍可被远端接受，必须由远端重新注册/轮换处理，不能假定重建本机会自动撤销世界各地已发出的离线授权。

**退出：** 验证旧凭据失败、失联且无可信锚时仍可本地救援、加密数据需合法恢复材料、新 Epoch 防止旧消息与旧 Worker 重放。`XAT-06/07` 扩展为全盘丢失与无外部锚点场景。

## R-02｜Workflow→Task 跨服务确认裂缝（P0）

**规则：** Workflow 在本权威存储事务中创建不可变 `DispatchIntent` 和 Outbox（含 `workflow_run_id`、`step_key`、`attempt_generation`、`input_digest`、`policy_epoch`、`origin_ref`、`idempotency_key`）；Task Engine 对 `(origin_ref,idempotency_key)` 的同一请求**持久去重并返回相同 `task_id`**，参数摘要不同则 Conflict。执行受理后 Task 先落盘 `Accepted`，然后返回 Task ID；Workflow 可能丢失回执，恢复时必须先按 `origin_ref` 查询/对账既有任务，不能仅因超时创建新任务。去重的保留必须覆盖可重试窗口，超过窗口仍能通过墓碑或对账拒绝旧请求。

**跨服务不是一个全局 ACID 事务。** 对外部不可幂等操作维持 `OutcomeUnknown`；补偿失败保留部分完成证据。单独 `DispatchIntent` 是 Workflow 的待执行意图，不是第二个权威 Task。测试注入 outbox 写入前后、Task 接受成功但回执丢失、Workflow 保存链接失败、幂等保留过期、消费者重复领取。

## R-03｜跨节点 Conversation 追加顺序（P0）

**首期选择单个 Conversation 的单权威写入者模型（设计级默认，存储引擎待 ADR-21 实测）**。每个 Room 具有 durable `conversation_id`、`writer_epoch`、`next_seq`，只有经过资源写入端 fencing 的当前写入者可分配序号并确认。消息在正文引用、ACL 决策和消息元数据持久提交后才返回 `Accepted(seq)`。分区侧可保留 `Pending` 发送请求但不得显示已经提交的正式序号；客户端用每主体/会话幂等键重连对账，未确认消息不能被当作可信历史或触发正式 Task。故障切换要求旧写者已被存储端拒绝，或人工执行可靠隔离；无可证 fencing 时暂停写入而不是双主接受。只保证单 Conversation 顺序，不承诺跨 Room 全序。

测试：分区同时发送、双主抢写、ACK 丢失、旧 Epoch 重连、离线消息积压和受限容量。未来 CRDT 多写另立 ADR 和语义迁移。

## R-04｜Prompt Injection 与 Confused Deputy（P0）

**权限检查是必要但不充分条件。** 高影响工具执行必须来自受认证的用户目标或明确授权 Workflow，绑定不可变 OperationPlan：目的/受影响资源、允许副作用类型、数据外发级别、来源证明、预算、审批签名、policy_epoch、输入与制品摘要。网页、Issue、仓库 README、Skill/聊天/模型输出统一标记 `untrusted_data` 及来源链；不得由其自行提升 Grant、扩大目标或把私有数据送至另一 Provider。若模型提出的行动超出已授权目的与效果范围，重新审批或拒绝；不依赖模型识别所有恶意指令。

测试：合法权限 Agent 被恶意文件诱导提交跨项目变更、隐式网络外发或变更保护分支；操作应因目的/目标/审批版本不符合而受阻。承认语义目的检查无法形式化覆盖所有自然语言，剩余高风险由人工审查与沙箱承接。

## R-05｜撤权强制能力和报告语义（P0）

为能力声明 `RevocationMode`：`GateOnly`（禁止下一次 API/续租）、`StreamCut`（停止流/输入）、`SandboxTerminate`（终止并确认运行环境）、`ResourceFence`（持久资源端屏障）、`ExternalNonRevocable`（已发生外部效果不可撤销）。撤权操作返回每个受影响主体/句柄/会话/卷的 `RevocationResult` 和确认状态；不得凭撤回 API Token 宣称已关闭已打开的 fd/挂载、GPU/设备写入或外部副作用。撤权超时须为 `Partial`/`RecoveryRequired` 并阻断新高风险操作，具体延迟预算由 ADR-04/06 实测。离线路径必须有失效期限与数据面 fence；不能凭控制面过期自动推断旧进程终止。

## R-06｜MCP 协议代际与适配（P1）

官方发布 [MCP 2026-07-28](https://blog.modelcontextprotocol.io/posts/2026-07-28/)：stateless wire core、逐请求 `_meta`/版本、可选 `server/discover`、`Mcp-Method` / `Mcp-Name` 路由、MRTR、Tasks 扩展、授权校验收紧；旧 2025-11-25 handshake/session 为兼容时代。PsiOS 的 MCP Adapter 必须先识别版本和能力，分别支持/明确拒绝，**不从旧 Session 隐式授权现代请求**。OAuth 令牌绑定签发方与受众，验证 issuer、scope、重定向和消息访问范围；不同 MCP Server 的 token 不可相互透传。Task 扩展 ≠ PsiOS Task，必须明确映射。现有接口和 SDK 是否均支持现代规范仍需实测。

## R-07～R-15｜其余已辨识风险的关闭条件（P1/P2）

| ID | 强制补充合同 | 所需验收 |
|---|---|---|
| R-07 Agent 恢复版本 | Run 绑定模型提供者/模型版本、Skill/Tool Schema、输入/输出 digest、状态快照和数据访问政策；不兼容则创建新 Run/等待人工，而非假定逐 Token 续跑 | 模型切换、上下文恢复与撤权测试 |
| R-08 宿主扩展 | 镜像式 Host 增加受管理 `HostExtension`：签名来源/许可证、内核 ABI/驱动版本、重建与回滚、安全升级和救援；普通应用不直接写宿主根 | 驱动/内核模块及更新失败恢复 |
| R-09 包安全更新 | PackageEnvironment 建立上游 CVE/仓库依赖索引；上游修复触发不可变制品重建、回归与受控重部署，数据 Schema 独立迁移；旧版本可禁用且不破坏用户数据 | 漏洞修复/镜像漂移/安装脚本失败 |
| R-10 外部仓库事实 | ExternalLink 记录 `provider/tenant/repo/object/revision/observed_at/owner` 与每字段 authority；Webhook 与周期性 read-reconcile，修订冲突返回待处理，不双向盲覆写 | Issue/PR/CI 重排、误删、失去权限 |
| R-11 DSL 可验证性 | WorkflowDefinition 采用定版 AST/Schema；DAG 节点稳定 ID，循环由 `repeat` 或动态任务显式预算控制，表达式无 I/O/网络副作用，定义升级不修改在途 WorkflowRun | 递归、版本跨越、暂停恢复、超预算 |
| R-12 Helium 预算 | 最小 Agent/Chat/Task API 与 Host 救援不依赖重型 Project/Model/DB，提供逐角色 CPU/RAM/磁盘/队列/日志预算和背压；资源阈值只在目标硬件实测后填 | 小内存、磁盘满、断网退化 |
| R-13 TableFS 决策 | 与普通 FS + 可选 Catalog/Index 同负载 A/B：POSIX、WAL 崩溃、数据校验、尾延迟、写放大、备份/导出；收益不足不得替代普通卷 | XAT-21～23，ACC-TFS-01～08 |
| R-14 Dataset Schema | 固定逻辑字段，但 `tag/score/vector/extra_data` 精确类型、NULL、Schema 迁移、JSON 路径和单行大型记录上限需 ADR-13；不得将初始草案字段解释为物理 SQL 列型已选定 | 精确 JSON 查询、超大记录、索引撤权 |
| R-15 GUI 会话 | Wayland Portal 选定后端实测；屏幕流以当前会话/代次绑定，PipeWire 优先 `pipewire-serial`；restore_token 单次使用后轮换。控制权转移与捕获撤销不因重连复活 | 显示器热插拔、锁屏、Portal 拒绝与双输入 |

## 决策、验收和冻结策略

本轮仅关闭 **规范缺口**，不声称修复或验证代码。`R-01~R-05` 是阻断 M0 契约冻结的设计门槛；`R-06` 阻断 MCP 兼容声明；`R-07~R-15` 在相应模块启用/发布前通过测试。每条建立 ADR 与可自动化的故障注入、产物摘要及明确 reviewer；数据页和主规划引用本页但不让本页产生第二套 Task/Project 权威。

