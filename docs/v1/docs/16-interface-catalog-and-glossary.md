# 16｜接口目录、字段词典与合同适用范围

## 1. 接口形态

本包交付逻辑服务接口与JSON合同，不假造已部署的URL、CLI命令、OpenAPI服务器或数据库表。HTTP/RPC路由、认证握手、版本协商及传输错误映射由ADR-05冻结；无论选何种传输，正文的授权、幂等、状态、恢复和错误语义不变。

**V1-IF-001｜请求、权威记录和展示投影分开。** `schemas/`文件的title明确对象用途。TaskSubmit、ChatSend与CancelOrigin是客户端请求；Accepted是服务端回执；Task、Run、Message等是受授权读视图。ResourceEnvelope给跨域列表提供共同信封，不能直接拿其中开放的spec创建任意资源。领域合同可以是信封中的spec/status投影，但同一服务必须登记唯一映射，不能使`metadata.resource_id`与扁平`task_id/volume_id`指向不同资源。

`ResourceRef`固定为`resource_id + version + digest`。示例中的ref digest只是虚构夹具标识，只有正式登记并认证的资源才可解析。引用Schema通过不证明对象存在、签名有效、未被回收或当前主体有权读取。所有示例使用虚构身份，无密钥和可用授权令牌。

## 2. 逻辑服务目录

| 服务与主要接口 | 输入 | 权威结果与关键条件 | 主要章节 |
|---|---|---|---|
| Resource.Get/List/Watch | 主体、资源/过滤、快照或游标 | 固定快照页、来源代次、ResyncRequired；不承诺跨authority全序 | 02 |
| Resource.Plan/Apply | 计划、预期版本、幂等身份 | Operation；实际效果前提在资源端强制 | 02、04 |
| Approval.Request/Decide/Bind | 计划引用、审批策略、决定、Operation | 不可变Decision与CAS使用单元；不是直接执行工具 | 04 |
| Policy.Revoke/GetRevocation | 目标Grant/主体与范围 | 受理结果和逐端点Enforced、残留效果 | 04 |
| Task.Submit/LookupSubmission | 固定来源、输入、预算、委托、幂等键 | TaskAccepted或四分查询；服务端分配task_id | 08 |
| Task.Cancel/CancelOrigin/GetResult | Task或来源身份、独立取消ID | 取消进度、效果知识、结果可用性 | 08 |
| Task.Claim/Renew/CommitResult | Worker身份、Task修订、RunClaim | 有效Run与领取代次；不能替代资源fence | 08、12 |
| Conversation.Send/LookupSend/Pull | 认证sender、client_send_id、内容、游标 | 单写日志回执；成员裁决、命名空间和恢复世代 | 10 |
| Conversation.Join/Leave/Revise/Redact | 会话权限、预期版本、作用范围 | 新日志事实和成员版本；不改写原始发送身份 | 10 |
| Workflow.Register/Start/Cancel | 版本AST、输入、根预算、策略 | Definition版本、WorkflowRun、DispatchIntent | 11 |
| Project.Accept/LinkExternal | 验收条件、权威来源、外部对象版本 | WorkItem验收、ExternalLink；外部事实仍回源核实 | 11 |
| Storage.Create/Attach/Detach | 受控卷、节点、访问模式、身份映射 | Volume/Attachment、真实访问与fencing结果 | 06 |
| Backup.Capture/Restore/Export | 资源清单、一致性、密钥引用、恢复计划 | 认证清单、数据验证和独立激活状态 | 05、06 |
| Dataset.Put/Query/View/Index | 版本记录、受限查询、授权、水位 | 精确/全文/近似语义、当前权限过滤、来源版本 | 06 |
| Package.Recognize/Import/Build | 隔离输入、来源证明、ABI/Profile | Imported制品与依赖快照；RuntimeVerified另需实测 | 07 |
| GUI.Create/Stream/AcquireInput/Stop | 会话、Portal批准、Viewer/Agent权限 | DisplayStream、ControlLease、范围和撤权证据 | 07 |
| Model.Resolve/Invoke | 模型Profile、外发政策、预算、输入引用 | 本次执行快照、用量、受控输出；模型不能批准自己 | 09 |
| Node.Enroll/Drain/Fence | 节点身份、资源委托、恢复世代 | 逐资源写权和状态；不可把心跳超时当已停机 | 12 |
| Update.Stage/Trial/Commit/Rollback | 确切部署、兼容与恢复材料 | 持久更新步骤、健康与数据兼容 | 03 |

