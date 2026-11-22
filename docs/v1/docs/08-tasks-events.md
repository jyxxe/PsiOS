# 08｜Task、Run、Operation 与持久事件

**文档基线：** PsiOS v1，2026-11-22。**性质：** 可实现、可检验的设计要求；实现状态 `NotVerified`，故障试验 `NotRun`。本文接续 AGT-001～005、EVT-001～007、R-02 与统一 API 契约。Task/Event 是共同 Core 的逻辑能力；同一契约不要求所有节点、租户和隔离级别使用同一个物理数据库。

## 8.1 对象与唯一写入权威

| 对象 | 精确定义 | 权威与边界 |
|---|---|---|
| `Task` | 一次经接受的目标及其不可变提交输入，包含目标主体、委托、预算、执行要求和取消域 | Task Broker 管理生命周期；外部框架不得直接覆盖其终态 |
| `Run` | 为同一 Task 分配的一次执行尝试 | Broker 分配 `run_id` 与领取代次；执行器提交观察和证据，Broker 验证后记账 |
| `Step` | 一个 Run 内可观察的执行步骤 | Run 中有序或显式依赖的步骤记录；不是 Workflow 的步骤定义 |
| `ToolCall` | 对某个版本能力接口的一次调用记录 | 记录输入摘要、结果和关联 Operation；网络重传不自动产生新业务 Operation |
| `Operation` | 一项受授权效果的持久意图、执行与结果记录 | 对应资源/效果服务管理；Task 引用它，不复制第二份可独立修改的效果事实 |
| `Artifact` | 经过校验和受控发布的结果资产 | Artifact/Storage 权威保存内容版本和引用；任务成功不意味着结果永久保存 |
| `Event` | 已提交事实的通知信封 | 产生事实的资源服务是事实权威；事件总线提供投递与游标 |
| `Job` | 人工、定时或事件触发的工作定义/入口 | 产生 Task；不能建立与 Task 互相覆盖的另一套执行状态 |

**V1-TSK-001（必须）**：`task_id`、`run_id`、`operation_id`、`event_id`、`artifact_id` 由各自权威分配且不得复用。资源身份包含签发域；显示名、节点地址、文件路径、外部 JSON-RPC `id` 不能充当这些身份。Task、Run、WorkflowRun 和 Operation 的版本不得互换。

`Task` 目标及提交输入一经接受不可原地替换。修正目标或输入须创建新 Task，并记录 `supersedes_task_id` 或 `retry_of_task_id`；旧 Task 的取消、结果和审计仍可查。状态/观察更新使用条件版本，不使不相关的审批计划失效。

## 8.2 提交请求、接受回执与查询

以下是语义字段表。统一采用 `schema_version: "1.0.0"`、snake_case 字段；安全域和调用主体以已认证通道为准。扩展字段必须在声明的 Schema 中出现。

### 提交 `Task.Submit`

| 字段 | 语义及验证 |
|---|---|
| `schema_version` | 请求契约版本，与文档版本、镜像版本、持久数据版本独立 |
| `origin_ref.authority_id` | 产生任务的来源权威，例如某 Workflow 服务；须与认证调用者的可代表范围匹配 |
| `origin_ref.origin_type` / `origin_id` | 来源种类与稳定身份，例如 WorkflowRun、Schedule、ChatDispatch、HumanRequest |
| `origin_ref.step_key` / `attempt_generation` | Workflow 来源时必需：稳定步骤实例键与业务派发尝试代次；网络重发不增加代次 |
| `idempotency_key` | 在指定来源、安全域和调用主体范围内唯一的提交键；不可用随机新键掩盖未知旧提交 |
| `request_digest` | 按第 02 章共同规则，对整个提交对象去掉 `request_digest` 后计算 JCS + SHA-256 域分离摘要；服务端重新计算；短期令牌、传输请求 ID 与回执本来就不属于该请求对象 |
| `input_ref` | 输入清单的资源 ID、不可变版本和内容摘要；不能只保存可变路径 |
| `task_type` | 已登记的处理类型及版本能力；未知类型拒绝 |
| `delegation_ref` | 权威委托引用；不是请求自报的权限列表 |
| `budget_ref` | 已预留或可原子预留的预算账本引用；不是调用者可以任意填写的额度余额 |
| `deadline_at` | 可选绝对截止时间；使用可信实时时间及偏差策略，恢复后不得重新获得完整旧期限 |
| `cancel_scope_id` | 父工作流/用户操作的取消域；取消继承规则必须已登记 |

