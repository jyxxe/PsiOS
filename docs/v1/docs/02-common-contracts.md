# 02｜身份、资源、接口与数据编码共同契约

本章承接旧 API-001～020，给出所有领域必须共用的定义。`psios.v1` 是本设计中的逻辑 API 名称；`schema_version: "1.0.0"` 标识本包消息结构。两者均不证明存在已经部署的服务。

## 1. 名词和单一事实来源

| 名词 | 精确定义 | 不等同于 |
|---|---|---|
| Principal | 可认证、可撤销的用户/Agent/服务/节点/外部连接主体 | 显示名、Linux UID、请求自报sender |
| CapabilityDescriptor | 后端可提供的动作、范围和强制能力说明 | 授权令牌或许可 |
| Grant | 授予主体在条件下访问资源/动作的政策记录 | 单次审批、Secret |
| Delegation | Grant范围内向另一主体传递的有界委托 | 任意扩权或自动继承发信人权限 |
| ApprovalRequest | 请求某审批者作决定的等待资源 | 已批准 |
| ApprovalDecision | 对不可变计划与条件的不可变批准/拒绝事实 | 执行成功或无限次通行证 |
| ApprovalUse | 将Decision可用额度以CAS绑定Operation的账本 | 删除后可重新消费的缓存 |
| OperationPlan | 目标、允许效果、输入、依赖与审批范围的不可变计划 | 模型内部推理文本 |
| Operation | 某一次系统变更/效果尝试的权威记录 | 任意Task整体或聊天消息 |
| EffectRecord | 一个可能或已经发生的外部/资源效果及核实证据 | 简单布尔success |
| Task | 一个可追踪的执行目标及输入、预算、取消域 | WorkItem或单次进程 |
| Run | Task的一次执行尝试 | 一个资源的全局fencing计数 |
| TaskStep / ToolCall | Run内部观察步骤/具体能力调用 | WorkflowStep实例 |
| WorkflowStepInstance | 定义版本中的步骤及动态迭代实例 | TaskStep或Task结果副本 |
| Artifact | 持久结果及其版本、摘要、访问和保留关系 | 临时下载URL |
| Asset | 用户或服务管理的数据资产通称，可引用Artifact/Volume/Record | 另一份重复原文 |
| ProjectArtifact | 项目到Artifact的关联视图 | 另一份Artifact权威 |
| Session | 单Agent的上下文与交互状态 | Conversation或系统登录会话 |
| Conversation / Room | 多主体交流权威；Room为群组形态 | 群成员自动拥有操作权 |
| ExternalLink | 指向外部对象及字段事实的映射记录 | 允许无条件双向覆盖 |
| SecretRef | 在特定主体和用途下解析秘密的受控引用 | 可解引用的明文密钥 |

`Pool → StoragePool`、`WorkflowInstance → WorkflowRun`、`Plan → OperationPlan` 是旧名称迁移。`Approval` 若出现在旧接口，映射到 Decision及使用账本的只读兼容视图，不创建第二授权权威。`Job` 是有调度定义的任务来源，最终仍提交统一 Task。

## 2. Resource与引用

**V1-API-001｜身份、位置和权限分离。** 每个资源分配不可复用的 `resource_id`。其签发域 `identity_domain_id` 和不可变ID共同证明身份；当前管理者 `authority_id`、运行节点及URL可变。实现可采用全局唯一 opaque ID，也可使用带签发域的编码；调用者不得拆解字符串来推导权限。资源删除、复制、克隆、恢复后的身份规则见恢复章。示例短ID仅为虚构夹具。

| Resource字段 | 类型/来源 | 约束 |
|---|---|---|
| api_version / schema_version / kind | string | 各自用途独立；未知安全版本拒绝 |
| metadata.resource_id | string；权威分配 | 永不回收给另一个实体 |
| identity_domain_id | string；不可变 | 与认证信任域验证关联 |
| authority_id | string；当前权威 | 变更通过有记录的权威转交 |
| security_domain_id / owner_principal_id | string | 从可信身份与策略赋值；跨域转移单独授权 |
| resource_version | opaque string | 仅相等/不相等比较，不解析大小、时间或十六进制 |
| spec_generation | 十进制计数字符串 | 仅在同一资源同一恢复世代内递增 |
| created_at / updated_at | UTC RFC3339 string | 显示/审计时间，不充当因果排序 |
| spec | 对应kind的版本化结构 | 不能以自由JSON绕过领域Schema |
| status.observed_generation | 十进制计数字符串 | 表明控制器实际观察到哪个spec；不能比当前spec更新 |
| status.lifecycle / conditions | 枚举/条件列表 | 生命周期、健康、同步状态分开 |