表中名称是逻辑操作。未提供对应JSON Schema的接口仍需按关联章节形成独立、可审查的消息合同后才能开发客户端；不得把该表当作完整OpenAPI。已提供Schema的接口也还需真实服务语义验证。

## 3. 关键字段与比较规则

| 字段或字段组 | 权威/作用域 | 允许的解释 |
|---|---|---|
| resource_id / task_id / run_id / operation_id | 各自对象权威 | 不可复用opaque identity；相同显示名不能替代 |
| identity_domain_id | 原身份签发域 | 不随迁移改变；不等同当前服务器地址 |
| authority_id | 当前管理或写入权威 | 可经显式转交变化；不是全局安全域 |
| trust_domain_id | 信任根/签发验证域 | 说明凭据由谁认证 |
| security_domain_id | 租户/项目授权边界 | 服务端验证；不能由调用者任意自报可信值 |
| resource_version / record_version | 同一对象同一权威 | opaque等值前提；禁止字典序判断新旧 |
| spec_generation / observed_generation | 同资源和恢复世代 | 十进制计数；观察代数不得超前；status更新不随意改spec |
| authorization_epoch | 指定授权权威/安全域 | 撤销新鲜度；不拿另一authority的较大数字代替 |
| security_recovery_epoch | 指定信任/安全域 | 已建立且执行端认可的信任恢复代次 |
| recovery_incarnation | 控制面/服务恢复域 | opaque世代，仅按绑定相等比较，不按UUID大小排序 |
| data_incarnation | 卷/数据恢复身份域 | 原目标恢复废止旧句柄；与控制面世代分开 |
| writer_epoch | 某Conversation或其他已声明Writer权威 | 资源端拒绝旧写者；非sender自报版本 |
| membership_revision | Conversation成员裁决版本 | 该消息提交依据；不等于当前允许读取 |
| conversation_seq / source_seq | 会话日志/来源事件日志及其incarnation | 十进制序号，允许缺口；跨域不可比较 |
| step_key / attempt_generation | Workflow步骤实例/业务派发尝试 | 网络重发不增加；Loop/Map实例与业务重试分开 |
| idempotency_key / client_send_id | 认证主体、域、authority、动作及有效命名空间 | 同键同摘要复用；过期不当新请求；不能证明任意外部API恰好一次 |
| digest | 明确算法/Profile的内容摘要 | 完整性比较；不自证来源、签名或当前授权 |
| deadline_at / not_after | 可信UTC绝对时间 | 到期判定需偏差策略；恢复不重置旧有效期 |

**V1-IF-002｜计数字符串不能偷偷转number。** JSON Schema pattern只验证语法，u64上限、范围内比较和跨字段关系仍需语义校验。UI可格式化显示，但请求与存储保持原字符串或无损整数。示例故意使用`9007199254740993`检验JavaScript安全整数边界。

## 4. 生命周期索引

**V1-IF-003｜同名状态始终限定所属对象。** Task.Cancelled、Operation.Cancelled、备份Cancelled与部署停止不是一件事；某一步完成也不能把全局恢复标为完成。生命周期、健康、可访问性、效果知识和补偿分别报告。

| 对象 | 生命周期/阶段字段 | 独立维度 |
|---|---|---|
| Operation | Pending、WaitingApproval、WaitingDependency、Running、CancelRequested、Succeeded、Failed、Cancelled | outcome_knowledge、compensation_status、effect_refs |
| Task | Accepted、Queued、Running、WaitingInput、WaitingApproval、WaitingResource、Paused、Cancelling、Succeeded、Failed、Cancelled | outcome_knowledge、result.availability |
| Run | Created、Running、WaitingInput、WaitingApproval、WaitingResource、Paused、Cancelling、Succeeded、Failed、Cancelled、Lost | outcome_knowledge、RunClaim有效性；Lost不证明进程已停 |
| WorkflowRun | Draft、Ready、Running、WaitingApproval、WaitingInput、Blocked、Cancelling、Compensating、Succeeded、Failed、Cancelled | outcome_knowledge、compensation_status、步骤引用 |
| DispatchIntent | Pending、Submitted、Linked、CancelPending、Cancelled、Blocked | accepted回执、取消墓碑、原submit_request |
| Volume | Provisioning、Available、Deleting、Deleted | health、Attachment、access_mode |
| VolumeAttachment | Attaching、Attached、Detaching、Detached | requested_access、effective_access、fence、挂载Operation |
| RecoveryCoordinator | 05章的restore_phase | data_validation、security_mode、效果对账、activation_scope |
| 存储恢复子操作 | Preflight、Restoring、Validating、AwaitingActivation、Activated、Failed | storage_restore_phase；不能覆盖全局恢复 |