**V1-TSK-002（必须）**：创建请求不携带尚未分配的 `task_id` 或 `run_id`。如确需导入既有身份，使用单独受控迁移接口。Broker 接受成功返回 `task_id`、`task_revision`、`accepted_at`、`request_digest`、`origin_ref`、`idempotency_key`、`deduplicated` 和 `durability_profile`。`Accepted` 只证明任务及必要元数据在声明持久化故障范围内已提交，不证明执行、效果或完成。

### 接受的提交点

**V1-TSK-003（必须）**：在一个本地事务或经过验证的等效协议中完成：提交去重判定、输入清单引用与保留关系、任务元数据容量预留、Task 记录、必要审计意图和 outbox。只有提交完成才能发出接受回执。队列达到任务数、输入引用字节、待保存结果预算或用户配额上限时返回 `QueueFull`/`QuotaExceeded`；禁止在内存中无限排队后先报接受。

输入清单可显式列出尚待上游生产的、版本或预期摘要已绑定的依赖。这种 Task 接受后进入 `WaitingResource`，不得创建可执行 Run。执行前必须验证依赖存在、摘要、当前读取权限和保留关系。普通可变路径不属于可等待依赖。跨服务内容 pin 不得假设与 Broker 数据库原子提交：使用可核验的保留凭据和幂等确认；不确定的 pin 进入对账，孤立 pin 按有界租期回收，不能在 Task 仍使用时过期删除。

### 幂等查询 `Task.LookupSubmission`

| 查询结果 | 精确含义 | 调用方允许的动作 |
|---|---|---|
| `Found` | 当前权威存在匹配提交记录，附同一个 Task ID 与摘要 | 链接该 Task；若摘要不同返回 `IdempotencyConflict` |
| `AbsentWithinWindow` | 当前权威的一致读证明：有效提交窗口内未接受此键，且没有取消/过期墓碑 | 使用同一来源、同一键和同一请求重传 |
| `Expired` | 该提交命名空间/有效期已结束，保留结果不足以断言未执行 | 停止自动提交；查询长保留账本、外部效果或人工核实 |
| `Indeterminate` | 权威不可达、恢复不完整、读取陈旧或事实不足 | 停止自动提交并保留待对账记录 |

**V1-TSK-004（必须）**：`NotFound` 的普通资源查询不等价于 `AbsentWithinWindow`。去重索引被 GC、Task 正文删除、旧备份恢复、从滞后副本查询均不能证明任务从未被接受。幂等命名空间必须有可验证有效期/代次或持续保留的拒绝墓碑；结束后拒绝旧键。Task 结果可回收，防重放记录不能随结果一起失去保护。

去重范围至少绑定 `security_domain_id + authenticated_requester + origin_ref.authority_id + origin_ref + idempotency_key`。对同一来源键改变输入、目标、业务参数、预算上限或允许效果，必须冲突；重新签发同一委托的短期传输凭证不改变业务摘要。返回重复回执仍须检查当前可见权限，不能向已撤权调用者暴露旧结果。

## 8.3 Workflow 跨服务派发与取消竞争

**V1-TSK-005（必须）**：Workflow 在自己的权威事务中写入 `DispatchIntent + outbox`。意图保存稳定来源键、请求摘要、输入引用、委托和预算引用、派发状态。Broker 在自己的权威事务中接受，随后 Workflow 条件更新该意图的 `task_id` 链接。两次提交之间存在故障窗口；使用去重与查询对账连接它们，不宣称跨服务 ACID。

