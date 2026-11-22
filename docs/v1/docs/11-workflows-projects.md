# 11｜PsiFlow、Project 与外部事实同步

**基线：** PsiOS v1 文档设计；承接 WFL-001～014、R-02/R-10/R-11 与 ADR-21～25 主题划分。**状态：** 本文给出目标语义，未选定实现引擎、数据库或外部连接器，工程状态 `NotVerified / NotRun`。

## 11.1 职责和数据权威

PsiFlow 负责可恢复的编排；Task/Run 负责执行；Operation 保存具体效果事实；Conversation 保存交流；Project 管理长期目标与证据关联。Project Service、复杂看板和 Workflow Studio 为可选能力，基础 Task/调度与最小工作流 API 不依赖它们的 UI 或大型中心控制面。

| 对象 | 规范定义 | 写入权威 |
|---|---|---|
| Project | 安全域、成员、目标、预算、仓库/存储/文档引用的长期工作空间 | Project Service |
| Milestone | Project 内的一组目标和退出准则 | Project Service |
| WorkItem | 可独立指派和验收的需求、Issue 或交付项，可关联多个 Task/WorkflowRun | Project Service；外部字段按 ExternalLink 单独定权 |
| WorkflowDefinition | 输入/输出 Schema、控制结构、步骤、预算和策略的不可变版本 | Workflow Definition Registry |
| WorkflowRun | 某 Definition 版本与输入快照的一次编排实例 | Workflow Engine |
| WorkflowStepInstance | 已实例化的步骤节点，绑定 step_id、迭代路径、输入与派发记录 | Workflow Engine；不复制 Task/Operation 结果权威 |
| DispatchIntent | 从步骤实例向 Broker 提交任务的持久意图 | Workflow 权威保存；Broker 独立确认接受 |
| ApprovalRequest / ApprovalDecision | 待判定请求 / 已判定不可变决策 | Security/Approval 权威；Workflow 只引用和等待 |
| ExternalLink | 本地资源与外部对象及字段权威的版本映射 | Integration Hub 管理映射；外部状态仍以对应提供者为准 |

**V1-WFL-001（必须）**：所有对象具有稳定 ID、安全域、所有者、Schema 版本、创建/更新责任主体和资源修订。`WorkflowInstance` 仅作为历史迁移别名映射到 WorkflowRun；`Run` 始终只表示 Task 执行尝试。Project 群成员、Project 成员、Workflow 操作权限和执行资源 Grant 各自裁决，不能无限传递。

## 11.2 WorkflowDefinition v1 的最小语言

**V1-WFL-002（必须）**：Definition 以版本化 AST 为语义权威。JSON 或 YAML 是导入格式，不允许 YAML 自定义对象构造、任意代码标签或字符串 eval。表达式使用经登记的有类型纯函数子集，限定 AST 节点数、深度、计算步数与输出大小；没有文件、网络、时间读取、随机数和动态代码执行。需要外部值时先通过正式 Task 取值并持久记录，再作为输入求值。

定义至少包含：`definition_id`、`definition_version`、`definition_digest`、`dsl_version`、输入/输出 Schema、稳定步骤 ID、依赖边、控制结构、能力选择器、失败/重试/补偿策略、审批计划、根预算、截止/并发限制和输出规则。未知安全字段、未知节点类型和无法解释的扩展必须拒绝；不能忽略后按更宽松策略执行。

| 节点种类 | 确切语义 |
|---|---|
| TaskStep | 绑定任务类型、输入映射、允许能力及权限引用；通过 DispatchIntent 提交 Broker |
| Condition | 对已持久输入进行纯布尔/枚举求值，保存所选分支与输入摘要；恢复不重新取“最新数据”改变选择 |
| Parallel | 同时释放有限个子节点；并发额度在共同预算账本预留 |
| Join | 声明汇合策略、必需输入与失败/跳过处理，不能仅数完成通知 |
| ApprovalGate | 引用具体 OperationPlan/审批请求，等待权威判定及可用执行许可 |
| WaitInput / WaitEvent | 等待特定身份和 Schema 的输入，或指定事件条件；带期限、去重和取消语义 |
| BoundedLoop | 显式包裹有限子图，声明最大迭代数、总时限和预算；每次迭代产生不同步骤实例键 |
| SubWorkflow / Map | 固定或受限展开子图，锁定子定义版本、输入快照与最大数量；禁止无界动态生成 |

