# v0.1–v0.6 → v1 设计迁移说明

## 1. 当前权威与保留关系

v1 以 `docs/` 的统一语义、`schemas/` 的结构约束和各登记表共同构成当前设计基线。正文与 Schema 冲突时必须修订，不能由实现者自行选择宽松版本。归档文件保存历史事实和讨论上下文，**原文保持原样，不直接覆盖 v1 修正**。本包不宣告某个操作系统基底、数据库、OCI 后端或性能数字已经批准或实测。

| 历史阶段 | 主要增量 | v1 的保留位置 |
|---|---|---|
| v0.1 | Linux Agent-Native OS；Helium/Neon/Argon；KDE；原生/第三方 Agent；可选数据服务；集群方向 | 01 产品架构及全部领域章；产品固定约束继续有效 |
| v0.2 | 统一 API、安全、Host、更新、应用、网络设备、存储、集群、运维和验收补全 | 02–09、12–13；旧编号全部在迁移登记保留 |
| v0.3 | 可选 TableFS、文件/表视图、Dataset 映射、POSIX 与恢复验证 | 06；新增真实持久边界、并发 GC、打开句柄撤权和 Go/No-Go |
| v0.4 | 多发行版包导入、隔离用户空间、FullDesktop/SingleApplication/NativeWeb | 07；细化供应链、真实输入隔离、Portal 版本与浏览器来源边界 |
| v0.5 | Agent 群聊/DM、自主沟通、PsiFlow、Project/WorkItem、外部 Repo/CI | 08–11；保留所有 Chat/Workflow 场景与权威分工 |
| Project 导入 v0.6 | v0.5 全文前附风险修订和实施路线，方便原生 Project 使用 | 所有新增风险并入相应领域；导入版原文仍归档 |
| Space 合并 v0.6 | 全量详细规范、跨模块接口/状态增量、WF-AT、ADR-23～25、实施队列与风险 | 02、04–05、08–13、ADR/路线/验收登记及本迁移目录 |

来源：[Project v0.6 导入版](../sources/archive/project-v06-unified-import.md)、[Space v0.6 正文](../sources/archive/space-unified-v0.6.md)、[Space 风险台账](../sources/archive/space-risk-v0.6.md)。每个源的 SHA、文件/页面身份见[来源清单](../sources/source-inventory.json)。

## 2. 导入版与 Space 的实际差异

Project 导入版开头已经明确说明它不是 Space 的逐字导出。本次逐字比较确认，其从字符偏移 7,033 开始包含上传 v0.5 的完整 100,498 字符正文；这个结果只说明内嵌关系，不说明它包含 Space 的全部新章节。

| 差异位置 | Space 中的内容 | 不能只依赖导入版的原因 |
|---|---|---|
| 第 35 章 / 35.9 | 跨模块接口和事件矩阵 | 导入版有总体架构和风险摘要，但没有该矩阵的同一完整章节 |
| 35.10–35.12 | 安全边界、状态权威/一致性、依赖建设顺序 | 需合并到共同契约、Security、Recovery、Task/Chat/Workflow，不能当重复摘要跳过 |
| 35.13 | ADR-21～25 的决策范围和证据 | 旧 v0.5 ADR-22 的 Project/外部同步部分与新增 ADR-23 需要拆分责任 |
| 35.14 | M0–M6 退出条件与 WF 子路线 | 不以标题相同推断退出标准逐字一致 |
| 35.15 | WF-AT-01～12 的具体场景和通过标准 | 这 12 个测试只在取得的 Space 正文中有完整定义，已全部纳入本包 |
| 35.16–35.17 | 带编号工作项及后续维护规则 | P0-04/P0-05 与 Project 路线含义不同；必须保留来源命名空间 |
| 第 36–37 章 | 风险增量、外部资料新鲜度及来源关系 | 导入版有部分等价摘要，不具有相同章结构；v1 合并具体约束后统一权威 |

“缺章”指缺少这些 Space 独立章节和部分独有细节，不表示导入版完全没有相关主题。Space 页面的旧编号从 35.9 起始是合并历史；v1 重排主题而保留溯源，不把编号缺口解释成材料已经取得。合并前 Space Page 的链接保留在原文中，本次没有取得其独立正文，不声称已直接比较该页面的每句话。