**V1-API-002｜引用必须固定可验证版本。** 输入、审批、运行检查点、制品和备份清单使用 `{resource_id, version, digest}` 的不可变引用；可变名称/路径/URL只用于显示或定位。引用解析必须确认来源权威、主体访问和保留关系。URL过期不意味着资源删除，资源存在也不意味着有权访问。`version` 是该对象的版本标识，不是假定全局可比较的计数。

关系明确分为 `owns`（生命周期拥有）、`uses`（租用/读取）、`depends_on`（可用性依赖）、`derived_from`（可重建派生）。删除前列出阻塞引用；持久任务输入、打开句柄、快照、备份、导出pin和在建索引分别持有保留引用。强制解除阻塞必须记录未清理项和可能损失，不能把“终止等待”写成数据清理成功。卸载应用默认保留独立卷。

## 3. 并发、条件写和状态投影

**V1-API-003｜条件写由资源权威原子检查。** 更新/删除需 `expected_resource_version`；检查与状态写入同事务或同一可证明串行化点执行。冲突返回 `Conflict`，客户端读取新版本后重新计划，禁止最后写入覆盖。status心跳可以改变resource_version，但不会自动改变效果相关的 `spec_generation/content_digest/dependency_digest`；审批绑定后者，避免无关心跳使批准不断失效。

**V1-API-004｜依赖条件须可强制。** 授权层检查通过只证明准入时条件成立。副作用层必须原子验证效果前提（如commit SHA、文件版本、设备稳定身份）并执行；否则存在检查后变化窗口。后端不支持conditional apply时，必须通过排他控制或受限人工流程提供等效保证，无法提供则返回 `UnsupportedCapability`/`RecoveryRequired`，不能声称资源版本检查已经防住竞态。

**V1-API-005｜声明式控制权显式移交。** 管理资源只能有一个当前spec权威。人工接管使用 `control_mode=Managed|Manual`、原因、期限和当前revision；控制器在Manual下停止冲突写入，到期需按政策重新对账，不能盲目覆盖手工修复。节点入群只转交明确资源，不接管用户主目录或桌面。未知进程和挂载先发现、标记Observed，不能直接认领为受管资源。

## 4. 编码、摘要和兼容

**V1-API-006｜跨语言编码固定。** 对外JSON使用UTF-8；字段名snake_case；计数/序号/epoch/字节数的无符号大整数使用规范十进制字符串（无正号、无前导零，零为`"0"`），不得经JavaScript number中转。上限不超过u64时仍在语义校验中检查`<=18446744073709551615`。普通浮点字段只接受有限值；需要精确金额使用币种+整数最小单位/decimal字符串。时间为UTC RFC3339，展示可转用户时区；对象排序只使用声明的authority scope与序号。