基础依赖图必须无环。循环只能通过 BoundedLoop 语义表示，其展开图仍有有限、可追踪的实例；“DAG 支持循环”不再作为未解释措辞。动态 Map 在展开时保存输入清单摘要及元素顺序，不从不断变化的外部列表重新决定任务数量。

**V1-WFL-003（必须）**：编译/登记时检测重复步骤 ID、悬空依赖、非法回边、永远不能满足的 Join、输入输出类型不匹配、无界循环/展开、未声明权限、没有失败路径的审批、无法解释的补偿和不可满足预算。通过语法/类型检查只证明定义可解释，不证明业务正确或效果安全。

## 11.3 步骤实例、尝试与幂等身份

| 身份 | 构成及变化规则 |
|---|---|
| Definition 步骤 ID | 在该定义版本内稳定，不由展示序号或标题推断 |
| `step_key` | WorkflowRun + 步骤 ID + 明确迭代/Map/子图路径形成的稳定实例键 |
| `attempt_generation` | Workflow 决定重新执行已结束业务步骤时增加；必须先确认可重试及效果安全 |
| Task ID | Broker 接受一次步骤派发后生成；同次派发网络重传始终得到同一 ID |
| Run ID | Broker 为 Task 的执行尝试分配；Task 内部运行重试不等于 Workflow 新步骤派发 |
| Operation ID | 一个具体业务效果的身份；与 HTTP 请求重传、Run 替换分开 |

**V1-WFL-004（必须）**：Workflow 在本地权威事务中写步骤派发状态、DispatchIntent、预算预留引用和 outbox。Broker 接受后再持久链接 Task ID。ACK 丢失、链接提交失败、引擎重启只做原键对账，不增加 attempt_generation。步骤输入、目标或允许效果改变时创建明确的新业务尝试并重新授权；旧未知效果必须保持阻塞。

Workflow 读取任务结果时绑定 Task/Run/Artifact 的确切版本。订阅事件只提示“可能有更新”，不能凭聊天中的“做完了”或过期缓存推进依赖。已回收结果显式返回 ResultEvicted；如果下游需要正文，必须由保留关系预先保护或转等待恢复，不悄悄重新执行生产任务。

## 11.4 生命周期、分支与汇合

**V1-WFL-005（必须）**：WorkflowRun 的生命周期为 `Draft / Ready / Running / WaitingApproval / WaitingInput / Blocked / Cancelling / Compensating / Succeeded / Failed / Cancelled`。Draft 尚未运行，Ready 已通过定义检查与准入；Waiting* 是已知等待，Blocked 包括缺资源、未知效果、版本不兼容和需人工核实。旧 `Completed` 对应成功时迁移为 Succeeded，不能把任何“执行结束”都转换为成功。

WorkflowRun 同时具有 `outcome_knowledge=NotStarted/InFlight/Known/Unknown` 和条件列表，语义沿用第 08 章。Unknown 阻止依赖释放及新的不安全效果；任务取消或 Workflow 终止后仍可 Unknown。Succeeded 要求 outcome_knowledge 为 Known、必需输出和成功条件满足、没有未核实效果，也没有未处理补偿失败。

步骤实例生命周期为 `Pending / Ready / Dispatching / Running / WaitingApproval / WaitingInput / Skipped / Succeeded / Failed / Cancelling / Cancelled / Compensating / Compensated / CompensationFailed`。资源等待/OutcomeUnknown 通过条件表达。Skipped 仅用于经过持久化分支选择明确不执行的节点，不得用来掩盖失败或丢消息；Compensated 表示执行过补偿，不等于从未发生原效果。

| Join 策略 | 满足条件 | 失败/跳过规则 |
|---|---|---|
| AllSuccess（默认） | 所有被激活的必需前驱成功并且结果已确认 | 激活前驱失败则失败/阻塞；未选择分支可按定义标为不参与 |
| AllSettled | 所有已激活前驱均结束且效果知识已确认 | 必须提供接收成功/失败/取消结果的类型化汇总规则；结束不自动等于业务成功 |
| AnySuccess | 存在一个已确认成功的候选，并持久选定获胜者 | 仅在定义已处理其余分支取消/结果/副作用后释放要求单一效果的后继；Unknown 的败者仍阻塞 |