DispatchIntent.Submitted仅表示已尝试发出，不能推断Broker已接受；Linked必须有匹配原请求的TaskAccepted；CancelPending表示取消对账尚未确认；Cancelled必须有Broker取消/拒绝墓碑或可证明尚未发出，不能只因删除本地outbox进入此状态；Blocked保留原因，未知是否接受时不生成新键。

Task、Run、Operation和WorkflowRun的`Succeeded`均要求`outcome_knowledge=Known`。历史成功制品后来回收时保留执行事实，另报Evicted；后续依赖缺失制品不能继续。

## 5. Schema覆盖与运行时语义

目前33份Schema覆盖共同类型、资源信封、错误、计划/效果、审批/许可、Task、Chat、Workflow、Event、Dataset、卷、能力/撤权、恢复、应用清单及完整ReleaseSpec。`schemas/catalog.json`为确切列表；`examples/manifest.json`列出每个正反样例及预期拒绝原因。字段中的数组/文本结构上限是本版合同的防御性上限，运行Profile可以更严格，不能无限放大。

ApplicationManifest必须通过release_spec_ref固定完整ApplicationReleaseSpec；后者包含执行、生命周期、身份、权限请求、资源硬限、许可/来源、依赖及备份范围。解析所有必要引用、验证对应类型与当前权限后才可形成Deployment，不能把发布元数据投影当作完整运行配置。

**V1-IF-004｜结构合同不放行不可信数据。** Workflow AST还需编译/类型/权限验证，RecoverySetManifest还需认证来源与真实对象验证，ExecutionPermit还需可信在线查询或签名链验证，ApplicationManifest还需隔离构建与真实兼容测试。示例`RuntimeVerified`所需的evidence引用也只有实际证据可查时才有效。

JSON读取必须在进入常见字典解析前拒绝重复键；默认“保留最后一个值”会丢掉攻击证据。Schema不知道输入曾经有重复键，必须由入口解析器先处理。任意JSON数据还需限制深度、长度和数值精度Profile；不支持的精度明确拒绝，不能先损坏数值再假装精确查询。

## 6. 术语补充

| 术语 | 定义 |
|---|---|
| ACK / Accepted | 服务端在声明故障范围内持久接受，不代表接收人已读或副作用成功 |
| outbox / inbox | 与权威状态协调的待发送/消费记录，用于可恢复投递，不自动构成跨系统exactly-once |
| fencing | 在实际资源写入口拒绝旧持有者，或经验证隔离旧执行者；租约过期本身不足 |
| pin / retention | 在使用期间保护内容版本不被GC的可验证保留关系，不只是缓存计数 |
| saga / compensation | 多个独立效果的协调与补偿；补偿不是全局事务回滚，可能失败或产生新效果 |
| Profile | 明确版本、环境、能力、数值上限、限制与证据的一组可部署配置 |
| RPO | 声明故障与恢复集下允许/实测的数据恢复点损失，不由“有备份”推导为零 |
| RTO | 指定起止边界的恢复时间，明确是否包含发现、取密钥、对账和重新开放 |
| ABI | 二进制链接/调用兼容约定，独立于网络API与数据Schema |
| OCI | 制品/运行标准族；采用OCI不自动提供所需的宿主/租户隔离 |
| Portal / EIS | 桌面授权/输入机制；支持范围、版本、用户会话与真实撤权行为必须实测 |
| NativeWeb | 应用已有的Web入口，经安全origin/会话边界接入；不等于GUI远程像素流 |
| TableFS | 可选统一文件命名空间与Catalog后端；派生索引不成为文件存在性的第二权威 |
| Knowledge | Dataset授权视图与必要索引，不是无来源的另一份原文数据库 |

外部协议全名、MCP新旧代际、A2A和两种ACP消歧见09章；不要只用缩写认定兼容。

## 7. Shell 视图对象

| 术语 | 边界 |
|---|---|
| PsiOS Shell | 浏览器桌面工作环境；通过统一 API 访问能力，窗口层不承担 Linux compositor 职责 |
| ShellWindow | 浏览器中的 UI 窗口、对象引用及布局状态；不等同于远端 ApplicationWindow |
| Shell 工作区 | 组织窗口与项目入口的视图；不创建权限域或任务执行域 |
| 应用呈现描述 | Application 关联的图标、入口、打开类型、实例策略和菜单等声明 |
| 工作区布局 | 按主体、安全域、连接目标与客户端保存的偏好；不能包含令牌或旧 ControlLease |

字段与接入边界见第 07 章 V1-SHL-008。它们是待冻结的视图合同，不因列入词典而成为已实现接口。