| 故障位置 | 恢复动作 |
|---|---|
| Workflow 意图事务未提交 | 无可派发记录；恢复后按原步骤条件重新决定 |
| 意图已提交、尚未发送 | 检查取消/授权/期限后用原键发送 |
| Broker 已接受，ACK 丢失 | 先查询该来源键；Found 时链接原 Task |
| Workflow 收到回执但链接未提交 | 同上；不增加 `attempt_generation` |
| 外部效果已发生、Operation 回执丢失 | 进入 `outcome_knowledge=Unknown`，核实该 Operation；创建新 Task 不能消除未知效果 |
| 去重窗口已过期或灾备只恢复了部分账本 | 保持阻塞，执行跨服务恢复对账 |

**V1-TSK-006（必须）**：取消不能只删除 Workflow outbox。取消域在 Workflow 持久关闭后，派发器停止新发送，同时将已发送/未知提交与尚未发送提交逐一对账。Broker 提供按来源键取消/阻止接受的语义：若提交已存在则请求取消该 Task；若不存在则提交取消墓碑，后续旧 Submit 必须拒绝。取消墓碑与接受判定由同一个权威串行化。无法获得 Broker 确认时返回 `CancelPending` 条件，不能声称已停止全部任务。

执行器在领取、每个新效果和长等待恢复后检查取消域及有效授权。已经发出的外部请求仍可能成功。取消确认分别记录“停止派发”“禁止新效果”“执行环境已停止”“外部效果已核实”，不存在一个布尔量同时保证四者。

## 8.4 生命周期与结果知识分离

**V1-TSK-007（必须）**：所有任务查询同时返回生命周期 `lifecycle`、结果知识 `outcome_knowledge`、条件列表、结果引用状态和当前修订。`OutcomeUnknown` 是可机器读取的条件/错误，规范字段为 `outcome_knowledge=Unknown`，不放入普通失败重试分支。

| Task 生命周期 | 进入条件与退出方向 |
|---|---|
| `Accepted` | 持久接受；随后进入 Queued 或明确等待态 |
| `Queued` | 输入/授权/预算达到排队要求；领取后 Running |
| `Running` | 一个当前有效 Run 正在执行；可等待、暂停、取消或结束 |
| `WaitingInput` / `WaitingApproval` / `WaitingResource` | 分别等待结构化输入、权威审批或资源依赖；恢复前重新校验条件 |
| `Paused` | 用户暂停、预算耗尽或未知效果阻塞；原因必须明确 |
| `Cancelling` | 已记录取消请求，但仍有在途执行或停止确认 |
| `Succeeded` | 必须步骤完成、结果已持久发布或明确无结果，且不存在 Unknown |
| `Failed` | 不再执行本 Task 的新尝试；具体原因与效果知识仍分别保留 |
| `Cancelled` | 本 Task 执行流程已结束；不表示外部效果已回滚或不存在 |

`outcome_knowledge` 为 `NotStarted / InFlight / Known / Unknown`：NotStarted 表示尚未开始所跟踪调用；InFlight 表示有跟踪中的调用；Known 表示结果与所记录效果结论有证据，或已确认该流程不需要任何效果；Unknown 表示至少一项已发出或可能发出的效果无法确认。Unknown 优先于其余聚合值。只读任务也应记录调用和结果知识，但不把只读网络断线宣称为未知写入。`Failed/Cancelled + Unknown` 合法且必须突出显示；`Succeeded` 必须为 Known。

Task 终态后不重新开放同一个目标执行。后续对账可以追加证据并更新结果知识，但不能修改已发生历史；需要再试时创建显式关联的新 Task，执行前消除重复效果风险。旧名 `Success` 迁移为 `Succeeded`。

Run 生命周期为 `Created / Running / WaitingInput / WaitingApproval / WaitingResource / Paused / Cancelling / Succeeded / Failed / Cancelled / Lost`。`Lost` 表示当前执行确认已不可信，不能推断旧进程已经停止。有效租约与资源端 fencing 确认后才允许新 Run 领取相关写资源。任务内的自动重试在 Task 达终态之前创建新 Run；重启后继续同一已验证检查点是否沿用 Run 由 Runtime 明示，跨节点替换执行者必须新建 Run。

## 8.5 效果、重试、补偿及迟到结果

**V1-TSK-008（必须）**：工具至少声明 `ReadOnly / IdempotentWrite / CompensatableWrite / NonRetryableEffect`，并声明幂等保证的范围、有效期、结果查询方法和实际取消能力。可补偿不等于可直接重试，幂等写也不免除授权、版本前提和资源所有权检查。