**V1-WFL-006（必须）**：并行“先成功者获胜”不能假装消除了其他已运行分支的效果。适用场景应优先限制为只读竞争；涉及写入时必须在计划中允许多项效果或提供明确补偿/核实流程。Join 不因重复终态事件计数两次，也不能在一个必需前驱永久 Pending 时无限沉默等待；有期限和诊断原因。

## 11.5 审批只使用一个权威

**V1-WFL-007（必须）**：ApprovalGate 创建或引用 Security 权威的 ApprovalRequest。ApprovalDecision 是不可变判定，包含计划摘要、允许效果、主体范围、期限及条件。旧 `Approval` 名称是 Decision 加使用账本的兼容视图，不维护第二套“工作流审批成功”数据库来授予权限。

单次决策使用由权威 CAS 绑定 `operation_id`；重试同一 Operation 不重复消费，绑定之后不自动返还给其他操作。执行许可 ExecutionPermit 由权威在当前策略下签发，短期绑定 Operation、计划摘要、授权代次和效果前提。Workflow 持久记“门已获判定”只是进度事实；真正执行前仍须验证许可、撤权和资源前提。

审批通知丢失通过请求 ID 查询恢复；审批消息重复不产生多份许可。审批后目标 Commit、参数、权限增量或效果相关依赖改变则原计划不再适用。普通进度、心跳或非效果字段改变不应反复使审批失效。动态子图只能在已批准的明确范围内扩展，不能以“批准整个工作流”获取未界定的任意操作权。

审批和远端写入不能凭两次 API 调用承诺跨服务原子性；远端支持条件操作时执行条件写，不支持时根据风险拒绝自动执行或进入专用人工接管流程。Agent 投票、AI Review、群聊“同意”只能作为证据，不替代正式决策。

## 11.6 暂停、取消、重试与补偿

**V1-WFL-008（必须）**：暂停阻止释放新步骤；取消持久关闭取消域并逐一处理派发中/已接受 Task，采用 Task 章节的 CancelOrigin 墓碑与对账协议。关闭浏览器、成员离线、模型额度耗尽不是取消请求。取消时显示在途动作、仍未确认的外部效果、停止确认和人工核实入口。

重试由结构化错误类别、预算、最大次数、总期限和效果状态共同决定。PermissionDenied、ApprovalInvalidated、SchemaVersionUnsupported、IdempotencyKeyExpired 和 OutcomeUnknown 不得按普通瞬时错误自动循环。退避具有上限并遵守原截止时间；新尝试不重置根预算。人工手动重试也必须核实旧效果，不能只换幂等键绕过保护。

**V1-WFL-009（必须）**：补偿计划在原步骤被接受前登记其可补偿范围、执行者权限、输入依赖和失败处理。默认按已完成效果的依赖反向顺序执行；独立效果可在明确无冲突且预算足够时并行补偿。补偿自身有 Operation ID、幂等和未知结果处理；失败进入 CompensationFailed/RecoveryRequired，保留具体残留项。原效果不可逆时声明不可补偿，不承诺全事务回滚。

成功、失败、取消与补偿结论在 UI 分开表达。例如“执行已取消，两个外部写入已确认，一项待核实”优于简单“取消成功”。恢复时只推进已持久确认的状态，不能从头重跑所有步骤。

## 11.7 版本、保留与运行恢复

**V1-WFL-010（必须）**：运行固定 Definition 版本、DSL 解释器语义版本、输入快照和相关 Skill/工具版本。修改定义产生新版本，不原地改变在途实例。工作流状态迁移需显式计划，列明旧/新步骤映射、已派发 Task/Operation 处理、审批有效性、预算和回退边界；不兼容时阻塞原运行或创建新 WorkflowRun。

保留关系覆盖 Definition、输入、必要 Task 元数据、结果资产、审批及外部映射。删除 Conversation 不破坏这些引用；删除 Workflow 也不删除独立用户数据或远端 Repo。跨服务备份恢复使用共同恢复清单和对账屏障；缺少某步 Task 不代表可以重新派发，必须先查询提交账本及外部效果。

## 11.8 Project 与 WorkItem

