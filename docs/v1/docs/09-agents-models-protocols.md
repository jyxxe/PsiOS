# 09｜原生 Agent、模型、Skills 与第三方协议

**基线：** PsiOS v1 文档设计；2026-11-22 官方资料核验。**工程状态：** `NotVerified / NotRun`。本章完整承接 AGT-001～011、R-06/R-07；Task、Operation、授权、桌面与存储使用相应章节的共同契约，不另建有特权的 Agent 专用通道。

## 9.1 组件责任及运行路径

| 组件 | 唯一责任 | 不拥有的权力 |
|---|---|---|
| Agent Registry | 身份绑定、能力/版本、可用性和兼容证据的登记 | 登记不构成资源授权，不证明代码可信或远程 Agent 处于本机沙箱 |
| Agent Runtime | 将任务输入、模型会话和工具步骤组织为可恢复 Run | 不能凭模型文字写 Task 终态或批准自身效果 |
| Agent Service Bus | 路由受认证请求、通知、可观察关联与背压 | 不替代 Task/Event 的持久提交，也不自动广播私聊正文 |
| Tool & Capability Gateway | 校验调用 Schema、效果分类、授权范围、计划与预算 | 不能因第三方协议已鉴权就跳过本地 Policy |
| Host Broker / 资源服务 | 执行获准的结构化宿主或资源变更，记录 Operation | 不接受普通 Agent 的任意 root shell 作为通用后门 |
| Model Gateway | 提供模型能力、路由、调用计量和数据出口控制 | 不负责授权 Agent、审批或持久任务状态 |
| Skill Registry | 管理说明文本、代码/工具依赖与来源版本 | Skill 文本本身没有授予权限的能力 |

**V1-AGT-001（必须）**：Task 进入统一 Broker 后，由调度器选择匹配能力的 Runtime/Adapter。模型输出先解析为结构化意图，再验证当前授权、OperationPlan、预算与效果前提，最后由获准执行者执行。模型失联、Agent 崩溃或聊天不可用不得阻止本地登录、诊断、存储检查和独立恢复。

`Agent` 表示一个稳定可授权主体及其受管理配置；`Session` 是该 Agent 的交互上下文；`Conversation` 是多个主体的沟通记录。一个 Session 可以关联若干 Task，但不能成为 Task 的唯一日志；一个 Conversation 也不能共享其全部成员的模型上下文。

## 9.2 执行版本、检查点与恢复

**V1-AGT-002（必须）**：每个 Run 记录以下执行清单，并在持久恢复时验证：

| 版本组 | 必须记录的内容 |
|---|---|
| 输入与计划 | 输入资产/消息的不可变版本及摘要，OperationPlan 摘要，目标资源效果指纹 |
| Agent 与 Runtime | 制品摘要、运行时版本、接口版本、隔离 Profile、相关 ABI |
| Skill 与工具 | Skill 文本/可执行依赖摘要，工具实现和输入/输出 Schema 版本 |
| 模型 | 提供者、模型标识、公开可得的修订/权重摘要、配置、支持能力和数据驻留约束 |
| 恢复 | 检查点格式版本、兼容矩阵、已提交 Step/Operation、待核实效果和保留关系 |
| 安全与资源 | 主体/委托链、授权代次、预算账本、取消域、资源租约及恢复世代 |

云模型若不提供不可变权重修订，应记录提供者返回的模型标识、调用时间和可见版本信息，并标记 `model_revision_assurance=ProviderReported`；不能虚构权重摘要或保证跨日完全复现。固定随机种子也不等于外部服务确定重放。

检查点保存已确认事实和可重建状态，不保存待无条件重放的工具命令列表。恢复前核查取消、授权、输入版本、工具实现、预算和实际资源。状态格式不兼容时拒绝原地继续；经批准的迁移必须产生新检查点版本及审计。更换执行器、跨节点重试或无法证明兼容的模型/工具语义变化创建新 Run；旧效果仍须先核实，不能因新 Run ID 而重做外部写入。

**V1-AGT-003（必须）**：运行日志保存目标、计划摘要、工具调用、返回值引用、执行证据和用户可见说明。不得以获取模型内部思维链作为运行、审计或验收的前提。流式 Token 是交互预览；最终结果须按 Artifact 规则验证并发布。