## 3. 对象、字段与状态转换规则

下表是设计迁移约束，不是已经执行的数据升级脚本。实际迁移需绑定旧 Schema、当前制品、记录清单和恢复集。

| 旧名称或歧义 | v1 明确定义 | 迁移时必须检查 |
|---|---|---|
| 资源 `uid`、路径、显示名混用 | 系统资源采用不可复用 `resource_id` 与身份签发域；Dataset 行的 `uid` 只在 dataset 内解释 | 不按名称合并身份；删除重建不能继承旧 Grant；跨域映射必须显式 |
| `Pool` | 正式资源名 `StoragePool` | 只做经过 Schema 识别的别名迁移，不生成第二种存储资源 |
| `resourceVersion` 等混合写法 | 外部字段统一 snake_case；`resource_version` 是不透明条件写前提 | 不能用字典序或整数加一猜测版本；跨域版本不能互比较 |
| `WorkflowInstance` | 历史迁移别名，对应 `WorkflowRun` | 不与 Task 的 `Run` 合并；不建立第二份可写 Workflow 状态 |
| `Attempt`、`Run`、工作流步骤重试混淆 | Task 执行尝试是 Run；步骤实例与受控的业务重试代次另有语义 | ACK 丢失/传输重试不自动生成新的业务尝试，也不扩大预算 |
| `OutcomeUnknown` 混入生命周期终态 | `outcome_knowledge=Unknown` 与 lifecycle 正交 | 保留未知效果及核实证据；不能自动转成 Failed→Retry 或 Succeeded |
| `RecoveryRequired`、`Blocked` 随意变成通用终态 | 各域具有明确状态机、条件与原因 | 按领域校验；不以相同词名跨域强制互换 |
| Workflow `Completed` | 只有已证明成功时映射为 Succeeded | 执行结束可能失败、取消或含未知效果；不能批量当成功 |
| 一个可变 `Approval` 对象 | `ApprovalRequest` 等待、不可变 `ApprovalDecision`、CAS 使用账本、短期 `ExecutionPermit` 分开 | 必须绑定 operation、计划摘要、允许效果、期限和安全条件；旧“同意”文本不生成执行许可 |
| `RevocationMode` 单枚举 | `revocation_capabilities` 集合；受理与实际强制结果逐目标报告 | `ExternalNonRevocable` 属已发生效果的限制，不能与撤权机制混在互斥枚举；Accepted 不是 Enforced |
| `sender_epoch`、WriterEpoch 与恢复世代 | 会话存储写者 `writer_epoch`、主体凭据状态与 `recovery_incarnation` 分别建模 | 旧字段含义需按来源核对，不做仅改名的无损迁移承诺；epoch 只在同资源同恢复世代可比较 |
| Chat 一般幂等键 | 客户端 `client_send_id` 与安全域、Conversation、认证发送者构成发送去重域 | 不能让不同成员或安全域相互命中；旧窗口结束不能当新消息自动接受 |
| 默认 RWO 解释成单进程写者 | ReadWriteOnce 描述附件/访问能力，单 Writer 还需写者权威与资源端 fence | 同节点可有多个进程；恢复后旧写者不能因控制面显示一个副本而视为已停止 |
| `score` 必须先有实数、vector 无模型版本 | 允许未评分的 null；评分/向量绑定记录内容与评分器/Embedding profile | 不为满足类型而制造 0 分；旧向量无 provenance 时标待重建/不可比较，不冒充新模型结果 |
| `extra_data` 被评分器收窄 | 任意合法 JSON 根值；missing 与 null 不同 | object/array/string/有限 number/bool/null 都要保留；对非 object 不能强写命名空间字段并丢原值 |
| 大书/仓库强拆为多条业务记录 | 仍是一条逻辑 record；正文可用不可变引用/分块，派生片段独立标来源 | `uid/name?/content/tag/score/vector?/extra_data?` 主模型保留；不能把分块 ID 当新权威原文 |
| 知识库复制独立原文 | Dataset 授权子集/视图与派生索引 | 索引重建、删除/撤权水位和访问时权限必须保留；不能用旧缓存继续在线泄露 |
| 单窗口截图等于单应用控制隔离 | 捕获范围、输入目标、剪贴板及文件/音视频权限分别证明 | 后端输入能越窗就不能宣称 SingleApplication 安全控制；不静默升级为整屏 |
| Portal token/Node ID 可无限续用 | 协议版本、能力、一次性 token 轮换、流标识与会话世代分别校验 | 消费后崩溃的未知窗口不能盲重放旧 token；目标版本未验证不能宣称支持 |