**V1-API-007｜规范化摘要有域隔离。** 本章指定的JSON协议对象摘要使用RFC 8785 JCS、SHA-256及明确的profile标签。计算输入为`UTF8("psios/<contract>/1\n") || JCS(payload)`，结果写为`sha256:<64小写hex>`。payload只排除该合同明确的摘要字段自身；传输request_id、Bearer凭据和独立回执本来就不在被摘要业务对象内。已经属于对象的plan_id、definition_id、清单创建者/时间等字段全部参与，不能按“服务端生成”再自行排除。具体合同明确纳入的业务字段及委托/预算引用。重复键、非法Unicode、NaN/Infinity、无法按合同表示的数值在计算前拒绝；数组顺序保留；不自动Unicode归一化。摘要是内容一致性证明，不能代替身份认证或数字签名。[RFC 8785](https://www.rfc-editor.org/rfc/rfc8785.html)

Task提交payload是其请求去掉`request_digest`；Chat摘要payload是发送请求去掉`content_digest`，因此幂等ID及附件/mentions等全参与，不能只hash正文。OperationPlan摘要payload是完整plan去掉`plan_digest`；Artifact正文摘要按原始内容字节计算，**不**套用JSON JCS。存在流式大文件时digest涵盖完整内容，不能只hash分页片段。附件引用digest应与上传校验后的权威对象一致。

本包合同名固定为 `task-submit`、`chat-send`、`operation-plan`、`workflow-definition` 和 `recovery-set-manifest`；后两者payload分别排除`definition_digest`和`manifest_digest`。`schema_version=1.0.0`直接绑定此摘要Profile，其算法为SHA-256、规范化版本为RFC8785；OperationPlan与RecoverySetManifest另携带显式digest_profile供单独审阅。JCS的数值范围不自动收缩Dataset的通用JSON能力：含任意精度数值的原始数据作为版本化内容制品按字节摘要，协议计划引用该制品，不先转double再规范化。若另定义Dataset语义摘要，必须新建明确的canonicalization Profile与跨后端测试，不能把原字节digest冒充语义等价判断。

**V1-API-008｜版本彼此独立。** OS镜像版本、Core API、消息Schema、持久数据Schema、插件协议、工作流DSL、Skill/工具/模型快照、ABI、Portal版本分别记录。实验协议只能标Experimental，版本接近不表示兼容。请求未知动作、未知安全字段、未知枚举必须拒绝；可忽略的展示扩展只允许在有命名空间的`extensions`中并有大小/权限界限。Schema采用`additionalProperties: false`的关键请求不能以“前向兼容”为由偷偷忽略授权字段。

## 5. Operation和效果状态

**V1-API-009｜接受、执行、结果和补偿独立。** 通用Operation的lifecycle为`Pending|WaitingApproval|WaitingDependency|Running|CancelRequested|Succeeded|Failed|Cancelled`。`outcome_knowledge=NotStarted|InFlight|Known|Unknown`独立表示效果认知；`compensation_status=NotRequired|Planned|Running|Complete|Partial|Failed`独立表示补偿。保留`effect_refs[]`指向独立EffectRecord及其核实证据。

| 场景 | 生命周期/效果语义 | 处理 |
|---|---|---|
| 入库但未执行 | Pending / NotStarted | 返回operation_id可查询，重新检查授权 |
| 请求已发出 | Running / InFlight | 记录effect intent和对端标识 |
| 本地超时但对端可能成功 | Failed或等待核实 / Unknown | 不以普通重试规则再发副作用 |
| 取消后有在途动作 | Cancelled / Unknown或Known | 取消只阻止后续步骤；保留已有effects |
| 全部要求成功且结果已持久 | Succeeded / Known | 可以解除依赖 |
| 补偿只恢复部分 | Failed或Cancelled / Known，compensation=Partial | 展示未恢复效果，不能写完整回滚 |

旧`OutcomeUnknown`状态迁移为条件`OutcomeUnknown=true`加outcome_knowledge=Unknown。原始记录不能确定生命周期时不得猜测Succeeded，进入对账Blocked。`RecoveryRequired`是需恢复的条件；`PartiallyApplied`是配置聚合条件；不能任意加到每个域的终态枚举。

执行生命周期终态不被迟到消息重新打开；对账可追加核实结果、修正outcome_knowledge和补偿信息，但保留历史及审计。若需要再执行，建立有显式关联的新执行请求；不能复用旧operation_id代表不同效果。

## 6. 幂等、配额和取消

**V1-API-010｜幂等命名空间包含安全域。** 对写请求去重键至少包含认证security_domain、实际调用主体/委托作用域、逻辑authority、动作、idempotency_key。Workflow来源另包含稳定origin_ref；Chat以Conversation+sender+client_send_id去重。相同键同摘要返回原记录，异摘要拒绝`IdempotencyConflict`。先认证并校验当前读取/查询权限再返回旧结果，幂等命中不能泄露其他主体或已撤销数据。

**V1-API-011｜保留期结束不可证明未执行。** `Found`、`AbsentWithinWindow`、`Expired`、`Indeterminate`分别表示已知记录、权威能证明窗口内未接受、历史已过可接受窗口、当前无法证明。旧键过期必须明确拒绝或保留拒绝墓碑；不能换键偷偷重试。存储压缩时保留窗口边界/关闭的提交epoch等可验证记录，不能无限保存正文，也不能清空去重后恢复“第一次请求”语义。

**V1-API-012｜配额准入与必要状态一起提交。** 入队数、输入/结果引用、审计、终态、去重与恢复空间均须有界。接受任务前预留必要持久元数据容量；资源不足拒绝并返回具体配额。模型费用估算、预留、在途承诺和实际账单分开，取消不自动返还已经发生的费用。子任务预算总和不得超过父可用额，除非获得新的显式授权。

**V1-API-013｜取消是独立可查询操作。** 请求取消与目标完全停止不同；先持久取消意图，再阻止新步骤，再通知执行者并执行可用fencing/终止机制，最后记录实际结果。断开前端连接不等于取消。跨服务取消必须覆盖“取消先到、Task稍后被接受”的竞态，见Task章；延迟操作不能靠没有task_id而漏掉取消。

## 7. 列表、分页、Watch与投影

**V1-API-014｜快照到事件无缺口。** 列表返回固定读取快照标识、分页游标及同一authority的watch起点；所有分页继续同一快照。服务端保留起点之后事件或提供等效屏障，客户端完整列举后接续watch。快照或游标过期返回`CursorExpired/ResyncRequired`，不得以空列表表示没有变化。重新同步必须处理已删除对象，不能只upsert导致旧资源永久残留。

过滤、排序、安全域、主体及授权代次绑定游标；游标不能被改造为越权查询。多authority汇总返回每个来源的游标/观测时间/不可达状态，不构造虚假的全局revision。Watcher收到旧世代的事件不能推进新世代进度；恢复后失效旧游标并重新列举。[etcd恢复中revision与watch缓存风险](https://etcd.io/docs/v3.6/op-guide/recovery/)

**V1-API-015｜通知不是授权凭证。** Event含ID、schema、来源域、资源版本、因果/关联ID、task/run引用及发生/接收时间；敏感正文/Secret/长效token不入通用广播。订阅过滤由服务端执行，读取正文再次授权。授权被撤销后已发出的副本无法保证收回，必须声明数据流向；持续流按撤权能力处理。

## 8. 错误约定

错误结构：`code, message, request_id, retryable, retry_condition, operation_id?, details?`。message是可本地化说明；自动客户端依据code和retry_condition。retryable只表示在明确条件下可再次提交同一操作，不证明对外副作用可重做。

| 错误 | 语义 | 客户端动作 |
|---|---|---|
| InvalidArgument / SchemaVersionUnsupported | 结构或版本不支持 | 修正请求/协商；不自动降安全版本 |
| AuthenticationRequired / PermissionDenied | 身份不足/无权 | 获有效身份或授权；不换入口绕过 |
| ApprovalRequired / ApprovalInvalidated | 缺批准/计划条件改变 | 形成可审查的新计划或重新授权 |
| ApprovalAlreadyBound | 单次使用单元已绑定另一Operation | 查询已有绑定；不得删除账本重用 |
| NotFound | 可授权的权威对象不存在 | 与过期历史/不确定查询分开 |
| Conflict / PreconditionFailed | 资源条件变化 | 重读、重算计划 |
| IdempotencyConflict / IdempotencyKeyExpired | 同键异内容/窗口已关闭 | 修复或核实，不自动换键 |
| QueueFull / QuotaExceeded | 准入容量拒绝 | 按预算和退避等待；不无限缓冲 |
| ServiceNotInstalled / UnsupportedCapability | 服务未装/后端未实现能力 | 提供准确能力说明 |
| DependencyUnavailable / StorageUnavailable | 当前运行依赖失败 | 诊断恢复；不创建空数据替身 |
| CursorExpired / ResyncRequired | 无法接续增量 | 完整快照重同步 |
| ResultEvicted | 执行记录存在，输出已回收 | 显示保留政策和清理证据 |
| ResultUnavailable | 输出暂不可取得且无已回收证明 | 保留执行事实，恢复/核实制品，阻塞依赖 |
| LeaseLost / Fenced | 当前领取失效/资源端拒绝旧写者 | 停止相关写入，保留可能已发生效果 |
| Cancelled / DeadlineExceeded | 请求流程已取消/超过期限 | 查询Operation，不由错误码推断没有效果 |
| OutcomeUnknown / RecoveryRequired | 效果未确认/需恢复 | 停自动副作用、核实或接管 |
| PartialFailure | 部分资源未达要求 | 返回逐资源结果，禁止聚合为全部成功 |
| InternalError | 无法归入已公开类别的内部失败 | 脱敏诊断；是否可重试按效果账本判定 |

## 9. Schema验证能力边界

**V1-API-016｜结构有效不等于请求可执行。** 本包Schema验证字段、类型、基本枚举和可表达条件；签名/Principal身份、引用存在/保留、digest正确、跨记录CAS、余额预留、政策新鲜度、运行版本兼容、时间有效和真实fencing必须由服务及契约测试验证。SDK必须复用同一规则，不得暗中替用户加权限。

**V1-API-017｜路径不是权限。** Unix Socket验证实际peer credentials及服务身份；HTTP/远端传输验证身份、目的、TLS与来源策略；D-Bus/容器通道按用户命名空间映射；请求body中sender、节点ID、owner和role不具有自证能力。连接建立时通过一次认证，也不能无限授权其后每个请求。

**V1-API-018｜边界有明确上限。** max请求字节、嵌套深度、数组长度、并发、流速、操作deadline、队列和墓碑保留都由版本化运行Profile给出。Schema中的字符串长度是传输结构保护，不是资源配额或安全预算的替代；未知上限必须阻止生产Profile激活。缺省不允许表示无限。