## 9.3 Skills 与工具边界

**V1-AGT-004（必须）**：Skill 清单将说明文本、工具依赖、可执行脚本、来源/签名、所申请能力、参数 Schema、版本与许可证分开记录。导入只使其可被发现；实际运行仍逐次执行授权。Skill 更新不扩大既有 Grant，删除 Skill 不级联删除任务证据或用户数据。多个 Skill 冲突时按明确版本与任务选择决定，不让外部内容自行宣布优先于系统策略。

工具清单至少给出输入/输出 Schema、大小限制、分页、只读/幂等/可补偿/不可安全重试效果分类、超时、取消方式、结果查询、所需权限和网络/数据出口。工具描述是可审查的声明，实际执行器必须强制相同约束。跨 API、CLI、SDK、Computer Use 的替代路径都受同一个目的和资源边界约束。

**V1-AGT-005（必须）**：网页、README、Issue、聊天、截图 OCR、工具结果和检索片段携带来源与安全域，按不可信数据处理。内容中的“已授权”“请上传密钥”“批准自己”等语句不改变策略。类型和计划限制可以阻止明确越界的效果，但不能保证自然语言目标的所有语义都被机器准确理解；高影响且语义不明的动作进入结构化审批/人工接管。失败处理不得改走权限更大的工具。

## 9.4 模型能力、数据与预算

**V1-AGT-006（必须）**：模型登记记录文本/图像/音频等输入输出模态、工具调用格式、上下文与输出上限、并发、流式与取消支持、部署位置、数据保留/出口约束，以及价格/额度信息的来源时间。任务声明允许提供者和区域、是否允许上传原文/图片、允许的最大成本与调用次数。实际请求先执行数据分类和出站策略；模型服务地址不能由检索内容随意改写。

模型切换是新的能力与数据策略裁决。本地模型故障时，`local_only` 数据不可自动发往云端；工具 Schema 不兼容、上下文截断可能丢失关键约束或多模态能力不足时暂停并解释。用户可预先授权同范围的备选模型，运行中遵守该边界，不要求对每次普通后续调用重复批准。

**V1-AGT-007（必须）**：父 Task/Workflow 的预算采用预留、计量和结算；重试、压缩、自动聊天、子 Agent 以及模型切换都计入共同根预算。显式计量 Token、费用、时长、并发和递归深度。估价与提供者账单事实分列；在途调用和账单延迟带来的最大暴露由并发/输出上限限制。预算不足拒绝新调用或暂停，不能侵占宿主诊断与救援配额。

长期记忆及摘要按主体、安全域、来源和保留关系隔离；权限在读取与向模型发送前检查。撤权后禁止再次检索/发送，不声称能够收回已经合法发送给外部提供者的数据。Dataset/Knowledge 未安装时可使用轻量会话存储和 Task 检查点；此基础能力不能硬依赖可选向量索引。

## 9.5 第三方接入的真实兼容范围

**V1-AGT-008（必须）**：兼容声明是“具体实现 + 版本/提交 + 传输 + 认证 + Profile + 方法/限制 + 通过用例”的元组，不能写成“兼容所有第三方 Agent”。C0/C1/C2 不是对每种对象都成立的线性认证等级。

| 能力等级 | 验证内容 | 不能推导的结论 |
|---|---|---|
| C0 受管理运行 | 本机运行、停止、日志、资源限制和隔离 Profile | 不能推导能够访问系统工具、恢复任务或参与群聊 |
| C1 受控能力接入 | 明确协议/CLI/SDK 的工具调用、授权与审计 | 不能推导可靠取消、持久结果、检查点或复杂委托 |
| C2 任务集成 | Task/Run/进度/制品/取消/恢复/委托的映射及故障测试 | 仅支持被测试的能力子集，不代表所有扩展均支持 |

远程 Agent 可以直接通过 C1/C2 适配，`execution_location=Remote`；C0 记为 NotApplicable，不得虚构本机沙箱保证。未知字段如果影响权限、效果或恢复必须拒绝；显示性扩展按版本规则处理。适配器升级前执行旧客户端、旧任务、取消与恢复回归。

## 9.6 各协议负责什么