详细契约见 [02 共同契约](../docs/02-common-contracts.md)、[04 Security](../docs/04-security-identity.md)、[05 Recovery](../docs/05-recovery-consistency.md)、[06 Storage/Data/TableFS](../docs/06-storage-data-tablefs.md)、[07 Application/GUI](../docs/07-applications-packages-gui.md)、[08 Task/Event](../docs/08-tasks-events.md)、[10 Conversation](../docs/10-conversations.md)、[11 Workflow/Project](../docs/11-workflows-projects.md)。

## 4. 实际工程迁移的顺序

1. **固定输入和权威。** 取得实际仓库完整 HEAD、制品摘要、当前数据库 Schema、启用 Profile 和资源清单；对每个字段确定唯一写入权威。归档 ZIP 的来源陈述不替代此步骤。
2. **先冻结可检查的边界。** 明确身份、授权、幂等、状态机、错误、持久 ACK 范围及兼容窗口；旧客户端未知安全字段应拒绝，不能由新服务默认忽略。
3. **建立可恢复检查点。** 验证跨域引用/附件/Artifact/审批账本/取消墓碑的恢复关系；保存独立恢复材料。数据恢复不意味着安全状态已证明新鲜。
4. **转换到隔离目标。** 默认在新目标导入并逐项校验身份映射、内容摘要、权限、删除屏障、去重窗口、版本和保留链。先读后写兼容也要明确其适用版本，不承诺任意 N 版本升级。
5. **阻断未知外部效果的重放。** 用原 operation/origin 去查询 Task、Repo/CI、外部 API；没有证据不能重复合并、部署、发送或支付。旧备份中的未完成标记不能证明效果从未发生。
6. **先完成实际 fencing 再开放新写者。** 新恢复世代必须被目标数据面持久认可；旧 Worker、打开句柄、桌面输入和挂载的撤销结果逐目标记录。不可达端不能假报已强制。
7. **执行与目标 Profile 匹配的迁移验收。** 运行本包旧测试、增量测试及故障注入；绑定 SHA、制品、环境、数据校验和复核记录。Schema 自检不替代这些产品测试。
8. **授权激活并记录回退边界。** 激活真实数据和应用与导入完成分开；失败时保留原目标及诊断。不可逆 Schema 收缩、已发生外部副作用和新安全代次不能借“回滚镜像”抹去。

## 5. 保留功能，不将困难目标删掉

三发行版、原生/第三方 Agent、可选集群、TableFS、Dataset/Knowledge、通用 JSON、DEB/RPM/Alpine APK、WebGUI、Chat/DM 和 PsiFlow/Project 都保留。变更主要把“应当支持”拆成可实现的合同、具体失败行为及启用前证据门槛。

TableFS 若不能在声明的 POSIX 子集、持久性、权限、导出和同负载对照中证明适用性，应维持普通文件系统加可选 Catalog/Index；这属于既定 Go/No-Go，不是偷偷删除 TableFS。多节点共享写、透明迁移、无头桌面、Android Runtime 和高级硬件继续按独立 Profile 启用，未测不宣称支持。

## 6. 目前已完成与仍未完成

已完成的是原文归档、编号提取、范围展开、同号冲突识别、v1 去向登记和文档级一致性修正。未完成的是当前 Forgejo HEAD 的代码复用审计、ADR 实际批准、实现、生产迁移、硬件/内核/桌面矩阵测试和任何软件发布。这些事实分别记录，不能用本目录存在或映射覆盖率为 100% 推导完成。