**V1-WFL-011（必须）**：WorkItem 保存标题/描述、项目与里程碑、优先级、父子/阻塞关系、指派、验收条件、目标资产、文档、Task/WorkflowRun、外部 Issue 和交付证据。依赖图检测循环并显示阻塞原因；指派只改变责任归属，不自动授予文件、密钥或合并权限。

建议规范生命周期为 `Proposed / Ready / InProgress / Blocked / InReview / Verified / Closed / Abandoned`，允许项目定义带映射的附加状态。Verified 表示所列验收条件有证据，Closed 表示授权责任者接受结束；代码合并、CI 绿色、部署成功与用户验收分别记录。工作流成功不能自动把缺少验收条件的 WorkItem 标为 Verified。

所有者、指派者和独立审查者使用真实 Principal 绑定，不以显示名“Reviewer”证明独立性。职责分离可以要求不同人、不同授权域或不同执行身份，具体由策略声明；使用同一后台万能账号的两个 Agent 名称不能通过独立审批门禁。

## 11.9 外部字段权威与一致性

**V1-WFL-012（必须）**：ExternalLink 记录 `provider`、端点/实例身份、外部租户/仓库、对象种类和稳定外部 ID、本地资源引用、字段权威表、修订类型、最后权威读取版本/时间、同步状态、事件去重与写回关联。Forgejo 与 GitHub 同名仓库、不同实例的数字 Issue ID 不得碰撞。

| 字段 | 权威例子 | 本地行为 |
|---|---|---|
| PR 当前 head、merge 事实、保护规则 | 对应仓库平台 | 本地保存带版本的观察；合并前重新核查并使用条件约束 |
| CI 结论 | 实际运行 CI 的系统及其 run/check 身份 | 保存仓库、提交 SHA、工作流/检查身份、尝试号和完成结论，不仅保存绿色图标 |
| WorkflowRun 状态 | PsiFlow | 外部评论可反映进度，不反向覆盖本地运行事实 |
| 本地验收准则与验收状态 | Project Service 或登记的外部需求系统 | 选择一个字段权威；冲突进入人工/策略处理 |
| 标题、描述、标签等双向字段 | 每字段显式配置 | 有版本前提和冲突策略时才写回；禁止默认最后到达者覆盖 |

**V1-WFL-013（必须）**：外部 `revision` 不一定是可排序整数；Git SHA、ETag、随机版本 ID 不按字符串大小或时间戳比较新旧。适配器登记 `MonotonicSequence / OpaqueETag / CommitGraph / None` 等修订语义。Opaque 版本只做相等/条件写；Commit 关系须按仓库图或权威 API 验证；无可靠版本则回源对账并承认并发窗口。

Webhook 只是变更通知。验签、来源/目标匹配、去重、速率限制与重放策略通过后，查询权威源补齐现状；乱序或漏投由周期对账和增量扫描修复。只有提供者实际支持时间/签名等机制时才能声称相应保护，不能虚构所有平台都有全局递增事件版本。重复事件必须使用相同业务关联，连接器发出的写回事件不能无限触发自己。

写回使用提供者支持的条件操作和最新权限。没有条件写的字段默认保持外部单权威、本地只读或提交待人工确认的补丁；“先读再写”不能被描述为无竞争原子更新。外部成功而本地未收到回执，先按具体对象与 Operation 对账，不能重复合并、创建、删除或部署。

## 11.10 开发工作流的真实门禁

**V1-WFL-014（必须）**：参考流程为 Plan、Implement、CI、IndependentReview、ProtectedMerge、Deploy/Verify、Close；每一关绑定实际资源和版本。此流程是可配置 Definition 的示例，不是所有工作都必须走代码部署流程。

| 门禁 | 必须绑定的证据 |
|---|---|
| Plan | 目标与范围、允许仓库/路径、输入摘要、验收条件、OperationPlan |
| Implement | 隔离工作区、确切源分支/基线与提交 SHA、变更摘要 |
| CI | 同一仓库与确切提交的真实检查集合、结果、尝试号、制品摘要；允许失败/跳过项按策略明示 |
| IndependentReview | 可核验审查身份、审查绑定的 head SHA、结论与需要处理的意见 |
| ProtectedMerge | 当前权限、分支保护、目标 head/base 前提、所需 ApprovalDecision；执行前再次核查 |
| Deploy/Verify | 被批准的合并制品/摘要、目标环境、部署 Operation、运行/数据检查与恢复记录 |
| Close | 逐项验收条件、缺陷/限制、责任者确认和最终交付引用 |