副作用前持久化 Operation 意图和稳定操作 ID；校验 ExecutionPermit；调用后保存回执、外部对象版本和结果。请求 ID 相同不能证明服务端有业务幂等。无可靠查询或有效幂等保证的超时一律停止自动重发。补偿是新的、独立授权和审计的效果，可以失败；补偿成功只证明该补偿的业务条件满足，不抹除原操作历史。

**V1-TSK-009（必须）**：Run 结果提交包含 Task 修订、Run ID、领取代次、恢复世代、输入/执行版本摘要和结果引用。Broker 拒绝过期写入者改变当前 Task，但须将可信迟到回执作为独立对账证据保存；“拒绝旧结果”不能等于“忽略已经发生的外部效果”。失败或重复结果不能释放其他活跃 Run 的资源。

## 8.6 结果与 Artifact

**V1-TSK-010（必须）**：大结果先上传到隔离暂存位置，验证大小、内容摘要和安全域，再提交不可变 Artifact 引用。成功状态提交与引用生效之间须有恢复协议；不可将尚未完成上传的 URL 标为成功。结果包含版本、媒体类型、长度、校验值、生产 Task/Run/Operation、保留关系和访问条件。

结果状态使用 `Pending / NoneExpected / Available / Evicted / Unavailable`。`NoneExpected` 仅用于合同已声明不产生结果制品的任务，不是错误造成丢失；`Available`须能按当前权限解析所列制品；`Evicted`保留清理时间、原因和允许查询的摘要；`Unavailable`表示暂时无法取得且无已回收证明。历史用语`NotProduced`仅在可证实原合同无结果时迁移为NoneExpected。正文回收后任务元数据、防重放墓碑和必须保留的审批证据独立处理；可信历史Succeeded不因结果后来不可取而改写，依赖缺失结果的步骤必须阻塞。日志、截图、模型输出和结果资产各自设保留规则。跨安全域复用内容地址不能绕过所有权与解密边界。

## 8.7 Event outbox、inbox 与订阅

**V1-TSK-011（必须）**：事件信封包含 `event_id`、`event_type`、`schema_version`、来源权威、资源引用/修订、发生时间与接收时间、关联/因果 ID、必要 Task/Run ID、事件序号和授权上下文引用。时间戳不是排序权威；序号仅在声明的权威分区内可比较。正文、密钥、访问令牌不得进入面向无关订阅者的信封。

状态与 outbox 同事务提交；发布 ACK 丢失可重复发送相同 event_id。消费者先登记 inbox 意图，业务状态变更、处理结果和消费进度须在同一事务中提交或使用等效恢复协议。执行外部效果的消费者必须使用 Operation 意图/幂等及核实流程；简单的“标记已处理，然后执行”会丢工作，“执行，然后只记游标”会重复效果，二者均不合格。

**V1-TSK-012（必须）**：列表快照返回可连续订阅的权威游标；订阅中途失效返回 `CursorExpired`，要求重新列举并从新游标恢复。分页 token 绑定查询、主体/安全域、授权代次和快照范围；不能把一次扫描的漏项伪装为删除。权限过滤在服务端进行，通知仅携必要 ID，读取正文再次授权。

**V1-TSK-013（必须）**：定义消息大小、并发、积压字节、重试次数/总时限与退避。毒消息进入可审计隔离队列；重放须绑定原 event_id、处理器版本和已知效果记录。事件历史裁剪之前保留所需快照/检查点；不能为恢复吞吐直接越过必须处理的终态或审批事件。临时进度可按声明丢弃；关键终态、必要审计与权威变更不可使用同样策略。

## 8.8 预算、调度和停机

**V1-TSK-014（必须）**：父子任务共享可核算的预算树。准入先预留、运行中计量、完成后结算，失败回收只释放未消费预留；并行分支不能各自读取相同余额并全部消费。云账单有延迟时展示估计与已确认费用；已发出调用可能造成有界超支，因此限制在途调用上限和最大输出量，不能把软计费预测宣传成严格零超支。