| 协议 | PsiOS 接入用途 | 边界 |
|---|---|---|
| Model Context Protocol（MCP） | 工具、资源及相关交互能力 | 不提供 PsiOS 完整 Room 治理，不替代本地授权与代码沙箱 |
| Agent2Agent（A2A） | 独立 Agent 的发现、消息与任务互操作 | 远端 Task/context ID 不自动等于本地 Task/Conversation；不得假设发送天然幂等 |
| Agent Client Protocol（ACP-Client） | 客户端与 Agent 的会话、工具和交互 | 按具体协议版本映射；文件/终端请求同样需要 PsiOS 权限 |
| Agent Communication Protocol（ACP-Communication） | 有现实迁移需求时的历史适配 | 与 ACP-Client 是不同协议；官网已说明并入 A2A，默认不设第二套长期核心依赖 |
| Agent–User Interaction（AG-UI） | 前端与 Agent 的交互事件、进度与 UI 意图 | UI 事件不能直接成为审批/执行权威 |
| OpenAPI | 描述已有 HTTP API，生成受控工具封装 | API 描述本身不证明实现、安全、幂等或恢复能力 |

上述职责分别由 [A2A 官方规范](https://a2a-protocol.org/latest/specification/)、[Agent Client Protocol v1](https://agentclientprotocol.com/protocol/v1/overview)、[Agent Communication Protocol 官方迁移说明](https://agentcommunicationprotocol.dev/introduction/welcome)、[AG-UI 官方概览](https://docs.ag-ui.com/introduction)、[OpenAPI 官方规范](https://spec.openapis.org/oas/v3.2.1.html)支持。它们是外部机制资料，不是 PsiOS 已实现的证据。2026-11-22 查阅时 ACP-Client 文档区分 v1 Latest 与 v2 Preview；采用任何版本仍须锁定确切 Schema/SDK 提交，不把预览标签自动提升为稳定支持。

## 9.7 MCP 双代际兼容

**V1-AGT-009（必须）**：适配器将协议时代、传输、认证和业务能力分别登记。`2026-07-28` 采用每请求协议元信息，无传统初始化会话；服务端须实现 `server/discover`，客户端可选择先发现，也可直接请求并处理不支持版本错误。`2025-11-25` 属初始化握手模式。依据：[现代版本协商规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)、[旧版生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)。

| 检查点 | 现代 Profile | 历史 Profile |
|---|---|---|
| 协议上下文 | 每请求验证版本与能力声明 | 按固定版本验证 initialize/initialized 及连接状态 |
| 身份 | `_meta` 中自报 clientInfo 只是协议信息；Principal 仍从认证通道建立 | 连接建立即鉴权也不能永久替代每项授权和撤权检查 |
| HTTP 元信息 | 校验协议要求的头与正文映射一致，不能按可伪造路由头绕过动作检查 | 按旧传输规范处理，不将旧会话 ID 当成现代请求授权 |
| 重连 | 无会话不意味着工具写入可无条件重发 | 会话恢复也不意味着旧请求从未执行 |
| 长任务 | 仅在该请求声明并支持 Tasks 扩展时使用现代任务形态 | 旧实验任务方法须独立实现和验证，不套用现代任务格式 |
| 不支持 | 明确协议/能力错误；降级须保持效果、数据与授权边界 | 不自动接受现代字段后忽略其安全语义 |

现代协议无状态描述的是传输/请求上下文，**不会消除 PsiOS Task、幂等键、审批、Operation 和外部任务映射的持久化需求**。业务状态仍落在对应权威服务。[MCP 传输规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports)还区分请求流的终止与传输取消；关闭浏览器或响应流不等于证实外部业务已回滚。

**V1-AGT-010（必须）**：HTTP OAuth 路径与 STDIO 进程凭据路径分开实现。PsiOS 对受保护资源强制自身认证/授权，不以协议允许匿名为理由放开。HTTP 检查资源 audience、签发者、作用域、有效期和当前策略，禁止直接把一个服务的令牌透传给另一个服务；STDIO 仅注入最小环境凭据、限制可执行制品/工作目录/继承 FD 和网络出口。来源：[MCP 2026 授权规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)。

授权响应的 issuer 校验按所锁定规范和服务端元数据执行，不能声称上游在所有情形都强制 `iss` 存在。PsiOS 可以要求更强的发行 Profile，但要作为自己的附加要求明示。认证发现地址、重定向和元数据抓取仍受 SSRF、DNS/代理出口和信任域检查。

## 9.8 外部 Task 到本地 Task 的映射

**V1-AGT-011（必须）**：持久映射键为 `(adapter_id, endpoint_identity, remote_tenant_scope, external_task_id)`，绑定本地 Task/Run/Operation 和协议版本。不能仅以远端短 Task ID 为全局主键；同一 ID 在不同端点、用户或协议时代不得串任务。

现代 MCP Tasks 扩展以服务器生成的任务句柄延后返回结果，公开方法包括 `tasks/get`、`tasks/update`、`tasks/cancel`。每次任务访问继续鉴权；任务 TTL 过期表示远端可能不再保存记录，不是“业务从未执行”。尤其 `completed` 可能携带 `isError: true` 工具结果，因此不能直接映射 PsiOS 成功。来源：[Tasks 2026-07-28 规范](https://tasks.extensions.modelcontextprotocol.io/specification/2026-07-28/tasks)。

| 外部观察 | 本地映射原则 |
|---|---|
| 正在处理/排队 | 本地保持运行/等待条件；记录观察时间，不能据此推断执行权威迁移 |
| 需要输入或授权 | 转 WaitingInput/WaitingApproval；外部提示文本不自动成为 ApprovalDecision |
| 协议完成且业务结果通过 | 核验结果类型、摘要及效果证据后记 Succeeded |
| 协议完成但业务报错 | 按业务错误记 Failed 或需恢复条件；禁止把“HTTP/协议正常结束”当成功 |
| 取消回执 | 记录对端声明与取消范围；仍核查已发生或在途效果 |
| 任务过期/被清理/查无对象 | 记 ResultUnavailable/对账条件；若效果未知，保留 Unknown |
| 通道断开或重启 | 先查原任务/Operation；无可靠幂等保证时不得重新发起效果 |

**V1-AGT-012（必须）**：A2A 发送是否幂等依据具体服务器契约和实测，不仅依据 `messageId`。官方规范将 Send Message 幂等表述为可选；客户端不能由协议名称推断 exactly-once。[A2A 幂等定义](https://a2a-protocol.org/latest/specification/#331-idempotency)。对远端推送验证来源、租户、任务映射、重放和目标地址；回调 URL 不能成为内网探测或密钥外发通道。

## 9.9 最低能力与验收

M3 至少完成一个真实独立第三方实现与原生 Agent 的同类工具任务；测试同一权限拒绝、取消、撤销、结果未知、预算耗尽及重启恢复。单纯启动进程仅证明 C0 对应子集。三版都具备原生 Agent 和第三方受控接入的能力；无本地大模型不使 Agent 接入失效，离线时明确云模型不可达。

| 测试 | 通过标准 |
|---|---|
| 第 62 步后崩溃 | 只恢复未确认工作，已完成效果不重复，版本不兼容明确阻止 |
| Skill 更新替换工具 | 已接受 Run 仍绑定原版本或经过显式迁移；权限不扩大 |
| 本地模型失败 | local_only 输入不会通过自动回退上传到云端 |
| 双协议时代 | 新旧握手、元信息、Tasks 方法分别成功或准确拒绝，身份不串用 |
| MCP completed + isError | 映射为业务失败，不能触发“成功后合并”等下游步骤 |
| A2A ACK 丢失 | 无经过证明的发送幂等时不盲重发；外部 Task 可对账 |
| 自报 clientInfo 冒名 | Principal 仍由认证通道建立，伪造信息无授权效果 |
| 云模型与上下文恢复 | 公开修订不完整时如实声明可复现性限制，不虚构完全相同运行 |

原 `AGT-T01～07`、`XAT-43/44` 和第三方兼容矩阵继续适用。扩展协议、完整群聊联邦和任意第三方框架的支持必须逐项增加证据；本章没有把 SDK、框架或服务版本锁定为已批准实现。

## 9.10 原生 Agent 自主编排（SuperAgent 类机制）

本节把 Router–Worker、主动任务发现、Ticket 跟踪和 Thread Subscription 作为 PsiOS 原生 Agent 的逻辑能力，不是直接部署 SuperAgent 原型，也不另建高权限控制平面。它是现行 v1.0.0 设计增补，不代表某个 ADR 已批准或任何运行实现已验证。

### 9.10.1 角色和权威

- **Router / Planner：** 按可见事件、用户目标、Agent 能力和任务关联提供计划、优先级、执行器建议；需要推理时才唤醒模型，不要求永久运行 LLM 会话。
- **Policy Evaluator：** 现有 Identity/Policy/Approval、预算和数据出口服务执行确定性授权；模型不能替系统签发权限。
- **Worker / Executor：** 可以是原生 Agent、经 C1/C2 能力验证的 DSH/Codex 等第三方 Agent 或远程 Agent；一律复用 Task/Run、Tool Gateway、沙箱和审计。
- **Watchdog：** 与模型独立的健康、超时、退避、租约和取消管理，不能以 Router 不在线为由扩大权限。

以上是职责角色，不是自带 root、跨用户文件访问、桌面控制或仓库合并权的独立 Principal 等级。

### 9.10.2 概念到 PsiOS 的映射

- `Ticket` → Core `Task`；如有 Project Service，长期工作由 `WorkItem` 关联 Task。禁止另建 Ticket 终态数据库。
- `Assignment` → Broker/Scheduler 领取与受限执行租约，关联 Agent、Task、Run、节点、隔离等级、代次和能力；调度决定不可由自然语言替代。
- `Mandate` → `Grant + Delegation + OperationPlan + ApprovalDecision/ExecutionPermit`，逐层约束操作范围、目的、预算、有效期与可继续委托的权限。
- `Trigger` → Event、Schedule、外部 Connector 的已验证来源事件；`Subscription` → Conversation 中任务/线程订阅，复用 Task/Event 去重与持久游标。
- `Session` → Agent 的模型交互上下文；不得充当 Task、Operation、审批或外部效果的唯一恢复日志。

### 9.10.3 主动任务闭环

事件来源先认证、过滤及去重；优先关联已有工作和原 Worker。新任务由预授权策略及正式 Task.Submit 接受。Router 可规划多 Agent 分工，但执行计划经确定性权限、预算、数据驻留与效果检查后，才由 Broker 分配独立 Run。Worker 可在群聊 @ 或 DM 协作，正式委托必须经 Task/Workflow API。Review/CI/Issue 后续变化回到已有订阅任务而不是反复新建 Agent；完成时根据可验证 Artifact、Operation 及外部权威记录结案。

### 9.10.4 安全、成本与回退

自主程度可以由未来 Profile 配置成只观察、提出候选、已授权范围内执行或逐阶段审批；这些为设计选项，具体 Schema/默认值仍由 ADR 决定。各层维持独立 Principal、最小沙箱、数据出口与目的约束、可撤销租约、统一根预算、并发/递归深度/唤醒跳数/冷却上限。群聊或恶意 README/PR/Skill 均不能更新授权、绕过人类接管或启动无界循环。节点断线、Router 重启、模型额度耗尽时从已提交任务与检查点恢复；旧写入者须经资源端 fencing，未知外部副作用不得盲重发。三版共享接口，Helium 可采用轻量事件驱动 Router，Neon 可部署长期控制服务，Argon 可接入 KDE 授权会话；均不把模型作为本机诊断和救援依赖。

### 9.10.5 增量验收计划（均 NotRun）

重复/乱序事件不增殖 Task；同一 PR Review 被原 Task/Worker 接续且重新检查成员权限；Agent A→B→A 的循环因根预算和跳数停止；恶意评论诱导越权被 Tool Gateway 拦截；两个 Worker 抢占时旧租约无法写入；Router 死亡和模型离线不重放已确认副作用；已发生外部写入但回执丢失保持 `OutcomeUnknown` 并对账；撤权按实际 RevocationMode 截断后续能力。M3 测单机，M4 才验证跨节点与 Forgejo/CI 闭环。沿用现有 AT-TSK、AT-AGT、AT-CHAT、AT-WFL、AT-CORE 分类，待正式注册新用例后才分配独立测试 ID。