PR head 在 CI/Review 后改变必须按策略失效旧证据；外部条件合并/受保护门禁决定最终结果。不能只因分支名相同、最后一次 Webhook 是成功或 Agent 声称“已经合并”就越关。权限不足、CI 缺失、来源不可达、审批失效都保留可解释阻塞状态，不自动降级保护。

## 11.11 验收和可选部署

M3 提供单机持久 DAG、审批等待、受控重试/取消、基础群聊/DM 和 Project 关联；M4 再验证跨节点与 Forgejo/CI 的闭环。复杂可视化、联邦和更多提供者可以后置，但接口与失败语义不能依赖 UI 才正确。

| 注入场景 | 通过标准 |
|---|---|
| 动态展开后引擎崩溃 | 使用原输入快照与步骤键恢复，不按新列表重复展开 |
| 未选分支与 Join | Skipped 明确不参与；必需失败不能被跳过假装成功 |
| AnySuccess 败者仍在写 | 先核实/取消/补偿败者，不能声称只有一个效果 |
| 审批后计划或 head 改变 | 原决策/许可不能执行新效果；非效果心跳不误使其失效 |
| 派发与取消交错 | CancelOrigin 与提交权威去重，未知提交不生成新键 |
| 补偿失败或超时 | 保留残留项与 Unknown，禁止写“完整回滚” |
| 外部 ETag/Git SHA 乱序 | 不使用字典序判断最新；回源权威读取与条件写 |
| CI 成功但 head 已变化 | 后续合并阻止，旧证据不冒充新提交通过 |
| 工作流/Task 备份不在同一恢复点 | 对账屏障阻止重派发，恢复引用并保留未知效果 |
| 未安装 Project/Studio | 本地 Task、Agent、最小工作流与 Chat 仍正常可用 |

以上覆盖 `ACC-WFL-01～05`、`WF-AT-04～08` 及原 XAT-33/35～37/39/44。ADR-22 负责 DSL/编排语义，ADR-23 负责外部同步，ADR-24 负责跨服务提交与审批/Saga，ADR-25 负责存储容量与跨服务恢复；任何实现方案获得批准都仍需独立提交和实测证据。

## 11.12 自主 Agent 开发与项目事件闭环

Router 是 PsiFlow 的计划与调度使用者，不能成为第二个 WorkflowRun、Approval 或 WorkItem 写入权威。Project/Studio 仍可选；未安装时本地 Task、Schedule、Agent 和最小 Workflow 可独立运行。

- **事件关联：** Forgejo/GitHub/CI 的 Webhook 先验签、过滤并回源查询权威状态，依据 ExternalLink 和修订关联已有 WorkItem、Task、WorkflowRun；重复/乱序/漏投事件经去重和周期对账处理。
- **Agent 分工：** Router 推荐 Plan/Implement/Review 等步骤，由已版本化 WorkflowDefinition 和 Task Broker 持久派发；子 Agent 共享预算及收窄委托，不通过聊天“同意”颁发审批。
- **交付门禁：** CI 与 IndependentReview 都绑定同一仓库、确切 head SHA、检查集合和审查身份；ProtectedMerge/Deploy 复查目标 head/base、实际授权、ApprovalDecision 和外部版本。无条件写能力时阻塞或人工接管，不绕过保护。
- **恢复：** Agent Session 变更不改变 WorkItem/Task 身份；替换 Worker 新建 Run，首先核实旧外部效果。PR 合并、评论发布或部署成功但回执丢失维持 Unknown，回源对账后才写结果。
- **成本：** Router 推理、各 Worker、群聊主动消息和重试均计入同一根预算；超限、权限不足和模型/Controller 离线时准确报告阻塞并允许用户暂停。

**增量验收（NotRun）：** 一次 CI 失败触发一个修复 Task；后续 Review 返回原 Worker；head 改变使旧证据失效；重复 Webhook 不双重合并；跨节点分区不双写；禁用 Project UI 后基本任务仍可用。