**V1-TSK-015（必须）**：Schedule 定义包括稳定 ID、修订、IANA 时区、当地触发规则、DST 重复/缺失时刻处理、错过触发的 Skip/Coalesce/BoundedCatchUp、重叠策略、截止时间和有效授权。每次逻辑触发生成稳定 `trigger_id`，绑定 Schedule 修订和逻辑触发时刻；补跑不生成重复身份。排队满时按策略拒绝或保留有界待重试意图，不能无限缓冲。修改调度从明确生效边界开始，不偷偷重解释已产生触发。

**V1-TSK-016（必须）**：关机先停止新接受/领取、持久化在途状态、使执行者到无写入安全点或由资源端 fencing，再释放会触发替代调度的租约。宽限到期仍无法证明隔离时进入 RecoveryRequired，不自动双写。浏览器断线、订阅断开或聊天成员退出均不自动取消任务。

## 8.9 必须执行的验收

| 测试重点 | 注入与通过标准 |
|---|---|
| 提交原子性 | 在容量预留、Task、去重记录和 outbox 各边界杀进程；不能出现接受成功却没有可恢复 Task |
| ACK 丢失 | Broker 接受后丢回执；重传、查询与 Workflow 重启均链接同一 Task |
| 过期防重放 | 回收任务结果、过期去重窗口、恢复旧副本；旧键不得被当作新工作 |
| 取消竞争 | Submit 与 CancelOrigin 两种顺序交错；要么 Task 被取消，要么提交被墓碑拒绝 |
| 迟到结果 | 旧 Run 在新 Run 领取后返回外部成功证据；当前状态不被覆盖，效果事实进入对账 |
| 未知效果 | 发出写请求后断网并取消；查询保留 Unknown，不能显示全量回滚或自动重试 |
| 事件恢复 | 重复、乱序、毒消息、游标过期和消费者版本变化；不丢权威终态，不重复效果 |
| 有界资源 | 队列、结果、日志和审计分别满额；准确拒绝且保留本地诊断/救援资源 |
| 定时与预算 | DST 双时刻、长停机补跑、并行子任务争用最后预算；触发唯一、补跑有界 |

上述用例扩展原 `ACC-EVT-01～03`、`AGT-T01～05`、`XAT-39`、`WF-AT-04～06/08`；文档中存在用例不表示测试通过。跨节点仍须满足集群章节的资源端 fencing；灾难恢复遵循恢复章节的外部效果对账屏障。

## 8.10 Agent 自主事件触发与任务派发

本节将 SuperAgent 类系统的事件触发、Router 和 Worker 机制映射到原 Task/Event，不增加第二个 Ticket、Queue、Run 或 Operation 权威；文档仍为 v1.0.0，工程 `NotVerified / NotRun`。

- **发现与关联：** 事件/定时器验证来源、权限、事件 ID、资源修订和规则版本。Router 优先关联已有 Task、WorkItem、WorkflowRun、Conversation/Thread；同一 Issue/CI/Review 的重复、乱序事件不能无限产生新任务。Webhook/评论只作为线索，回源查询外部权威。
- **受控创建：** 只有在有效规则、Grant/Delegation、目的、预算及范围下，才通过 `Task.Submit` 的 `origin_ref`、稳定 `trigger_id`、`idempotency_key` 和不可变输入摘要提交新 Task。否则保持候选/等待审批，不能报 Accepted。队列和预算满时有界拒绝。
- **执行选择：** Router 只推荐能力与步骤；Broker/Scheduler 决定 Run、领取租约/代次、隔离 Profile、数据驻留及 Worker。Agent 不得自行声称领取成功或改写权威终态。
- **恢复：** 非 LLM Watchdog 独立检测任务、租约、取消和预算。Router 或模型离线不妨碍读取已持久记录。失去租约时先 fencing，跨节点/模型/工具不兼容时创建新 Run，并核对已执行 Operation；`OutcomeUnknown` 不盲重试。
- **验收（未执行）：** 重复 CI 事件只产生一个 Task；后续 Review 进入原 Task；Router 崩溃后无重派发；模型失败不拖垮本机管理；旧 Worker 被 fencing 后无法双写；外部成功但 ACK 遗失先对账。正式新增测试编号须同步测试注册表。
