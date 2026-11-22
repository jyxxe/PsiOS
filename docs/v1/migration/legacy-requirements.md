# 旧规范逐项迁移表

旧规范共 **245** 个唯一编号；已有明确 v1 文档去向 **245** 项，定位覆盖率 **100.0%**。该分母包括 BASE 的20项固定约束；旧附录单列的专业规范为225项。

所有结果为文档迁移定位；工程状态仍为 **NotVerified / NotRun**。每项保留原编号，重复出现在多个版本只算一个逻辑编号；不同文本版本和所有引用仍在JSON中分别保留。

完整机器登记：[legacy-requirements.json](../registries/legacy-requirements.json)。来源身份、文件ID及SHA：[source-inventory.json](../sources/source-inventory.json)。

`superseded_by` 表示 v1 的修正语义替代旧表述；`consolidated_into` 表示并入列出的明确条款；`retained_context` 表示保留产品边界、待决事项或可选能力门槛。三者都不等于实现完成。
 
## AGT（11 项）

### AGT-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-001](../docs/08-tasks-events.md)；[V1-AGT-001](../docs/09-agents-models-protocols.md)；[V1-AGT-002](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 任务与副作用语义，第 1214–1214 行。

> AGT-001（必须）：Agent Runtime 使用统一 Task & Event Engine，不能另建一套与普通 Job 不一致的权威任务状态。区分 Task（用户目标）、Run（一次执行尝试）、Step（可观察操作）、ToolCall（一次具体能力调用）、Artifact（结果）和 Session（交互上下文）。任务至少记录所有者、委托、输入与制品引用、执行版本、状态、预算、取消标志及父子关系。输入引用绑定不可变版本或摘要并建立任务期间的保留关系；Run 的规范标识为 run_id，跨节点重试创建新 Ru…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-002

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-TSK-002](../docs/08-tasks-events.md)；[V1-TSK-003](../docs/08-tasks-events.md)；[V1-TSK-007](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 任务与副作用语义，第 1216–1216 行。

> AGT-002（必须）：任务具有已接收、排队、运行、等待输入/授权/资源、暂停、取消中及成功/失败/取消终态。OutcomeUnknown 单独表示“副作用可能已发生而结果未确认”，不可混入普通失败自动重试。关键接受和状态提交记录持久化后，才对外报告相应已确认状态，并须可从持久日志恢复；模型对话历史不能充当任务事务日志。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-008](../docs/08-tasks-events.md)；[V1-TSK-005](../docs/08-tasks-events.md)；[V1-API-010](../docs/02-common-contracts.md)；[V1-REC-010](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 任务与副作用语义，第 1218–1226 行。

> AGT-003（必须）：执行器在副作用前持久化操作意图和操作 ID，随后记录调用、回执和结果。工具声明操作性质：只读、幂等写、可补偿写或不可安全重试的外部副作用。能与本机事务一并提交的操作采用同一事务边界；跨服务操作使用幂等键、结果查询或补偿。不得承诺对任意外部服务具备端到端 exactly-once。 | 故障点 | 必须采取的行为 | |---|---| | 调用前崩溃 | 恢复意图并重新检查授权及资源前置条件 | | 对端成功、回执丢失 | 用操作 ID 查询或重用被对端保证幂等的键；否则标记结果未知 | |…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-006](../docs/08-tasks-events.md)；[V1-TSK-016](../docs/08-tasks-events.md)；[V1-AGT-002](../docs/09-agents-models-protocols.md)；[V1-CLU-005](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 任务与副作用语义，第 1228–1228 行。

> AGT-004（必须）：取消、暂停、进程终止与业务回滚是不同操作。任务超时、模型额度耗尽、节点租约失效和用户接管分别记录原因。恢复时从已确认的步骤继续，不重放已完成副作用；如果输入、执行版本或效果相关资源版本改变，重新评估兼容性和授权。跨节点重试必须遵循集群的租约与 fencing 规范，避免两个执行者同时写同一资源。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-AGT-003](../docs/09-agents-models-protocols.md)；[V1-AGT-005](../docs/09-agents-models-protocols.md)；[V1-API-016](../docs/02-common-contracts.md)；[V1-API-018](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 任务与副作用语义，第 1230–1230 行。

> AGT-005（必须）：工具使用结构化输入输出、Schema 校验、大小限制、超时和明确错误类型。将授权拒绝、暂时故障、容量不足、前置条件冲突、版本不兼容与结果未知区分。工具结果及文件内容包含来源标识，默认不能执行其内嵌命令。只记录执行计划、调用和可核实结果，不以获取模型内部思维过程为运行前提。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-AGT-004](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › Skills 与兼容等级，第 1234–1234 行。

> AGT-006（必须）：Skill 描述、可执行依赖、工具实现、授权和沙箱分别版本化。导入时解析来源、完整性、能力申请和依赖；运行时继续执行系统策略。Skill 更新不可修改既有授权，模型对 Skill 文本的解释不可产生额外权力。删除 Skill 只移除注册与依赖引用，不隐式销毁任务制品或共享数据。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-AGT-008](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › Skills 与兼容等级，第 1236–1244 行。

> AGT-007（必须）：将“开放适配第三方 Agent”落实为兼容矩阵，而非对所有现存和未来框架作无条件保证。 | 等级 | 验证范围 | 不隐含的能力 | |---|---|---| | C0：受管理运行 | 能在声明的运行环境启动、停止、记录日志及限制资源 | 无法因此假定具有持久任务或系统工具访问 | | C1：受控能力接入 | 通过明确协议/CLI/SDK 调用授权能力并审计 | 无法因此假定支持恢复、取消或多 Agent 协作 | | C2：任务深度集成 | 映射任务、进度、取消、制品、恢复和委托语义 |…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-008

**迁移：** `superseded_by` / `Mapped`。MCP/A2A/AG-UI/OpenAPI职责分别保留；现代和旧代际不能混为同一握手/认证。

**去向：** [V1-AGT-009](../docs/09-agents-models-protocols.md)；[V1-AGT-010](../docs/09-agents-models-protocols.md)；[V1-AGT-011](../docs/09-agents-models-protocols.md)；[V1-AGT-012](../docs/09-agents-models-protocols.md)；[9.6 各协议负责什么](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › Skills 与兼容等级，第 1246–1248 行。

> AGT-008（必须）：各协议按用途接入：MCP 适合工具/资源接口，A2A 适合 Agent 发现与任务互操作，AG-UI 适合交互事件，OpenAPI 适合已描述 HTTP 服务；A8A9它们不能相互替代全部职责，也不提供通用代码沙箱。MCP 的 HTTP 授权与本地 STDIO 机制须分别实现，面向不同服务的访问令牌不能直接透传。A1A2 原稿中 ACP 必须写明含义。Agent Client Protocol 是客户端与 Agent 的交互协议；Agent Communication Protocol 是另…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-009

**迁移：** `consolidated_into` / `Mapped`。至少一个真实第三方与原生Agent同类任务验收仍保留。

**去向：** [V1-AGT-008](../docs/09-agents-models-protocols.md)；[9.9 最低能力与验收](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › Skills 与兼容等级，第 1250–1250 行。

> AGT-009（必须）：M3 至少验证一个独立第三方实现与原生 Agent 对同一工具完成同类任务，并测试拒绝、撤销、取消和故障路径。其余框架逐一登记成熟度；“进程能启动”不得标记为 C2。插件升级、协议弃用和接口映射变化遵循系统 API 兼容窗口，不依赖追随某个框架的未固定最新版。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-AGT-006](../docs/09-agents-models-protocols.md)；[V1-AGT-007](../docs/09-agents-models-protocols.md)；[V1-TSK-014](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 模型路由与运行预算，第 1254–1254 行。

> AGT-010（必须）：Model Gateway 独立于 Agent Runtime，声明文本/多模态、工具调用、上下文容量、并发、部署位置和计费/额度信息。任务指定允许的提供者范围、数据外发规则、成本/时间/调用次数/递归深度预算。计费估计与账单事实分别标注；预算耗尽应暂停或拒绝新调用，不影响本机 CLI 和系统管理。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### AGT-011

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-AGT-006](../docs/09-agents-models-protocols.md)；[V1-AGT-007](../docs/09-agents-models-protocols.md)；[V1-ARCH-004](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十、Agent：执行、第三方兼容与模型治理 › 模型路由与运行预算，第 1256–1260 行。

> AGT-011（必须）：模型切换重新检查能力和数据策略。原本仅允许本地处理的数据，不能因本地故障自动发往云端；工具调用能力或输出 Schema 不兼容时不得静默继续执行。上下文压缩、长期记忆与历史检索遵守用户隔离、来源及保留规则；未安装 Dataset/Knowledge 时，任务持久化和基础记忆仍可用，不建立强依赖。 Agent 群聊/私聊的消息属于 Conversation 服务，不直接混入 Session 或 Task 的事务日志；Agent 响应须通过能力/预算与身份策略，参与工作流只通过正式 Task/W…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、12 处引用；2 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## API（20 项）

### API-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-001](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 935–935 行。

> | API-001 | 必须给可持久引用的资源分配不可复用的稳定 uid，将名称、显示标签与身份分开；跨节点引用包含不可变的身份签发域与 UID；可变运行位置和管理权威另行记录，迁移不改变资源身份。 | 同名资源删除后重建，旧授权、旧任务和旧事件均不能命中新资源。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-002](../docs/04-security-identity.md)；[V1-SEC-003](../docs/04-security-identity.md)；[V1-API-017](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 936–936 行。

> | API-002 | 必须分别定义部署/管理范围及用户/项目安全域，均携带具体范围 ID、所有者、可见性及引用权限，不能只凭 scope=user 等标签授权；Linux UID/GID 映射关联节点及 user namespace，不能与 PsiOS Principal ID 混用。 | 不同用户或不同节点的同名资源互不越权；日志可追溯真实主体。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-ARCH-002](../docs/01-product-and-architecture.md)；[V1-API-009](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 937–937 行。

> | API-003 | 必须分别记录期望状态 spec 和观察状态 status；每个资源只有一个明确的状态写入权威。长期操作使用独立 Operation，不能以返回 HTTP 成功代替实际完成。Operation 是系统变更记录，可由 Task 的 Step/ToolCall 引用；生命周期阶段、健康条件和错误原因分别记录，不混成一个枚举。 | 应用启动请求已接受而进程失败时，Operation 和资源状态能区分接受、执行与失败。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-003](../docs/02-common-contracts.md)；[V1-API-004](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 938–938 行。

> | API-004 | 必须提供并发修改前提，例如不透明的 resourceVersion/ETag；修改与删除检查前提，冲突返回明确错误，不静默覆盖；审批另绑定效果相关的配置代数及依赖指纹，不因无关的状态/心跳更新失效。 | 两个客户端并发改同一资源，第二个旧版本写入被拒绝并可重新读取。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-005

**迁移：** `consolidated_into` / `Mapped`。StoragePool及Dataset记录身份的正式用语保留。

**去向：** [V1-API-001](../docs/02-common-contracts.md)；[1. 名词和单一事实来源](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 939–939 行。

> | API-005 | 必须统一命名：StoragePool 为正式资源名，原稿表中的 Pool 仅为草案旧称；Dataset 记录 uid 与系统资源 UID 分别标明所属空间。 | Schema、SDK、工具描述和 UI 使用一致的资源词汇。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-002](../docs/02-common-contracts.md)；[V1-STO-004](../docs/06-storage-data-tablefs.md)；[V1-APP-007](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.1 资源标识与所有权，第 940–940 行。

> | API-006 | 必须声明引用的生命周期规则：拥有、使用、依赖、派生四类关系分开；删除前报告阻塞依赖。卸载应用不级联删除独立数据资产。 | 有依赖卷、索引、备份时，删除计划展示影响范围，未授权数据仍存在。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-009](../docs/02-common-contracts.md)；[V1-API-010](../docs/02-common-contracts.md)；[V1-API-012](../docs/02-common-contracts.md)；[V1-API-013](../docs/02-common-contracts.md)；[V1-API-018](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 956–956 行。

> API-007：必须为有副作用的系统 API 定义输入验证、授权检查、幂等范围、超时、取消含义、结果持久化和补偿行为。只读请求也必须受数据权限、速率和响应大小限制。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-005](../docs/04-security-identity.md)；[V1-SEC-006](../docs/04-security-identity.md)；[V1-SEC-007](../docs/04-security-identity.md)；[V1-SEC-009](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 958–958 行。

> API-008：必须在高影响操作执行前生成可审阅的 OperationPlan，至少记录发起主体及委托链、目标 UID、效果相关的配置代数/内容版本/依赖指纹、操作类型、参数摘要、权限增量、数据影响、恢复条件、制品摘要和有效期限。审批绑定该计划的不可变摘要；目标、参数、效果相关依赖或权限扩大后旧审批失效；执行前重新校验授权与这些前提，纯进度更新不使审批失效。已批准范围内的普通后续步骤可沿用有效授权，不要求反复确认。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-009

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-API-010](../docs/02-common-contracts.md)；[V1-API-011](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 960–960 行。

> API-009：必须定义幂等键作用域为明确的主体、操作和有效时间窗口，并保存规范化请求摘要。相同键与相同请求应返回同一 Operation；相同键但参数不同必须返回冲突。幂等记录保留期必须覆盖可接受的重试窗口；超过有效期的旧键返回 IdempotencyKeyExpired 或通过墓碑拒绝，不能默默当作新操作。幂等键不能证明任意外部服务已恰好执行一次。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-010

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-API-009](../docs/02-common-contracts.md)；[V1-TSK-007](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 962–962 行。

> API-010：必须为 Operation 的公共执行语义区分 Pending / Running / WaitingApproval / WaitingDependency / Succeeded / Failed / CancelRequested / Cancelled / OutcomeUnknown 等必要语义；具体状态枚举可在 M0 统一，但不得把 OutcomeUnknown 自动转换为成功或可安全重试。Task、应用、卷、备份和更新保留各自专用生命周期，并映射公共执行语义。RecoveryRequi…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-011

**迁移：** `consolidated_into` / `Mapped`。错误码、重试条件、结果未知与恢复原因在统一错误表展开。

**去向：** [V1-API-009](../docs/02-common-contracts.md)；[V1-API-013](../docs/02-common-contracts.md)；[8. 错误约定](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 964–964 行。

> API-011：必须为 API 错误提供稳定代码、可否重试、关联 Operation、必要的重试条件和经过脱敏的说明。至少覆盖权限拒绝、授权过期、前提冲突、能力不支持、配额不足、依赖不可用、部分失败、结果未知及重建订阅。自动客户端不能只解析自然语言报错。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-014](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.2 操作计划与执行契约，第 966–966 行。

> API-012：必须规定分页和事件订阅的一致性边界。单一资源权威内，列表返回可用于后续订阅的修订游标；游标过期需显式要求重新列举。跨权威聚合返回来源游标或明确的增量重建协议，不要求全局全序；不能连续订阅的后端声明限制和有界重新列举方式。跨节点资源汇总可为最终一致视图，但须标明来源、观察时间和不可达状态。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-013

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-005](../docs/02-common-contracts.md)；[V1-CLU-001](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.3 本机、集群和应用控制权，第 970–970 行。

> API-013：必须为被管理资源登记一个当前期望状态权威：本机、指定集群控制器或用户会话。集群控制器只管理节点明确委托的资源范围；加入集群不能默认接管全部应用、用户目录或桌面。切换权威需要显式移交、版本检查与未完成任务处理。

**溯源范围：** 6 处定义、18 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-014

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-005](../docs/03-host-updates-network-devices.md)；[V1-OBS-001](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.3 本机、集群和应用控制权，第 972–972 行。

> API-014：必须提供独立于 Shell、Agent、模型服务和可选数据库的本机诊断与恢复入口。常规 CLI 与 WebUI 走同一授权 API；可信本地救援通道采用单独身份校验和最小操作集，不能设计成普通 Agent 可调用的绕过权限接口。

**溯源范围：** 6 处定义、18 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-015

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-005](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.3 本机、集群和应用控制权，第 974–974 行。

> API-015：必须对声明式管理规定人工修改与控制器的关系。对受管资源，人工变更要么更新同一 spec，要么进入带原因和期限的手动接管状态；控制器不能在用户修复时无限重写，也不能把未知宿主进程自动据为己有。

**溯源范围：** 6 处定义、18 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-016

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-ARCH-001](../docs/01-product-and-architecture.md)；[V1-ARCH-004](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.3 本机、集群和应用控制权，第 976–976 行。

> API-016：必须在服务注册中声明启动依赖、运行依赖和可选增强依赖，并检测硬依赖环。身份引导、根卷挂载、本地网络恢复和诊断不能依赖需要它们才能启动的应用服务。可选 Dataset/Knowledge/模型服务缺失应返回能力不可用，而不是拖垮 Core。

**溯源范围：** 6 处定义、18 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-017

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-008](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.4 版本与扩展契约，第 980–980 行。

> API-017：必须独立记录系统镜像、Core API、插件协议、资源 Schema、持久数据 Schema、应用制品和驱动 ABI 的版本。它们之间采用发布兼容矩阵，不能用一个全局版本号代替兼容性判断。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-018

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-REL-006](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.4 版本与扩展契约，第 982–982 行。

> API-018：必须在正式发布前给出弃用策略、支持窗口、滚动升级允许的版本组合和降级条件；窗口长短由发布 ADR 决定，不在没有测试结果时承诺固定 N 个版本。实验能力明确标识，不具有正式 API 的稳定承诺。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-019

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-008](../docs/02-common-contracts.md)；[V1-API-018](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.4 版本与扩展契约，第 984–984 行。

> API-019：必须对扩展字段设置命名空间、大小限制及权限；未知安全字段和未知请求动作应拒绝或明确报告不支持。可忽略的展示性元数据与安全关键字段采用不同规则，不把 OCI 对未知注解的宽容规则直接套用于 PsiOS 权限清单。G2

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### API-020

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEVX-001](../docs/13-operations-release-governance.md)；[V1-API-016](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十五、统一资源、控制权限和配置契约 › 15.4 版本与扩展契约，第 986–986 行。

> API-020：建议提供由同一 Schema 生成或验证的 CLI、SDK、OpenAPI/工具描述和契约测试；工具描述中标注只读/副作用、授权范围、结果分页及取消行为。SDK 不能暗中获取高于调用者的权限。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## APP（9 项）

### APP-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-001](../docs/07-applications-packages-gui.md)；[V1-APP-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 清单和运行时责任，第 1153–1163 行。

> APP-001（必须）：统一清单管理应用身份与生命周期；OCI、Flatpak、开发容器和强化沙箱使用各自 Runtime Driver。清单是 PsiOS 控制面的安装与运行声明，不取代 OCI 或 Flatpak 自身格式。原始第三方包可由适配器生成受管理描述，不能要求所有上游重新打包才可接入。 | 清单部分 | 必须表达的内容 | |---|---| | 元数据 | Schema 版本、应用 ID、版本、发布者、来源、许可证、制品摘要和签名证据 | | 兼容性 | CPU 架构、运行时、所需系统 API、可选…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 清单和运行时责任，第 1165–1165 行。

> APP-002（必须）：清单的“申请权限”、用户/管理员的“批准权限”和运行时的“实际权限”分别保存，可查询差异。更新申请新权限时不得沿用旧版本扩大后的授权。实际运行制品使用不可变摘要；标签可以作为发现入口，不能单独决定恢复时运行的字节。

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-003](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 清单和运行时责任，第 1167–1167 行。

> APP-003（必须）：Runtime Driver 声明其隔离、配额、设备、挂载及撤权能力。目标后端不能落实某项必需限制时，安装/启动应明确失败或改用经授权的替代后端，不能静默忽略限制。普通工作负载默认不获得宿主 PID/网络命名空间、全量根文件系统、管理 Socket 和任意设备访问。允许传统兼容应用提出例外，但需列出具体边界变化并由策略授权。

**溯源范围：** 6 处定义、16 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-003](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 清单和运行时责任，第 1169–1169 行。

> APP-004（必须）：Flatpak 的静态权限和 Portal 动态授权映射到统一权限视图；Portal 仍由对应桌面环境执行，不由 PsiOS 私有接口冒充已得到用户授权。上游文档表明两者是不同的授权路径，因此权限展示应分别说明。A3

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-004](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 安装、运行与升级，第 1173–1173 行。

> APP-005（必须）：生命周期至少包含 Absent → Resolving → Verified → Prepared → Installed → Starting → Running/Degraded → Stopping → Stopped，以及 Updating、Failed、Removing。状态是可查询的控制面事实，不能仅由进程是否存在推断。安装先验证依赖、来源、磁盘容量和权限，再创建卷及实例；操作有唯一 ID 和持久化进度，系统重启后能完成或撤销未完成步骤。

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-005](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 安装、运行与升级，第 1175–1175 行。

> APP-006（必须）：依赖是显式资源/服务条件，不是固定延时。启动失败具有可配置的重试上限与退避，避免重启风暴；“进程活着”“能够提供服务”和“适合接收新请求”分开表示。停止先停止接收新任务并执行关闭钩子，再按期限终止；强制停止保留原因和数据一致性风险。

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-006](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 安装、运行与升级，第 1177–1177 行。

> APP-007（必须）：同一应用/数据域的升级、恢复和卸载互斥，使用可恢复操作锁。升级必须验证 API/配置/数据兼容性，预备新版本，执行清单规定的迁移与健康检查，再切换。无状态应用可以快速回退镜像；有状态应用只有在数据 Schema 允许时才能回退二进制，否则进入恢复或前向修复流程。迁移钩子是受控工作负载，不得因是“安装脚本”取得任意宿主权限。

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-004](../docs/07-applications-packages-gui.md)；[V1-APP-007](../docs/07-applications-packages-gui.md)；[V1-STO-004](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 安装、运行与升级，第 1179–1179 行。

> APP-008（必须）：卸载默认删除运行实例及应用制品引用，保留持久数据并标记其归属。销毁数据需要独立操作与授权。共享卷、镜像层、模型缓存和依赖由引用关系控制回收；卸载一个应用不得破坏其他使用者。对卷配额、设备失联、日志空间耗尽和运行时失效定义 Degraded/Blocked 原因，不得误显示为安装成功或无数据。

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### APP-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-007](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › 安装、运行与升级，第 1181–1183 行。

> APP-009（建议）：应用仓库区分官方维护、社区和用户导入渠道；签名验证证明来源和完整性，不等于内容安全。提供离线安装包、依赖锁定、撤回公告和替换版本。应用、Skill、插件均有维护者、兼容范围和更新渠道；可执行插件在独立进程/沙箱运行，不能仅凭签名装入高权限 Core 进程。 M2 应完成 OCI 安装—运行—停止—持久化—卸载闭环和基础清单验证；M4 覆盖 Argon 的 Flatpak 映射；更强隔离后端按需求扩展。验收包括 APP-T01 安装中断后无孤立特权和误删卷，APP-T02 更新新增权限被拦截，…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、16 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## BASE（20 项）

### BASE-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-001](../docs/01-product-and-architecture.md)；[V1-ARCH-001](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 39–39 行。

> | BASE-01 | 基于 Linux 的 Agent-Native OS，传统管理方式完整可用 | Agent、模型、外网和可选数据引擎不能成为基础管理或救援的硬依赖。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-001](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 40–40 行。

> | BASE-02 | Helium＝精简、Neon＝服务器、Argon＝桌面 | 场景与默认组件不同，不按硬件档次锁功能；三版共享 Core。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-001](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 41–41 行。

> | BASE-03 | 三版都有原生 Agent、第三方接入及集群加入能力 | 不另建不支持 Agent 的精简内核，也不要求每机部署大模型或 Controller。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-002](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 42–42 行。

> | BASE-04 | 普通应用、Agent 执行和可选服务原则上隔离 | 内核、驱动、KWin、必要特权代理仍可在可信宿主；隔离方式按后端能力选择。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-05

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-001](../docs/01-product-and-architecture.md)；[V1-GUI-001](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 43–43 行。

> | BASE-05 | Argon 采用 KDE Plasma；PsiOS Shell 是共用 WebUI | 不重新设计替代 Plasma 的桌面，不为每个入口复制一套业务逻辑。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-06

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-003](../docs/01-product-and-architecture.md)；[V1-ARCH-004](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 44–44 行。

> | BASE-06 | Dataset/Knowledge 主线维护、可选安装 | 未安装时 Core、Agent 和基础存储照常工作。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-07

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-003](../docs/01-product-and-architecture.md)；[V1-DAT-004](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 45–45 行。

> | BASE-07 | 知识库是主数据集子集/视图与派生索引 | 不另建独立原文事实来源；Embedding 服务独立于数据引擎。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-08

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DAT-001](../docs/06-storage-data-tablefs.md)；[V1-DAT-002](../docs/06-storage-data-tablefs.md)；[V1-DAT-003](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 46–46 行。

> | BASE-08 | 主字段保留 uid/name?/content/tag/score/vector?/extra_data? | extra_data 仍为通用 JSON，支持精确查询；不把它缩成评分器私有字段。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-09

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-001](../docs/12-cluster-coordination.md)；[V1-CLU-010](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 47–47 行。

> | BASE-09 | 单机独立运行，Controller 是角色 | 高可用控制面、分布式存储、透明迁移继续分阶段，不成为首轮启动依赖。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-10

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-ARCH-005](../docs/01-product-and-architecture.md)；[V1-GOV-007](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 48–48 行。

> | BASE-10 | XeLab 只提供可选择复用的代码 | 审计以 Forgejo 实际最新 HEAD 为准，不承诺旧 ABI/API/数据库兼容。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-11

**迁移：** `retained_context` / `Mapped`。OS、默认FS、OCI与更新后端继续作为待ADR验证的候选，归档不构成技术选型批准。

**去向：** [V1-GOV-005](../docs/00-reading-and-authority.md)；[V1-FND-001](../docs/03-host-updates-network-devices.md)；[5. 尚未决定的事项](../docs/00-reading-and-authority.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 49–49 行。

> | BASE-11 | OS 基底、默认文件系统、OCI 后端、更新实现待验证 | 先冻结行为契约；不把技术候选改写成已经选定或已经实现。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-12

**迁移：** `retained_context` / `Mapped`。长期建设与证据阶段保留；文档版本不宣告日历工期或实现成熟度。

**去向：** [V1-GOV-002](../docs/00-reading-and-authority.md)；[V1-GOV-005](../docs/00-reading-and-authority.md)；[1. 产品目标与固定约束](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 50–50 行。

> | BASE-12 | 长期开发，无强制完成日期 | 以 M0–M6 证据门槛推进，不用日历日期或主观成熟度百分比代替完成标准。 |

**溯源范围：** 6 处定义、0 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-13

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-001](../docs/06-storage-data-tablefs.md)；[V1-TFS-001](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 51–51 行。

> | BASE-13（v0.3） | TableFS 是可选的表原生文件存储后端 | 普通应用看到目录/文件；底层目录元数据、可查询结构化属性和内容引用来自同一权威存储；不替换 Host Storage，不成为启动或基础恢复依赖。 |

**溯源范围：** 5 处定义、5 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-14

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-007](../docs/06-storage-data-tablefs.md)；[V1-DAT-004](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 52–52 行。

> | BASE-14（v0.3） | TableFS 与 Dataset/Knowledge 明确分层 | inode_id 与 (dataset_id, uid) 不混同；Dataset 字段不变，知识库仍是选择性视图和派生索引，必须限制数据副本与权限泄露。 |

**溯源范围：** 5 处定义、5 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-15

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-008](../docs/07-applications-packages-gui.md)；[V1-APP-009](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 53–53 行。

> | BASE-15（v0.4） | DEB、RPM、Alpine APK 共存是应用兼容目标，不是宿主包管理器混装 | 通过适配器和隔离的发行版用户空间运行；按应用 ABI/架构/依赖声明实际支持等级；Android APK 是独立生态。 |

**溯源范围：** 4 处定义、4 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-16

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 54–54 行。

> | BASE-16（v0.4） | PsiOS Shell 可作为图形应用交互入口，但不替代 KDE Plasma | GUI Bridge 支持完整桌面、单应用窗口及原生 Web 入口；图形观察与输入必须复用 DesktopSession、ControlLease、Identity/Policy/Audit。 |

**溯源范围：** 4 处定义、4 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-17

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-001](../docs/10-conversations.md)；[V1-CHAT-002](../docs/10-conversations.md)；[V1-CHAT-009](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 55–55 行。

> | BASE-17（v0.5） | Agent 群聊、私聊是 PsiOS 原生协作入口 | 人类与原生/第三方 Agent 是独立身份的会话主体，允许项目群、任务群、临时群和一对一私聊；群聊不会自动授予资源操作权限。 |

**溯源范围：** 3 处定义、3 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-18

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-004](../docs/01-product-and-architecture.md)；[V1-ARCH-002](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 56–56 行。

> | BASE-18（v0.5） | Chat、Workflow 与 Project 分工清楚 | Conversation 保存交流与消息；Workflow 管理可恢复执行及验证；Project 管理长期目标与工作项；不得从一条自然语言消息直接推导为已批准、已执行或已验收。 |

**溯源范围：** 3 处定义、3 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-19

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-009](../docs/10-conversations.md)；[V1-CHAT-010](../docs/10-conversations.md)；[V1-AGT-007](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 57–57 行。

> | BASE-19（v0.5） | Agent 可自主沟通但须有界 | Agent 可以主动发言、@指定 Agent、发起私聊/邀请；唤醒、自动回复、跨群传播和相互委托受预算、限流、权限、递归深度与撤销约束，不得形成无限模型调用循环。 |

**溯源范围：** 3 处定义、3 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### BASE-20

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-ARCH-003](../docs/01-product-and-architecture.md)；[V1-ARCH-004](../docs/01-product-and-architecture.md)；[V1-CHAT-014](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 零、修订基线与阅读约定（0.1 保留 v0.2 历史说明） › 0.2 继续有效的设计约束，第 58–58 行。

> | BASE-20（v0.5） | 共享 Core 与精简节点自治 | 三版可按需运行基础 Conversation/Task API；完整 Project UI、集群控制器与复杂编排可选装，不成为 Helium/Argon 单机使用或救援的硬依赖。 |

**溯源范围：** 3 处定义、3 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## CHAT（16 项）

### CHAT-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-001](../docs/10-conversations.md)；[V1-CHAT-002](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.1 会话与资源模型，第 1988–1988 行。

> CHAT-001（必须）：Conversation 支持上述四种会话类型，成员可为用户、Agent 或受控服务主体。提供创建、查询、加入/邀请、退出、归档及按策略删除操作；不要求所有 Agent 在同一节点或同时在线。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-002](../docs/10-conversations.md)；[V1-CHAT-003](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.1 会话与资源模型，第 1990–1990 行。

> CHAT-002（必须）：成员主体以统一 Principal/Agent ID 识别，明确创建者、所有者、成员角色和 ACL/ABAC 域；每次读取或发言按当前主体和委托范围授权，不能仅依赖前端显示的成员列表。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-002](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.1 会话与资源模型，第 1992–1992 行。

> CHAT-003（必须）：Agent 能按授权创建 DM、AdHocRoom、邀请其他 Agent 和向特定 Agent 发请求；没有 Manager 人工转发不是权限豁免。跨项目邀请、将私聊扩为群聊、共享历史以及邀请外部远程 Agent 需要额外策略检查。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-004](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 1996–1996 行。

> CHAT-004（必须）：消息信封至少包含 message_id、conversation_id、thread_id?、sender_principal_id、reply_to?、mentions[]、message_type、schema_version、created_at、服务器确认序列、correlation_id?、causation_id?、task_id?、正文及受控附件引用。结构化 request/response/decision/approval_request/task_event 类型可以扩…（完整原文见JSON与归档）

**溯源范围：** 3 处定义、17 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-005

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-CHAT-004](../docs/10-conversations.md)；[V1-CHAT-005](../docs/10-conversations.md)；[V1-CHAT-006](../docs/10-conversations.md)；[V1-CHAT-012](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 1998–1998 行。

> CHAT-005（必须）：服务端按 Conversation 提供稳定的追加顺序与游标，并定义至少一次通知/幂等发送语义；不承诺跨会话全局顺序。消息重发使用稳定幂等键；重写与撤回须采用保留审计语义的版本/墓碑，不能暗中篡改已经引用的消息。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-008](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 2000–2000 行。

> CHAT-006（必须）：离线 Agent 的消息通知和未处理请求须持久排队、限额并支持可恢复游标与已处理确认；读取消息正文时重新授权。重复投递不能触发重复不可幂等副作用。到期、容量满、无权/未知接收者和死信均有明确状态，不伪装成已送达。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-009](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 2002–2002 行。

> CHAT-007（必须）：普通群消息默认只记录和按订阅推送，不唤醒所有 Agent；显式 @Agent、有效 DM、新 Task 分配、授权订阅匹配可以唤醒指定 Agent。群通知与业务执行分开：唤醒失败不丢消息，也不能默认推迟所有任务进度。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-009](../docs/10-conversations.md)；[V1-CHAT-010](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 2004–2004 行。

> CHAT-008（必须）：Agent 可以主动发言、跟进讨论、回复他人或在私聊确认后发群内摘要，但行为受当前授权、订阅范围、发送速率、Token/成本/时间预算、私聊可见范围与自动交流策略约束。被授权自主发言不等于无限循环权限。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-010](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.2 消息协议、投递与路由，第 2006–2006 行。

> CHAT-009（必须）：自动回复和委托链须有去重、因果链追踪、跨 Agent 最大跳数、最大自动消息/轮次、并发上限、冷却/退避、预算终止条件和手动暂停。Agent 自己发送的消息、机器人回声与桥接回流默认不得再形成无限唤醒回路；预算不足时准确报告，不占用宿主必要管理资源。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-004](../docs/04-security-identity.md)；[V1-CHAT-003](../docs/10-conversations.md)；[V1-CHAT-009](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.3 权限、隐私与任务边界，第 2010–2010 行。

> CHAT-010（必须）：群成员关系、Project 成员关系和执行资源 Grant 是三套不同授权。加入群不自动暴露聊天外文件/密钥/桌面；收到 @/DM 不继承发信人更高权限。聊天消息、Issue、网页、Tool 输出按不可信内容处理，不能通过文字变更 Policy。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-011

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-009](../docs/10-conversations.md)；[V1-BASE-004](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.3 权限、隐私与任务边界，第 2012–2012 行。

> CHAT-011（必须）：消息可以引用正式 WorkItem/Task/WorkflowRun，或使用受控 API 发起结构化委托与审批请求。文字“执行”“已完成”“批准合并”不自动改变工作流、系统权限或外部 Repo 状态；正式状态转移必须记录操作主体和核验结果。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-011](../docs/10-conversations.md)；[V1-CHAT-012](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.3 权限、隐私与任务边界，第 2014–2014 行。

> CHAT-012（必须）：每次唤醒只给 Agent 当前授权范围内的相关消息窗口、摘要、引用和附件；跨群、DM、跨项目、历史检索和长期记忆均重新授权。私聊信息不会自动共享给参与者所在的其他群；摘要发布也需经过数据分类和权限检查，不以模型压缩过程绕过 ACL。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-013

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-011](../docs/10-conversations.md)；[V1-CHAT-012](../docs/10-conversations.md)；[V1-REC-023](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.3 权限、隐私与任务边界，第 2016–2016 行。

> CHAT-013（必须）：Message/Attachment/Delivery 的保存期、容量、搜索索引、删除/撤回、导出、隐私和审计分别定义。关键 Task/审批证据依赖消息时必须保存稳定、可验证的受控引用或单独审计记录；普通聊天删除不能擦除法定/强制审计，但也不能让审计系统默认泄露完整私聊正文。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-014

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-013](../docs/10-conversations.md)；[V1-AGT-008](../docs/09-agents-models-protocols.md)；[V1-AGT-011](../docs/09-agents-models-protocols.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.3 权限、隐私与任务边界，第 2018–2018 行。

> CHAT-014（必须）：第三方 Agent 接入至少经受控 CLI/SDK/API Adapter 映射成员身份和投递/回复状态；A2A 适合 Agent 间请求/任务语义，MCP 适合工具调用，但任一协议不被冒充为多成员 Room 治理标准。跨平台聊天桥接应显式标注身份映射、安全等级和外部数据流向，未验证的能力必须报告不支持。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-015

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-006](../docs/10-conversations.md)；[V1-CHAT-007](../docs/10-conversations.md)；[V1-API-014](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.4 故障、部署与 UI，第 2022–2022 行。

> CHAT-015（必须）：本机消息持久化和离线管理不依赖远端 Controller、模型服务或可选 Dataset；跨节点时按 CLU/EVT 的租约、认证、分区和恢复语义降级。连接丢失后不能凭旧会话缓存继续越权读取；消息历史、搜索和事件通知须有明确一致性与水位提示。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CHAT-016

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CHAT-014](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十四、Agent Chat：群聊、私聊与自主协作（v0.5） › 34.4 故障、部署与 UI，第 2024–2024 行。

> CHAT-016（必须）：Shell 提供群列表、DM、@提及、线程、消息搜索、任务关联、成员控制和暂停 Agent 自动回复入口；CLI/SDK/API 具备同等能力。不同发行版默认 UI 与可选服务可以不同，不能因 Project Studio 未装就让基础 Agent Chat 不可用；用户总能停用某个 Agent 的主动发言而不必关闭整个系统。

**溯源范围：** 3 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## CLU（12 项）

### CLU-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-001](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 身份、控制权与阶段边界，第 1521–1521 行。

> - CLU-001｜本地自治。 三个发行版必须包含加入集群所需的基础能力，Controller 仍为可选角色。节点开机、本地登录、宿主恢复和本地工作负载不依赖 Controller、外网模型或集群数据库在线。集群任务与本地任务使用可区分的所有权与配置命名空间；集群策略只能在节点明确授予的资源范围内生效。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-002](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 身份、控制权与阶段边界，第 1523–1523 行。

> - CLU-002｜节点准入。 节点加入须完成集群身份验证、节点身份登记、权限授权及能力/版本协商。引导凭据必须限制用途、有效期和使用次数；正式通信使用可轮换、可撤销的节点身份，不能长期共用万能入群密钥。节点证书轮换、机器重装、身份冲突、时钟偏差及密钥泄露均有明确恢复路径。系统镜像克隆后不得复用原节点私钥或节点 ID。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-003](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 身份、控制权与阶段边界，第 1525–1525 行。

> - CLU-003｜成员状态。 状态为 Joining → Ready → Draining → Left，并包含 Unreachable、Quarantined。失联表示观测不到，不能直接推断节点已经停机。退出先停止接收任务、处理在途任务和存储引用，再撤销凭据；强制移除必须保留残留工作负载与数据清单。重新加入须核对身份和代次，旧节点不得携旧授权直接恢复写入。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-010](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 身份、控制权与阶段边界，第 1527–1527 行。

> - CLU-004｜控制面分阶段。 M4 首先实现单 Controller 的可持久化状态与受控重启恢复，明确其故障窗口；不声称高可用。启用多 Controller 前再冻结一致性存储、成员变更、领导者选举、故障域和灾难恢复 ADR。若采用 etcd 等多数派方案，必须满足其仲裁要求，不能把两个投票节点互相复制称为可容忍任一节点故障的 HA。D2 本机独立运行不要求部署该一致性存储。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-004](../docs/12-cluster-coordination.md)；[V1-CLU-005](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 租约、分区和任务副作用，第 1531–1531 行。

> CLU-005｜任务领取。 Task Broker 为每次领取分配 task_id、run_id、任务版本、集群恢复世代及领取租约。Run 标识执行尝试；共享资源 fencing token 必须由该资源的租约权威统一排序，覆盖所有竞争写入该资源的任务，不得用各任务独立计数的 Run 编号替代。Worker 以任务身份和实际节点权限执行；结果提交先核对恢复世代、领取租约/Run 与任务状态，再确认持久化。重复领取与重复结果提交必须可识别；大结果使用受授权对象引用，不能无限扩大控制面日志。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-005](../docs/12-cluster-coordination.md)；[V1-CLU-006](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 租约、分区和任务副作用，第 1533–1533 行。

> CLU-006｜租约并非隔离措施。 心跳或租约过期只证明控制面不能继续信任该次领取，并不证明旧进程已停止。涉及共享可写资源的工作负载，必须由资源端校验 fencing token（资源授权世代和代次），或先验证旧实例已停机/失去写权限，再重调度。后端不支持可靠 fencing 时，必须暂停自动迁移并报告 RecoveryRequired，不能为了“自动恢复”制造双写。对于外部不可撤销副作用，未知结果进入待核实状态，不做无条件重试。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-007](../docs/12-cluster-coordination.md)；[V1-FND-006](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 租约、分区和任务副作用，第 1535–1535 行。

> CLU-007｜离线语义。 每种工作负载必须声明断联策略：本地独立任务可继续；允许离线的只读任务按有效授权继续；幂等性仅决定重试能力，不替代所有权、资源前提、授权新鲜度或 fencing，离线写入必须同时满足全部约束；依赖独占租约的共享写入任务在续约失效后停止相关写入；需要即时在线审批的操作暂停。队列和结果可在有界本地空间暂存，重连后以事件游标、任务版本、Run 和资源授权代次对账，禁止按最后写入时间盲目覆盖状态。本地经过时间使用合适的单调时钟，不跨节点比较各自单调钟数值；证书、绝对有效期与计划任务使用可信实时时间…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-ARCH-002](../docs/01-product-and-architecture.md)；[V1-CLU-010](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 租约、分区和任务副作用，第 1537–1537 行。

> CLU-008｜一致性分层。 集群成员、授权、任务归属和调度代次必须由单一权威顺序更新；本地宿主状态由节点负责；应用事务由应用/数据库负责；用户文件不因加入集群自动复制。状态快照须注明版本和观测时间。若采用多数派控制面，失去多数派时停止需要共识的新变更，已有本地服务按既定断联策略运行；恢复多数派并对账后再开放调度。D3

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-011](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 调度、网络、存储与升级，第 1541–1541 行。

> - CLU-009｜放置约束。 Placement 至少包含架构、Runtime、CPU/内存、GPU能力与访问模式、数据位置、资源授权及优先级。节点广告不等于资源预留，调度后必须由节点再次准入。资源不足返回排队/拒绝原因；不得通过强占用户桌面或未经授权复制数据来满足任务。GPU 显存预算与厂商能力按 Device Service 的实际能力映射。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-011](../docs/12-cluster-coordination.md)；[V1-NET-004](../docs/03-host-updates-network-devices.md)；[V1-STO-003](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 调度、网络、存储与升级，第 1543–1543 行。

> - CLU-010｜连接与数据路径。 服务发现只返回授权范围内的端点和能力，注册不等于获准调用；跨节点传输必须认证、加密并受网络策略控制。支持受限网络或主动连接 Worker 的方式由网络 ADR 选择，不能假定节点互相可达。Storage Federation 首先提供显式远程访问；跨节点挂载、多写和透明迁移只有通过后端验收后才能公布。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-011

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-012](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 调度、网络、存储与升级，第 1545–1545 行。

> - CLU-011｜版本与升级。 入群时协商 API、任务格式、驱动和能力版本。兼容窗口由发布矩阵给出；不兼容节点可以保持本地可用，但不得领取无法理解的任务。滚动升级先排空或检查点化，再升级并复核能力；任务检查点是否可跨版本恢复由具体 Runtime 明示，不能假定运行中进程可以无损迁移。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CLU-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-CLU-008](../docs/12-cluster-coordination.md)；[V1-CLU-009](../docs/12-cluster-coordination.md)；[V1-REC-013](../docs/05-recovery-consistency.md)；[V1-REC-014](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十四、Cluster & Coordination：集群与分布式运行 › 调度、网络、存储与升级，第 1547–1547 行。

> - CLU-012｜控制面灾难恢复。 备份必须覆盖成员/授权/任务状态、版本及恢复引导材料，密钥按独立策略保护。恢复旧控制面快照后，不得仅将备份中的整数代次加一。须利用资源写入端独立保留的最新 fencing 状态恢复单调性，或经授权建立新的恢复世代 incarnation，由写入端持久认可新世代并拒绝旧世代。世代按明确绑定校验，不比较随机 ID 大小。相关写入端确认新世代或旧写入者可靠隔离前，不得重新调度该资源；随后核实租约与存活 Worker，防止旧任务重放。不能仅恢复数据库后立即派发未完成任务。该规则同样适用…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## CU（8 项）

### CU-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 会话、权限与执行边界，第 1268–1268 行。

> CU-001（必须）：建立 DesktopSession 和 ControlLease 资源。前者标识用户、安全域、显示后端、物理/隔离虚拟类型、可见表面和生命周期；后者绑定 Agent、任务、观察/输入/剪贴板权限、有效期和会话代次。API、无障碍、浏览器自动化及视觉路径使用同一授权范围；换用视觉路径不能突破 API 被拒绝的权限。

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 会话、权限与执行边界，第 1270–1270 行。

> CU-002（必须）：默认优先使用隔离会话；对已有用户桌面的操作需命中该会话的有效授权。创建虚拟桌面不意味着可以接入其他用户的 D-Bus、浏览器配置、认证 Cookie、剪贴板或 home。浏览器配置独立保存，仅按显式授权导入必要状态。Argon 提供 KDE/Wayland 集成；Neon、Helium 按原稿提供可选后端，未安装时报告能力不可用，不强迫精简系统携带图形栈。

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 会话、权限与执行边界，第 1272–1272 行。

> CU-003（必须）：屏幕观察、键盘、鼠标、剪贴板读/写、文件传输、麦克风/摄像头分别授权。授权范围必须按后端实际能够限制的表面显示；不能把整屏输入冒充“仅单窗口操作”。遇到无法可靠隔离的敏感界面，暂停自动化并允许用户处理，或转到独立会话。屏幕脱敏属于尽力而为，不能承诺从任意应用中可靠识别全部密码与秘密。

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-003](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 输入仲裁、撤销和恢复，第 1276–1276 行。

> CU-004（必须）：同一桌面会话的输入控制采用单一写入租约；多个 Agent 的操作排队，不能同时抢鼠标或交错输入。观察者单独授权。人类接管优先，提供不依赖模型和 Agent 正常运行的本地停止入口；触发后撤销输入、清空待发送输入并关闭不再授权的捕获流。物理输入接管策略可配置，但界面必须清楚显示当前控制者。

**溯源范围：** 6 处定义、14 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-004](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 输入仲裁、撤销和恢复，第 1278–1278 行。

> CU-005（必须）：会话注销、锁屏、用户切换、显示器/窗口变化、休眠恢复及后端重启使旧目标定位和租约失效或重新验证。不得将旧截图坐标自动应用到另一会话或另一窗口。任务检查点保存语义目标和已执行动作，不保存一串坐标作为无条件重放脚本；无法验证的在途动作同样进入结果未知流程。

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-005](../docs/07-applications-packages-gui.md)；[V1-GUI-006](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 输入仲裁、撤销和恢复，第 1280–1280 行。

> CU-006（必须）：使用实际支持的 XDG RemoteDesktop/ScreenCast、PipeWire 和输入后端进行会话创建、授权、数据流及结束管理。上游接口提供选择设备/来源、启动与关闭会话机制，但“协议有接口”不等于目标 KDE 后端已满足全部能力。M4 前必须完成指定桌面版本的端到端 PoC，登记可用输入方式、持久授权、屏幕范围和撤销行为。A6A7

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-005](../docs/07-applications-packages-gui.md)；[V1-GUI-006](../docs/07-applications-packages-gui.md)；[V1-GUI-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 输入仲裁、撤销和恢复，第 1282–1282 行。

> CU-007（必须）：无人值守任务使用专用会话及事先授权策略；仅在后端明确支持时使用持久授权或恢复令牌。不得自动操作授权弹窗批准自身访问，也不得静默绕过 Portal 的权限判定。需要交互授权而当前没有用户时，任务转为等待授权；恢复令牌失效、权限撤销或目标屏幕消失后重新建立有效授权。A6A7

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### CU-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-009](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › 输入仲裁、撤销和恢复，第 1284–1286 行。

> CU-008（必须）：截图、输入录制和剪贴板属于敏感任务数据，按用户/任务隔离且采用明确的保留与删除策略；默认保留操作事件和必要证据，不录制全部键盘内容。通过受控凭据入口注入秘密时不得把明文返回模型；若应用只能由用户完成登录，提供用户接管点。暂停捕获并不能删除此前已合法发送给外部模型的画面，应在授权界面说明数据去向。 M3 完成会话和授权模型；M4 验证 Argon 实际桌面与至少一种按需无头会话。验收：CU-T01 两 Agent 同时输入只允许一个有效租约；CU-T02 用户停止后无排队输入继续到达；CU-T0…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、10 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## DAT（6 项）

### DAT-001

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-DAT-001](../docs/06-storage-data-tablefs.md)；[V1-DAT-003](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1408–1408 行。

> - DAT-001｜保持字段契约。 主记录字段仍为 uid、可选 name、content、tag、score、可选 vector、可选 extra_data。uid 在所属数据集内稳定且不可复用；跨数据集使用 (dataset_id, uid) 定位。extra_data 保持通用 JSON，可容纳多来源元数据；评分器只能更新其授权的元数据范围，不能覆盖其他内容。字段具体类型、缺省值、未评分表示、JSON 路径语义和表达式语言须由版本化 Schema 冻结并测试，不能由底层数据库临时推断。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DAT-002

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-DAT-002](../docs/06-storage-data-tablefs.md)；[V1-DAT-003](../docs/06-storage-data-tablefs.md)；[V1-DAT-005](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1410–1410 行。

> - DAT-002｜大记录与查询。 原文的逻辑记录边界必须保留；书籍、仓库等大记录采用流式导入和读取，物理分块不改变主记录身份。索引块必须引用父记录 ID、内容版本及块位置。JSON 精确查询、全文查询和向量相似度查询应为不同操作，返回各自的匹配语义；禁止把近似向量匹配当作 JSON 精确过滤。查询表达式必须经过解析、类型校验、资源预算及权限过滤，不能作为任意代码执行入口。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DAT-003

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-DAT-002](../docs/06-storage-data-tablefs.md)；[V1-DAT-004](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1412–1412 行。

> - DAT-003｜版本与派生关系。 Dataset、View 和 Index 必须可追溯到明确数据版本。视图须声明查询定义与读取方式：固定版本或动态最新版本。索引记录主数据版本、水位、过滤定义版本、切块策略、Embedding 提供者与模型版本/向量维数；模型变化需新索引代次或受控重建，不能混用不兼容向量。Embedding 仍是独立能力服务，不写死模型，也不恢复使用已弃用模型作为默认值。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DAT-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DAT-004](../docs/06-storage-data-tablefs.md)；[V1-DAT-005](../docs/06-storage-data-tablefs.md)；[V1-REC-023](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1414–1414 行。

> - DAT-004｜发布与删除传播。 新索引先在后台构建，校验后原子切换引用，旧代次按引用与保留策略回收。增量事件必须可重放和去重，缺口要触发对账或重建；每次检索返回索引水位及是否落后。授权权威确认删除/撤销后，后续在线查询必须通过当前授权代次和删除水位屏障；缓存按主体、资源与授权代次校验，不能等待异步索引刷新才阻止访问，已有数据流按撤销契约关闭。无法验证当前要求的共享读取/检索默认拒绝；只有明确允许离线且仍有效的授权可以例外，撤销保证受 SEC-008 的有限离线期限约束，不能宣称跨分区瞬时撤销。重连或收到撤销后…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DAT-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DAT-004](../docs/06-storage-data-tablefs.md)；[V1-DAT-005](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1416–1416 行。

> - DAT-005｜权限与去重。 知识库只引用主数据和必要派生索引；为性能保留的片段或缓存须标明来源、用途和回收策略，不形成无主第二份原文。所有列表、计数、搜索、导出和检索都必须执行资源权限；缓存按授权范围隔离，不能泄露其他用户数据存在性。内容去重不得改变数据所有权或绕过访问控制。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DAT-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DAT-006](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 数据集、知识库与数据生命周期，第 1418–1418 行。

> - DAT-006｜导出与退出。 数据集须提供与具体引擎解耦的导出契约，包含主记录、Schema、元数据、版本与清单校验；索引可重建时允许不导出其物理格式，但须保留生成配置。服务卸载默认保留数据并列出仍依赖它的应用/视图；删除主数据须单独授权，并说明尚在保留期或离线备份中的副本。未安装 Dataset/Knowledge 时，其 API 返回 ServiceNotInstalled，不阻塞 Core、普通 Agent、基础存储或本机恢复。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## DEV（5 项）

### DEV-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEV-001](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › DEV-01：设备对象与热插拔，第 1343–1345 行。

> DEV-01：设备对象与热插拔 Device Service 必须维护稳定设备身份、驱动/固件、能力、所属节点、健康、可用性和租用关系。GPU/NPU、USB、音频、摄像头、蓝牙、打印和外接存储按类适配，不要求重造上游驱动。插拔、设备重置和驱动升级须发布事件并使受影响的租用与工作负载进入明确状态；路径变化不能把权限误授予新插入设备。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DEV-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEV-002](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › DEV-02：计算资源契约，第 1347–1351 行。

> DEV-02：计算资源契约 CPU、主内存、进程数、块设备 IO、临时存储与设备访问须由宿主资源控制落实，任务/应用/用户配额分别计量。应当为宿主管理与恢复保留可用资源并限制日志、结果、缓存和失败重试增长。cgroup v2 可提供多类资源控制，其控制器与实际执行结果需要在目标平台验证。F5 GPU 调度必须区分整卡独占、厂商支持的硬件分区、共享运行和仅可观察模式。显存请求、预算、实际用量与可强制限制能力分别报告；没有硬件/驱动隔离时不得宣称任意进程获得严格显存配额或安全多租户隔离。先支持单机准入与回收，再扩展跨节…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DEV-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEV-002](../docs/03-host-updates-network-devices.md)；[V1-DEV-003](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › DEV-03：资源不足与公平性，第 1353–1355 行。

> DEV-03：资源不足与公平性 任务提交前做资源准入；运行中发生内存耗尽、显存不足、设备丢失或 IO 阻塞时记录具体原因，释放租用并按任务幂等性决定重试。不得无限重试显存不足任务。交互桌面、核心管理与后台任务的优先级必须可配置且可观察；集群不能未经本机策略批准占满用户桌面资源。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DEV-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-002](../docs/03-host-updates-network-devices.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)；[V1-GUI-007](../docs/07-applications-packages-gui.md)；[V1-SEC-003](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › DEV-04：多用户、图形和传统设备体验，第 1357–1359 行。

> DEV-04：多用户、图形和传统设备体验 系统用户、服务身份和 Agent 身份必须分离；各用户设备、剪贴板、通知、音视频与图形会话授权以实际会话归属为界。Argon 的显示、输入法、键盘布局、多显示器、缩放、音频路由、外接设备、无障碍和网络登录应进入桌面验收矩阵。全局服务与用户服务的启动、注销后驻留及资源计费必须有不同规则。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### DEV-05

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEV-004](../docs/03-host-updates-network-devices.md)；[V1-UPD-005](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › DEV-05：电源与固件生命周期，第 1361–1363 行。

> DEV-05：电源与固件生命周期 电源服务统一处理关机、重启、睡眠、休眠、唤醒、低电量、温度与恢复后的设备重新探测；具体能力由硬件验证决定。长任务可申请有上限、可撤销的抑制休眠请求。Neon 的无人值守启动与掉电恢复、Argon 的合盖/睡眠恢复分别验收。恢复后重新检查网络会话、设备租用和任务授权，不沿用已失效的租约。固件升级独立报告可逆性，不承诺由 OS 回滚恢复不可逆固件更新。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## EVT（7 项）

### EVT-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-002](../docs/08-tasks-events.md)；[V1-TSK-003](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1565–1565 行。

> - EVT-001｜有界接受。 Task Broker 必须按任务数、输入字节、未完成结果字节及单用户配额限制容量；入队与预留必要元数据容量必须原子化，超过上限返回明确的 QueueFull/QuotaExceeded。只有任务及必要输入引用持久化后才能返回接受成功。输入引用须绑定不可变版本/摘要，验证存在性、访问权限和任务期保留关系；未就绪输入显式进入依赖等待，不使用可变路径冒充可恢复输入。各类任务可共享统一契约，但不要求不同隔离等级或节点共用单一物理数据库。

**溯源范围：** 6 处定义、9 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-004](../docs/08-tasks-events.md)；[V1-TSK-010](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1567–1567 行。

> - EVT-002｜结果持久化。 工作负载完成、结果数据已写入、结果可查询是不同状态。成功状态必须绑定已持久化结果或明确声明的无结果任务；大产物先完成上传校验再提交引用。结果回收后保留足够任务元数据，并显式返回 ResultEvicted 及清理原因；不能把已回收结果伪装成任务从未存在。保留和容量政策可配置，不引入旧项目的固定磁盘预算。

**溯源范围：** 6 处定义、9 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-011](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1569–1569 行。

> - EVT-003｜事件信封。 事件至少带 event_id、类型与 Schema 版本、来源节点/服务、资源 ID 与版本、task_id/run_id、发生及接收时间、关联 ID、必要授权上下文引用。敏感明文与令牌不进入事件总线。事件读权限与原资源绑定；按用户筛选必须在服务端执行。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-011](../docs/08-tasks-events.md)；[V1-TSK-012](../docs/08-tasks-events.md)；[V1-TSK-013](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1571–1571 行。

> - EVT-004｜投递语义。 持久事件采用可恢复游标与至少一次投递，消费者必须用事件 ID 去重；仅在声明的资源或分区内提供顺序，不承诺全系统全序或通用 exactly-once。状态变更与事件写入采用同事务 outbox 或等价可靠协议；去重记录、处理结果与消费进度须同事务提交或采用等价可恢复协议；不得先标已处理再执行副作用，消费进度只在结果持久化后确认。临时进度事件可降级为尽力投递，但最终任务结果和关键审计不能混入此类。

**溯源范围：** 6 处定义、15 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-012](../docs/08-tasks-events.md)；[V1-TSK-013](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1573–1573 行。

> - EVT-005｜背压与毒消息。 消费者落后时限流、暂停或按已声明策略隔离；重试必须有预算、退避和终止条件，不能无限增长。无法解析或反复失败的事件进入隔离队列，带原因和可审计重放入口。事件历史被裁剪后返回游标过期，要求从资源快照重新同步，不伪装成“没有变化”。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-006](../docs/08-tasks-events.md)；[V1-TSK-016](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1575–1575 行。

> - EVT-006｜取消与停机。 取消请求必须记录为独立操作，终止新步骤、发出协作式取消，并在宽限策略后按能力终止执行环境；已发生副作用由 Agent/应用补偿契约处理。系统关机先停止接单并持久化在途进度，再使相关执行者进入无写入安全点，或确认资源端 fencing/撤权生效，之后才释放会触发重调度的租约并结束服务；无法确认时保持待恢复状态，禁止自动双写。单纯丢失前端连接不得视为用户取消任务。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EVT-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TSK-015](../docs/08-tasks-events.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十五、Task & Event：任务、事件和调度 › 任务和事件基础契约，第 1577–1577 行。

> - EVT-007｜定时与事件触发。 定时任务必须记录调度定义、用户时区、夏令时、错过触发的跳过/补跑策略、并发上限和有效授权；每个逻辑触发生成稳定触发 ID 并经统一 Broker 入队。重启补跑、重复事件和修改调度不得产生未经识别的重复副作用；明确授权过期、队列满和前次仍运行时的处理。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## EXT（10 项）

### EXT-001

**迁移：** `retained_context` / `Mapped`。保留原EXT-001对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1684–1684 行。

> | EXT-001 虚拟机 | Workload Runtime Driver、Device、Volume | VM 身份、虚拟固件/磁盘生命周期、直通隔离、暂停与快照保证；与容器共享调度和授权模型。 | 单机应用、设备与恢复已稳定，另完成虚拟机后端 ADR。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-002

**迁移：** `retained_context` / `Mapped`。保留原EXT-002对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1685–1685 行。

> | EXT-002 跨设备协作 | Session、Asset、Event、授权服务 | 同步冲突、离线修改、设备配对与撤销；会话迁移不等同进程检查点迁移。 | 设备信任、数据版本和传输授权验证完成。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-003

**迁移：** `retained_context` / `Mapped`。保留原EXT-003对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1686–1686 行。

> | EXT-003 弹性和跨架构调度 | Placement、Runtime Driver、制品索引 | CPU 架构/指令集/驱动兼容、镜像是否存在、伸缩冷却、状态落点及成本上限。 | M4 基础任务调度可靠，存在资源测量和可恢复的伸缩流程。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-004

**迁移：** `retained_context` / `Mapped`。保留原EXT-004对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1687–1687 行。

> | EXT-004 开发环境 | Application、Workspace Volume、SDK | 源码、构建产物、开发凭据和生产凭据隔离；调试端口授权；远程开发会话撤销。 | M2 容器/卷/网络和发布的 SDK 最小集完成。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-005

**迁移：** `retained_context` / `Mapped`。保留原EXT-005对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1688–1688 行。

> | EXT-005 声明式运维 | 统一资源 spec/status 与 Operation | 手动接管、差异预览、冲突和修复权限；自动修复不能将数据损坏扩大。 | API-013 至 API-016 验证完成。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-006

**迁移：** `retained_context` / `Mapped`。保留原EXT-006对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1689–1689 行。

> | EXT-006 多终端客户端 | 同一 System API、通知与授权视图 | 移动端/远程端身份、丢失设备撤销、弱网和可访问性；不另造管理后端。 | 稳定 API 和远程会话策略完成。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-007

**迁移：** `retained_context` / `Mapped`。保留原EXT-007对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1690–1690 行。

> | EXT-007 集中设备策略 | Policy、Principal、节点角色 | 家庭/团队策略优先级、租户隔离、离线有效范围与本机恢复权；不默认强制企业账号。 | M4 节点身份与控制权转交验收完成。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-008

**迁移：** `retained_context` / `Mapped`。保留原EXT-008对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1691–1691 行。

> | EXT-008 AI 资源治理 | Model Gateway、任务预算和审计 | Token/请求/并发/费用的估计与结算误差、熔断、账单对账；不把估算当真实账单。 | M3 具备供应商用量采集、预算拒绝和重试记账。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-009

**迁移：** `retained_context` / `Mapped`。保留原EXT-009对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1692–1692 行。

> | EXT-009 传统应用兼容 | Flatpak、OCI/兼容沙箱、必要的 VM | 对需要宿主写入、驱动或内核模块的应用单独声明信任与支持等级，不自动放开 privileged。 | 对代表性应用形成兼容矩阵，明确哪些仍不支持。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### EXT-010

**迁移：** `retained_context` / `Mapped`。保留原EXT-010对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。

**去向：** [7. 高级能力进入条件](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.3 P2 能力纳入架构、按角色逐步实现，第 1693–1693 行。

> | EXT-010 扩展硬件 | Device Provider、镜像 Profile、测试矩阵 | ARM64、GPU/NPU、特殊外设的驱动和固件来源、更新恢复、设备隔离保证。 | 对该硬件构建、启动、升级恢复及关键功能均通过测试。 |

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## FND（7 项）

### FND-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-001](../docs/03-host-updates-network-devices.md)；[V1-REL-005](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-01：平台与镜像契约，第 996–1000 行。

> FND-01：平台与镜像契约 必须在 M0 的平台 ADR 中列出首批 CPU 架构、固件启动模式、虚拟化平台、存储控制器及必要驱动，区分“发布验收覆盖”“实验支持”“未支持”。x86-64、ARM64、UEFI、传统 BIOS 均不得仅因出现在讨论中就宣称支持。最低内存、磁盘、启动耗时和空闲资源预算须来自实测，并按发行版及启用角色公布。 每个镜像必须携带发行版、架构、源码提交、组件清单、镜像摘要、配置 Schema、系统状态 Schema、更新来源和恢复兼容范围。ISO、可写入磁盘的镜像、虚拟机镜像以及云镜像属于…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-002](../docs/03-host-updates-network-devices.md)；[V1-ARCH-001](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-02：启动依赖与故障隔离，第 1002–1006 行。

> FND-02：启动依赖与故障隔离 正常启动依次建立固件/启动器、内核与早期用户空间、必要根与状态卷、本机身份与特权代理、基础系统 API，然后才启动应用执行环境、Agent、可选数据服务及集群角色。网络、外部模型、应用数据库、集群仲裁和 WebUI 不得成为本机救援入口的启动依赖。远程根卷等特殊配置必须另行声明其依赖，不计入默认独立启动保证。 启动完成判据必须拆成“宿主可管理”“发行版基本能力可用”“可选工作负载健康”，不能因模型服务断网而判定整个 OS 启动失败。Argon 的桌面可用性单独检测，显示驱动故障时仍…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-003](../docs/03-host-updates-network-devices.md)；[V1-REC-002](../docs/05-recovery-consistency.md)；[V1-REC-008](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-03：持久状态分类，第 1008–1023 行。

> FND-03：持久状态分类 以下为逻辑布局，不预先冻结物理分区、文件系统或具体路径： | 状态域 | 内容与责任主体 | 更新和回滚规则 | |---|---|---| | BootState | 启动项、尝试次数、已知可启动标记 | 更新器与启动器协调；保留可用回退入口 | | SystemDeployment | 内核、早期用户空间、宿主二进制、默认配置 | 以可校验部署单元安装；运行中原则上不可任意修改 | | LocalConfig | 主机网络、普通账户映射和节点配置 | 带版本与所有者；显式合并或迁移，…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-004](../docs/03-host-updates-network-devices.md)；[V1-REC-018](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-04：安装与重装，第 1025–1029 行。

> FND-04：安装与重装 安装流程必须提供设备探测、兼容性检查、目标磁盘与将受影响分区的展示、加密与恢复方式选择、账户初始化、镜像校验和首次启动验证。磁盘写入须针对稳定设备标识再次验证，禁止依赖容易变化的设备枚举顺序。自动安装使用相同规划与校验逻辑，配置中不得嵌入可长期使用的明文密钥。 “全新安装”“保留数据重装”“恢复已有系统”必须是不同操作。保留数据重装应当先检查卷标识、所有权、加密方式、数据 Schema 与旧配置兼容性；无法确认时停留于可诊断状态，不能把无法识别的数据盘当作空盘格式化。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-05

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-005](../docs/03-host-updates-network-devices.md)；[V1-UX-001](../docs/03-host-updates-network-devices.md)；[V1-SEC-016](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-05：初始化、配置与救援，第 1031–1035 行。

> FND-05：初始化、配置与救援 首次启动必须创建本机唯一身份和初始管理者，删除或使一次性初始化凭据失效，并避免镜像克隆产生重复节点密钥。配置变更应当保存来源、修订号、操作者与上一有效版本；同一配置字段只有一个写入责任主体，人工修改与声明式配置冲突必须显式报告。 必须提供可从启动菜单或外部介质进入的救援路径，至少能够检查磁盘与挂载、查看宿主日志、选择已知可启动部署、修复配置、恢复备份及重新建立本机管理访问。救援程序不得依赖正常容器引擎、Agent、云端认证或集群控制面。恢复入口不是无条件绕过磁盘加密和本机认证的后门…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-06

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-006](../docs/03-host-updates-network-devices.md)；[V1-API-017](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-06：Host Core 运行约束，第 1037–1039 行。

> FND-06：Host Core 运行约束 宿主应当保持精简并由声明式构建定义生成；调试性临时修改须可检测、可撤销，并与可发布镜像区分。时间同步、时区、名称解析、证书信任库、随机数初始化与本机 IPC 是基础服务的一部分。授权到期、更新元数据到期和租约应当分别处理不可信时钟、时钟回拨与重启，不得把某一远端服务不可达当作自动放宽安全检查的理由。进程存活期间的经过时间采用合适的单调时钟；跨重启有效性绑定持久授权代次、启动标识和可验证的时间条件，不能重置剩余秒数而延长授权。证书和绝对有效期使用可信实时时间及偏差策略；无法…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### FND-07

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-FND-007](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十六、OS Foundation：构建、安装、启动与持久状态 › FND-07：安全启动与加密恢复 Profile，第 1041–1043 行。

> FND-07：安全启动与加密恢复 Profile 必须声明每个发布 Profile 是否验证了 Secure Boot、模块/驱动签名及可选 TPM 解锁；签名系统镜像不自动等于整条启动链可信。可信根导入、替换和恢复由拥有者明确管理。若采用 TPM 绑定解锁，须验证正常升级、回退、固件变更和主板更换的恢复路径，并提供独立恢复材料；不能把 TPM 成为所有设备可用的前提，也不能承诺无密钥可解密。具体后端和默认开关通过 ADR 冻结。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## GOV（4 项）

### GOV-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GOV-006](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.4 模块质量与责任边界，第 1697–1697 行。

> GOV-001：必须给每个主线模块指定维护责任、输入输出契约、数据所有权、支持等级、升级策略与至少一个失败恢复用例。允许一个维护者承担多个模块，但不允许关键组件没有责任归属。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GOV-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GOV-007](../docs/13-operations-release-governance.md)；[V1-ARCH-005](../docs/01-product-and-architecture.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.4 模块质量与责任边界，第 1699–1699 行。

> GOV-002：必须在合并 XeLab 复用模块前登记实际 Forgejo 仓库、分支、完整提交 SHA、依赖清单、许可证、旧接口耦合点及修改记录。本次设计文档补全不代替该代码审计，也不从旧 GitHub 代码推断当前实现。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GOV-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GOV-001](../docs/00-reading-and-authority.md)；[V1-GOV-002](../docs/00-reading-and-authority.md)；[V1-GOV-005](../docs/00-reading-and-authority.md)；[V1-REL-005](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.4 模块质量与责任边界，第 1701–1701 行。

> GOV-003：必须让需求、ADR、实现变更和验收证据相互可追溯；不以生成文档、存在目录或测试框架已运行代替功能通过。高影响系统变更通过独立审查和所需故障试验后才进入可发布通道。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GOV-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GOV-008](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.4 模块质量与责任边界，第 1703–1703 行。

> GOV-004：建议从 M0 建立有界复杂度预算：每新增必装服务须说明启动依赖、常驻开销和失效影响。Helium 的精简目标由测量验证；不通过削减授权、审计和恢复不变量获得体积优势。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## GUI（15 项）

### GUI-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1290–1290 行。

> GUI-001（必须）：PsiOS Shell 声明三种应用入口：FullDesktop、SingleApplication、NativeWeb。FullDesktop 呈现经授权的整个 DesktopSession，SingleApplication 只呈现隔离应用或经 compositor 验证可捕获的指定窗口，NativeWeb 经应用自带 Web 前端提供入口。三者共享 Application 身份和权限，不等于每个应用都支持所有模式。失败时展示后端/权限限制，不悄然升级为全屏捕获。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1292–1292 行。

> GUI-002（必须）：复用 DesktopSession（权威会话）和 ControlLease（输入/观察授权范围），新增 ApplicationWindow（窗口/表面标识）、DisplayStream（可见画面和音频传输）及 ViewerSession（浏览器观看连接）作为子资源。后者均绑定会话 ID、安全域、应用/窗口 ID、用户、授权代次和生命周期；重启、窗口销毁或会话代次改变不得继续使用旧凭据。浏览器连接不拥有新的应用/桌面最高控制权。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)；[V1-APP-003](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1294–1294 行。

> GUI-003（必须）：GUI Bridge 至少包括会话编排、窗口/表面选择、采集、编码/传输、输入代理及授权门卫。工作负载原则上运行于隔离的 Wayland compositor 或经审查的 X11 兼容后端；默认不把宿主 Wayland/X11 Socket、KDE 会话总线、GPU 管理接口或用户 home 直接共享给不可信容器。特殊宿主会话接入走明确授权与后端支持路径。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-007](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1296–1296 行。

> GUI-004（建议）：WebRTC 作为低延迟音视频与输入通道候选；WebSocket、VNC/RDP 网关等可作为按能力限定的回退方案。连接能力协商必须显式报告视频/音频/输入/剪贴板/文件/硬件编码支持范围；无 WebRTC 或硬件加速不能伪称体验等价。Xpra、Selkies、Guacamole 等是可评估的实现组件，不在本稿冻结为唯一依赖。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-002](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1298–1298 行。

> GUI-005（必须）：单应用模式应优先通过应用专属虚拟会话、嵌套 compositor 或已验证的窗口级捕获实现。Wayland 并非天然允许未经授权读取任意窗口；对 X11 多窗口则需考虑窗口树、剪贴板与输入侧通道。只能可靠捕获整个桌面时，必须展示为 FullDesktop 并重新请求该范围授权，不能以窗口裁剪冒充安全隔离。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-002](../docs/07-applications-packages-gui.md)；[V1-GUI-003](../docs/07-applications-packages-gui.md)；[V1-GUI-007](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1300–1300 行。

> GUI-006（必须）：屏幕观察、键鼠输入、剪贴板读、剪贴板写、文件上传/下载、音频播放、麦克风、摄像头、设备穿透分开授权；仅将后端实际可强制的隔离范围呈现给用户。浏览器端粘贴、文件选择、Drag&Drop、IME、快捷键和音频同步按能力报告并在不支持时明确退化，不从单一“允许远程控制”按钮派生全部权限。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-003](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1302–1302 行。

> GUI-007（必须）：浏览器、KDE 本地用户、原生及第三方 Agent 共享 CU-004 的单一输入租约和人类接管优先规则；允许多个单独授权的只读 Viewer，但任一时刻指定会话最多一个远程输入控制者。观察者不能借由媒体通道写入输入事件；本地紧急停止、授权撤销和租约失效必须中止待发送输入。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-001](../docs/07-applications-packages-gui.md)；[V1-GUI-002](../docs/07-applications-packages-gui.md)；[V1-GUI-009](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1304–1304 行。

> GUI-008（必须）：会话以用户/租户/应用安全域隔离；浏览器身份与 PsiOS Principal 显式关联，短期连接凭证绑定 ViewerSession、来源/目标、授权代次和用途。禁止把原用户 D-Bus、浏览器 Cookie、密钥、剪贴板和主目录自动带入另一容器；跨用户查看、共享屏幕或 Agent 观察需要独立授权和审计。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-004](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1306–1306 行。

> GUI-009（必须）：应用进程、DesktopSession 和 ViewerSession 生命周期相互区分。浏览器标签页关闭、短暂网络中断或 Viewer 重连不应默认终止运行中的应用；是否让会话持续、允许谁重连、无客户端时资源是否冻结/超时回收由策略明确声明。重新连接必须重新校验主体、权限、租约与会话代次；不会因为旧 URL 或旧流标识恢复操控权限。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-010

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-GUI-005](../docs/07-applications-packages-gui.md)；[V1-GUI-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1308–1308 行。

> GUI-010（必须）：Argon 使用已有 KDE Plasma/Wayland 会话的捕获与输入时，应按实际 Portal/Compositor 支持提供用户可见授权和停止入口；不能偷偷读取物理会话或将窗口捕获升级成整屏。Neon 和 Helium 的无头 GUI 无需完整 Plasma，但必须安装经过验证的 compositor/渲染/输入和应用依赖组件；后端未安装时不得阻碍 Shell 的其他管理能力。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-011

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-GUI-004](../docs/07-applications-packages-gui.md)；[V1-GUI-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1310–1310 行。

> GUI-011（必须）：至少定义显示尺寸/DPI、方向/缩放、编码格式、输入坐标变换、窗口焦点和键盘布局的协商与重新同步。窗口移动、分辨率变化或断线恢复后，旧截图坐标不得无条件重放；Agent 应优先使用 AT-SPI、应用语义 API 等能获得目标和角色的接口，并在授权失效时停止而非改走视觉绕过。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-007](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1312–1312 行。

> GUI-012（必须）：GUI Bridge 的 CPU、内存、显存、编码器会话数、码率、并发 Viewer 和流量预算均可限制，不能影响 Host Core 的登录、诊断或恢复。GPU 渲染/硬件编码为能力发现的可选加速项，软件渲染/编码仅在资源预算满足时回退；资源不足时拒绝新流或有界降帧，而不无限积压输入和帧缓存。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-013

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-007](../docs/07-applications-packages-gui.md)；[V1-GUI-009](../docs/07-applications-packages-gui.md)；[V1-SEC-014](../docs/04-security-identity.md)；[V1-SEC-015](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1314–1314 行。

> GUI-013（必须）：远程流使用认证、加密和安全的信令/媒体鉴权；WebSocket/HTTP 端点防重放、跨站请求及不受信来源控制，TURN/STUN 等部署若使用凭证则必须短期有效、限定用途。操作事件与必要元数据按审计政策记录，截图/录屏/剪贴板/原始音频属敏感内容，不得默认永久保存或上传给远程模型。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-014

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-008](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1316–1316 行。

> GUI-014（必须）：NativeWeb 入口优先以受身份校验的 URL/反向代理提供，只有在应用的 CSP、frame-ancestors、Cookie/会话策略允许且安全模型相容时才嵌入 iframe。不能以绕过目标网站安全策略为前提保证任意 Web 应用均可嵌入；SSO/反向代理必须有明确用户、域名、路径与权限边界。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### GUI-015

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-GUI-005](../docs/07-applications-packages-gui.md)；[V1-GUI-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十一、Computer Use：桌面会话与用户接管 › GUI Bridge：WebUI 图形应用与会话协议（v0.4），第 1318–1320 行。

> GUI-015（必须）：将图形能力向第三方 Agent 暴露时复用系统级 DesktopSession、ControlLease 和审计；语义接口优先，视觉回退只在同授权范围工作。Agent 不得自动批准自身的 ScreenCast/RemoteDesktop 许可弹窗。跨节点 GUI 访问通过 Cluster/Identity 已授权代理路由，不借远程连接获得宿主集群管理权。 实现阶段与验收： M3 定义资源与授权 Schema；M4 对 Argon 的 KDE 授权桌面、Neon 的至少一个无头 GUI 和一个…（完整原文见JSON与归档）

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## NET（4 项）

### NET-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-NET-001](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › NET-01：网络责任边界，第 1325–1327 行。

> NET-01：网络责任边界 Network Service 负责期望配置、状态读取、权限与事务；宿主网络后端负责实际执行。每个接口、路由、DNS 区域和防火墙规则集合必须有唯一配置权威。KDE 网络界面、CLI、WebUI、Agent 与集群控制器共用该权威或受控适配，不得各自反复覆盖配置。NetworkManager 与其他 Linux 网络组件保留为实现候选。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### NET-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-NET-001](../docs/03-host-updates-network-devices.md)；[V1-NET-003](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › NET-02：网络资源与功能面，第 1329–1331 行。

> NET-02：网络资源与功能面 统一描述物理/虚拟接口、连接配置、地址、路由、DNS、代理、网络域、端口发布和访问策略；明确“未配置”“离线”“受限”“已连接”状态。基础能力覆盖已声明支持平台的有线网络、IPv4/IPv6、静态配置和动态获取；无线、多网卡、桥接、VLAN、VPN、复杂路由按角色与后端能力声明。不能以仅支持 IPv4 的规则宣称已实施完整隔离。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### NET-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-NET-002](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › NET-03：安全变更与远程管理，第 1333–1335 行。

> NET-03：安全变更与远程管理 修改管理接口、路由、DNS或防火墙必须先保存可恢复检查点，试用新配置，要求在目标配置代数下新建立的管理连接及必要路由/DNS路径完成确认；旧长连接仍可用不足以证明新登录正常。未确认则回退。变更须串行化或使用配置栅栏，旧检查点不得覆盖已提交的新配置；明确授权的主动断网/关闭接口目标使用对应的本机确认策略。超时和确认方由策略设置，不能无限等待。必须有本机恢复入口；连通性探测不得只依赖公共互联网地址。后端能力不足时需明确返回不支持自动恢复，禁止无提示假装事务成功。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### NET-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-NET-003](../docs/03-host-updates-network-devices.md)；[V1-NET-004](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十二、Network & Device：网络、资源、设备和电源 › NET-04：工作负载与集群网络，第 1337–1341 行。

> NET-04：工作负载与集群网络 每个应用必须声明网络模式、对外端口和外连策略，默认不得隐式暴露管理端口或取得宿主网络命名空间。服务发现返回可授权的端点，不以“发现服务”替代身份验证。节点通信、应用通信与用户远程管理分别配置身份与访问策略；跨子网、NAT和代理场景通过经声明支持的连接方式实现，不默认所有节点互相可达。 网络访问限制必须作用于真实执行路径，涵盖 DNS解析后地址、IPv6、重定向以及代理出口。容器网络驱动或策略执行器不可用时，不得静默退回无隔离宿主网络。离线时本机网络配置、日志和应用管理仍可使用；云模…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## OBS（6 项）

### OBS-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OBS-001](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1595–1595 行。

> - OBS-001｜最小诊断常驻宿主。 本地日志、服务状态、磁盘健康、启动失败原因和基础诊断 CLI 必须不依赖 Agent、模型、Dataset、集中遥测后端或容器引擎。Shell 不可用时仍可查询和导出诊断信息；远程告警/长周期存储为可选能力。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OBS-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OBS-002](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1597–1597 行。

> - OBS-002｜关联但分工。 系统应区分诊断日志、指标、执行追踪和安全审计。以 node_id、boot_id、主体、资源、任务/步骤 ID 关联；可评估 OpenTelemetry 作为日志/指标/追踪的互操作方案，其日志模型支持与 trace/span 上下文关联。D4 采用该方案不替代授权审计，也不要求所有版本安装大型 Collector 或监控数据库。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OBS-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OBS-003](../docs/13-operations-release-governance.md)；[V1-SEC-020](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1599–1599 行。

> - OBS-003｜审计可靠性。 特权或策略要求审计的操作须先保证审计意图可持久化，再执行；完成后记录结果、策略版本和错误。审计存储耗尽或失效时拒绝新增需强制审计的特权操作，同时保留只读诊断和受控救援通道。审计失败、丢弃和紧急操作必须可见；仅靠本机追加日志不能声称可抵御宿主管理员篡改，更强保护需可选独立见证或受控远端存储。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OBS-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OBS-004](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1601–1601 行。

> - OBS-004｜边界与隐私。 日志、追踪、截图、Agent 原始输入输出分别设置采集开关、访问权限、容量与保留期。默认日志不采集完整密钥、会话凭据和不必要的用户正文；诊断包应提供清单与脱敏。遥测出口必须显式配置，禁用后不得后台发送。高频任务 ID 不应直接成为无限增长的指标标签。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OBS-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OBS-002](../docs/13-operations-release-governance.md)；[V1-OBS-005](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1603–1603 行。

> - OBS-005｜健康与告警。 至少区分存活、就绪、退化、不可服务；进程仍存在不等于业务健康。告警覆盖存储耗尽、备份超期、证书临期、队列积压、重复失败、GPU/OOM、时钟异常和审计丢失，并支持去重、抑制与恢复通知。每个严重告警应链接确定性排障步骤，Agent 可以解释和辅助处理，但不能成为唯一修复入口。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OBS-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-004](../docs/13-operations-release-governance.md)；[V1-ARCH-003](../docs/01-product-and-architecture.md)；[V1-REL-005](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十六、Observability：观测、审计与故障诊断，第 1605–1605 行。

> - OBS-006｜资源预算。 观测链路必须有独立 CPU/内存/磁盘预算，过载时优先降低可采样诊断信息，不侵占关键状态空间。指标/日志导出失败不得阻塞普通工作负载；安全审计仍遵守 OBS-003。发布必须记录基线启动时间、空闲内存、常驻进程、存储占用与压力表现，测量结果再确定各发行版资源门槛。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## OPS（5 项）

### OPS-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-001](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.2 配置与运维，第 1668–1668 行。

> OPS-001：必须给配置定义优先级与所有权：发行版默认值、管理员策略、节点配置、用户配置、任务临时覆盖。安全限制按可委托范围逐层收紧，低层优先级项不能通过覆盖扩大权限；最终生效配置及来源应可查询。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OPS-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-002](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.2 配置与运维，第 1670–1670 行。

> OPS-002：必须把配置 Schema 迁移、导出/导入、校验、预演和恢复纳入系统更新计划。配置记录与修订的提交必须原子完成；跨网络、设备和服务的实际应用由 Operation 协调，保存逐项结果与补偿，允许明确的 PartiallyApplied 条件。只有观察到必要资源达到目标修订后才标记 Applied，不承诺通用跨系统原子事务。敏感值不直接出现在普通配置导出中，导入后对失效 Secret 引用显示可修复状态。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OPS-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-003](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.2 配置与运维，第 1672–1672 行。

> OPS-003：必须提供 maintenance/drain 的一致语义：停止接收新任务、处理正在运行的有状态应用、保留操作日志、执行更新/迁移、验证后恢复调度。强制中止和正常排空必须分别记录。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OPS-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-004](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.2 配置与运维，第 1674–1674 行。

> OPS-004：必须预留运行预算与压力策略，包括 CPU、内存、IO、临时磁盘、事件与审计空间、网络带宽和模型费用。每项预算有范围、硬限/软限、超限行为和可查询用量；数值由目标平台测量后冻结，不继承旧 XeLab 虚拟机容量配置。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### OPS-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-OPS-005](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.2 配置与运维，第 1676–1676 行。

> OPS-005：必须制定资产退役流程：停接新任务、数据导出或迁移、备份确认、Secret 和节点证书撤销、集群成员退出、设备与存储释放、可执行的擦除或保留策略。设备不可达时标记撤销与数据清理的实际完成范围。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## PKG（10 项）

### PKG-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-008](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1187–1187 行。

> PKG-001（必须）：在系统 API 和应用清单中区分 SourcePackageFormat、TargetDistribution/发行版版本、CPUArchitecture、UserspaceABI（glibc/musl 等）、RuntimeDriver 与 ApplicationEntry。初期目标明确为 Linux DEB、RPM、Alpine APK 三类；文件扩展名不等于可信格式或可执行保证。应用必须报告真实导入和运行状态，不以宿主发行版包管理器混装实现“兼容”。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-009](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1189–1189 行。

> PKG-002（必须）：PackageImport 包括 Detected → Resolved → Verified → EnvironmentPrepared → Integrated / Rejected，导入器必须读取元数据、脚本、依赖、目标架构、ABI、架构特定依赖、共享库、运行服务和 GUI 要求。多候选目标发行版时必须显式选择或要求用户确认；无可验证的目标环境应返回不支持，不能随意用另一发行版 rootfs 强行安装。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-009](../docs/07-applications-packages-gui.md)；[V1-APP-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1191–1191 行。

> PKG-003（必须）：每个 PackageEnvironment 对应可追溯、隔离且可重建的发行版用户空间；可按兼容策略复用只读基础层，但应用运行身份、包数据库写层、权限和持久数据隔离。apt/dpkg、dnf/rpm、Alpine apk 仅在相应隔离环境执行；不得修改宿主包数据库、宿主系统库或 Host Core。内核/设备依赖仍由宿主能力矩阵判定，不承诺容器拥有独立 Linux 内核。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1193–1193 行。

> PKG-004（必须）：导入时将原包摘要、上游仓库/发行版快照、依赖版本与来源、目标基础镜像摘要、构建/安装日志和最终 OCI 制品摘要绑定在同一 Application 来源记录。签名/哈希证明来源或完整性，不等于二进制安全；离线导入与第三方仓库需显示信任级别和更新来源。不得静默漂移到未固定的基础镜像。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-009](../docs/07-applications-packages-gui.md)；[V1-APP-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1195–1195 行。

> PKG-005（必须）：把 preinst/postinst、RPM scriptlets、Alpine 安装脚本、触发器、系统服务声明和自动化钩子视为不可信可执行内容；默认仅在限制权限的构建/安装环境执行。可能写宿主设备、创建宿主服务、设置全局权限或使用系统 D-Bus 的操作必须由显式特权代理校验；未获授权即拒绝或标记为兼容限制，不因“为了安装成功”绕过 SEC/APP。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-001](../docs/07-applications-packages-gui.md)；[V1-APP-006](../docs/07-applications-packages-gui.md)；[V1-APP-007](../docs/07-applications-packages-gui.md)；[V1-APP-008](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1197–1197 行。

> PKG-006（必须）：包导入结果转换为 APP-001 统一 Application/Deployment 管理，不再生成第二套权威应用对象。启动/停止/更新/卸载、卷、资源限制、审计、故障和操作状态均复用 APP-005～008。包版本升级、OCI 基础层升级、数据 Schema 迁移分别登记与回退；不能用宿主级包管理器偷偷替换容器依赖。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-008](../docs/07-applications-packages-gui.md)；[V1-APP-011](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1199–1199 行。

> PKG-007（必须）：GUI 程序需要实际验证 .desktop 启动入口、Qt/GTK/Electron 运行库、D-Bus/Portal、字体、输入法、通知、GPU、音频和会话生命周期。需 systemd、宿主服务、特定内核模块或特权设备的程序可以有单独受控 Profile；不能默认承诺所有 DEB/RPM/APK 程序均可运行。图形会话由 GUI Bridge/CU 统一授权。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-008](../docs/07-applications-packages-gui.md)；[V1-APP-011](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1201–1201 行。

> PKG-008（必须）：声明细化兼容状态：Recognized（格式及元数据可读取）、Imported（隔离安装/依赖解析完成）、RuntimeVerified（指定功能用例运行通过）、GUIVerified（指定图形入口与交互通过）。这是逐包/版本/架构/环境的测试结果，不能从格式支持自动推导运行成功或 GUI 可用。对于服务和 CLI 程序，GUIVerified 不适用。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-007](../docs/07-applications-packages-gui.md)；[V1-APP-010](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1203–1203 行。

> PKG-009（必须）：安装、升级和恢复对仓库不可达、依赖冲突、脚本超时、磁盘满、恶意来源、跨架构执行和不支持的内核能力给出可区分错误。保留已安装可运行的旧 Deployment 和用户数据；即使依赖安装中止也不得遗留宿主权限或破坏其他 PackageEnvironment。共享基础镜像引用与镜像层 GC 需避免删除仍被使用的版本。

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### PKG-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-APP-008](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十九、Container & Application：应用清单与生命周期 › Universal Package Adapter：多发行版包兼容规范（v0.4），第 1205–1207 行。

> PKG-010（必须）：Android .apk 与 Alpine Linux .apk 分别由识别器归类；Android 应用不得交给 Alpine 包管理器，也不得宣称 Linux OCI rootfs 可直接运行 Android 包。Android Runtime（如经验证的兼容容器/虚拟环境）为可选适配器，涉及 Android Framework、图形、输入、权限、CPU 架构及硬件能力时必须另立兼容矩阵和验收，不纳入初始 Linux 三包格式的完成标准。 实现阶段与验收： M2 构建容器和 Applica…（完整原文见JSON与归档）

**溯源范围：** 4 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## REL（5 项）

### REL-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-REL-001](../docs/13-operations-release-governance.md)；[V1-REL-002](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十七、Release & Compatibility：发行与开发者体系 › REL-01：统一构建输入与产物清单，第 1619–1623 行。

> REL-01：统一构建输入与产物清单 三个发行版必须来自统一源码、共享 Core 版本和显式发行配置；依赖以版本与摘要锁定，禁止发布构建悄然拉取浮动标签。每个制品记录源码提交、构建器、构建参数、依赖、许可证信息、SBOM、摘要及签名。来源证明须校验受信构建者、允许的源码仓库/提交与实际制品摘要的绑定，未验证的标签或提交字符串不是来源证据。可参考 SLSA 的可验证制品来源模型，但在达到对应要求之前不得宣称取得某个安全等级。F6 可复现构建作为目标：必须记录可重建输入并执行独立重建比较；如果不能逐字节一致，要列明非确…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### REL-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-REL-002](../docs/13-operations-release-governance.md)；[V1-REL-003](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十七、Release & Compatibility：发行与开发者体系 › REL-02：发布控制与密钥，第 1625–1629 行。

> REL-02：发布控制与密钥 构建、测试、批准、签名、分发应使用职责分离的流水线和最小权限凭据。发布必须绑定具体源码及制品摘要，检查失败不得通过重新生成同名制品绕过。签名密钥轮换、撤销、丢失或泄露必须有演练过的应急流程；可信根更新独立于普通应用更新。TUF 是更新元数据保护与密钥委托的候选参考，不等于已选实现。F4 应用仓库、Skill仓库和模型制品来源分别记录发布者、许可证、平台兼容、权限增量和撤销状态。签名有效仅证明来源及完整性，不能代替代码审查、权限判断或兼容测试。宿主二进制、固件与闭源驱动的再分发条件须在发…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### REL-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-008](../docs/02-common-contracts.md)；[V1-REL-006](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十七、Release & Compatibility：发行与开发者体系 › REL-03：版本与兼容政策，第 1631–1635 行。

> REL-03：版本与兼容政策 OS、API、资源 Schema、应用清单、数据 Schema、节点协议、SDK 和制品版本必须分别标识。破坏性 API/Schema 变更使用显式版本或迁移，不能在同名字段下改变语义。兼容窗口、废弃通知、支持周期、漏洞响应责任与发布通道规则须在 M6 前写成发布政策；未确定的时长不得提前承诺。 客户端必须先发现服务和能力，再调用可选功能；不支持应返回可区分的错误。集群滚动更新与跨版本客户端需要明确兼容矩阵，超出范围时拒绝加入或修改，不能依赖“版本接近应该可用”。C/C++ 二进制插件…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### REL-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-DEVX-001](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十七、Release & Compatibility：发行与开发者体系 › REL-04：开发者交付与应用生态，第 1637–1641 行。

> REL-04：开发者交付与应用生态 开发者套件必须包含可生成客户端的 Schema、类型、认证与取消约定、分页/事件订阅、幂等与错误语义、最小应用/服务/Agent/Skill模板、沙箱开发环境和兼容性检查器。CLI、Shell 和 Agent 应复用同一服务契约，SDK 不另行藏入特权逻辑。 发布者应当能在隔离环境验证权限清单、安装升级、迁移、卸载及数据保留；本地开发源与受信发布仓库要有清晰标识。安装同一依赖的不同版本必须通过运行时隔离或显式冲突处理，不能擅自替换宿主依赖。传统 Linux 软件、需要额外系统权限…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### REL-05

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-REL-004](../docs/13-operations-release-governance.md)；[V1-REL-005](../docs/13-operations-release-governance.md)；[V1-REL-006](../docs/13-operations-release-governance.md)；[V1-OBS-004](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十七、Release & Compatibility：发行与开发者体系 › REL-05：发布验收与维护，第 1643–1647 行。

> REL-05：发布验收与维护 CI 按实际风险覆盖 Schema/契约、集成、镜像启动、安装、更新断电、数据恢复、权限撤销、桌面与已声明硬件平台。测试结果须绑定制品摘要；真实设备未测与虚拟机已测不可互相代替。安全通告、已知限制、升级注意事项及退役迁移须随发布提供。 故障监测可以启用本地统计及用户授权的远程报告；崩溃资料脱敏、可查看、可删除。正式发布不应依赖默认上传用户提示词、文件内容或桌面截图。回收错误版本时仍需保留用户数据与可用恢复路径。

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## SEC（11 项）

### SEC-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-002](../docs/04-security-identity.md)；[V1-SEC-003](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 安全主体与授权对象，第 1098–1098 行。

> SEC-001（必须）：统一 Principal 至少区分人类用户、应用服务、Agent、节点及外部服务。每个主体具有不可随显示名称变化的稳定 ID、所属安全域、状态、认证方式和凭据版本。Linux UID/GID 是本机执行映射，不可作为跨节点全局身份；容器内 UID 0 也不等于宿主管理员。身份删除、重新创建及设备重装不得复用旧权限映射。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-003](../docs/04-security-identity.md)；[V1-SEC-014](../docs/04-security-identity.md)；[V1-SEC-016](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 安全主体与授权对象，第 1100–1100 行。

> SEC-002（必须）：本机用户认证与 WebUI/远程认证映射到同一授权模型，但不得共享未区分受众的会话令牌。管理员、普通用户、应用服务和 Agent 分别拥有会话；长期服务使用独立服务身份。OIDC 等外部身份提供者属于可选接入，身份服务或互联网失联时，已有本机账户仍可按本地策略登录及恢复系统。首次部署必须建立可独立于 Agent 使用的管理入口。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-003

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-SEC-002](../docs/04-security-identity.md)；[V1-SEC-004](../docs/04-security-identity.md)；[V1-SEC-005](../docs/04-security-identity.md)；[V1-SEC-007](../docs/04-security-identity.md)；[V1-SEC-008](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 安全主体与授权对象，第 1102–1112 行。

> SEC-003（必须）：授权对象至少包含以下语义，字段名由正式 Schema 固定： | 对象 | 最低信息与约束 | |---|---| | Capability | 操作名称、资源类型、参数约束、支持的审计与撤销方式 | | Grant | 授权者、执行主体、资源选择器、动作、有效期、节点/网络约束、是否允许再委托、撤销代次 | | Delegation | 原始用户、父授权、Agent、任务、可进一步缩减的范围和预算 | | Approval | 审批者、待执行操作摘要、参数摘要、效果相关配置/内容版本与依赖…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-004

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-SEC-005](../docs/04-security-identity.md)；[V1-SEC-006](../docs/04-security-identity.md)；[V1-SEC-007](../docs/04-security-identity.md)；[V1-SEC-009](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 安全主体与授权对象，第 1114–1114 行。

> SEC-004（必须）：用户可设置一次操作、单次任务、会话或有效期内持续授权。已有授权覆盖的动作直接执行；仅权限不足、审批条件触发或操作范围发生实质变化时请求新增授权。审批不可简化为对任意后续命令有效的“允许 Agent”；参数、目标效果相关版本或制品摘要改变后，旧的精确审批失效；无关 status/心跳更新不使审批失效。删除系统数据、修改信任根等高影响动作可由策略要求独立审批，具体默认策略在 M0 记录，不要求普通查询和可逆操作反复确认。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-001](../docs/04-security-identity.md)；[V1-SEC-010](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 宿主与工作负载边界，第 1118–1127 行。

> SEC-005（必须）：按职责划分执行信任，不按“是否装在容器里”推定权限。 | 区域 | 责任 | 允许的宿主访问 | |---|---|---| | 最小宿主 | 启动、设备、根存储、更新恢复、账户及特权代理 | 完成自身职责所需的明确系统权限 | | 受信系统服务 | 管理存储逻辑、应用、Agent、任务等 | 通过受限宿主 API；独立身份及最小能力集合 | | 普通应用与 Agent | 应用逻辑、工具执行、数据处理 | 显式授予的卷、设备、网络及系统能力 | | 第三方/不受信代码 | 外部插件、临时代…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、13 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-010](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 宿主与工作负载边界，第 1129–1129 行。

> SEC-006（必须）：Host Service Broker 暴露类型化的操作，如创建受限卷、改变已授权网络配置、查询设备；每次调用携带主体、委托链、任务/操作 ID 和授权证据。Broker 校验参数、效果相关版本/依赖指纹、权限及前置状态，记录结果。普通系统 Capability 不得退化为任意 root shell。确需宿主调试的管理员入口是单独权限，不可由普通 Agent 的“工具执行”隐式获得。

**溯源范围：** 6 处定义、13 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-003](../docs/04-security-identity.md)；[V1-SEC-011](../docs/04-security-identity.md)；[V1-API-017](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 宿主与工作负载边界，第 1131–1131 行。

> SEC-007（必须）：本地服务认证使用可验证进程/通道身份；远程服务使用验证服务端且能够认证调用者的加密通道。不得相信请求体自行填写的 UID、Agent ID、节点名或来源 IP 即完成认证。文件操作必须防止路径穿越、符号链接替换及授权检查与实际打开对象之间的竞态；权限绑定稳定资源或安全打开的句柄，不能只比较路径字符串前缀。

**溯源范围：** 6 处定义、13 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-008

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-SEC-017](../docs/04-security-identity.md)；[V1-SEC-018](../docs/04-security-identity.md)；[V1-SEC-019](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 撤销、凭据与审计，第 1135–1135 行。

> SEC-008（必须）：撤销是端到端操作。系统必须停止新调用、使后续续租失败，并对已有桌面流、设备句柄、挂载卷和网络通路执行相应隔离动作；必要时终止或重建沙箱。不可声称只撤销 API Token 就收回已经授予的直接文件访问。撤销响应上限、离线授权最大有效期及失联处理必须在对应实现 ADR 中给出可测试值。无有效授权或无法验证高影响写操作时拒绝执行，保留本机恢复通道。

**溯源范围：** 6 处定义、13 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-003](../docs/04-security-identity.md)；[V1-SEC-013](../docs/04-security-identity.md)；[V1-SEC-016](../docs/04-security-identity.md)；[V1-SEC-021](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 撤销、凭据与审计，第 1137–1137 行。

> SEC-009（必须）：Secret 由受限凭据服务保管，模型只看到引用和使用结果。优先以代理代调用或短期凭据提供能力；第三方程序只能通过其需要的受限环境、文件描述符或密钥接口接收凭据，并阻止进入日志、崩溃转储和模型提示词。API 密钥、节点证书、签名密钥分别定义导入、轮换、撤销、备份和失效后的恢复流程。节点克隆及重新入群必须重新建立独立身份，不能复制仍有效的节点私钥。

**溯源范围：** 6 处定义、13 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-020](../docs/04-security-identity.md)；[V1-SEC-021](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 撤销、凭据与审计，第 1139–1139 行。

> SEC-010（必须）：审计至少记录责任主体链、授权/策略版本、操作及参数摘要、资源、结果、时间、追踪 ID 和恢复关联。审计与原始聊天、截图和秘密分离；默认不记录密码或完整敏感输入。高影响写操作在无法持久化必要审计时停止或进入明确的应急管理模式；应急操作仍须保留独立可恢复记录。不得宣称普通本地日志能够抵抗已经控制宿主 root 的攻击者；远程防篡改审计属于可选加强能力。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### SEC-011

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-001](../docs/04-security-identity.md)；[V1-SEC-005](../docs/04-security-identity.md)；[V1-SEC-010](../docs/04-security-identity.md)；[V1-SEC-012](../docs/04-security-identity.md)；[V1-SEC-015](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十八、Security & Identity：安全、身份与宿主能力 › 撤销、凭据与审计，第 1141–1141 行。

> SEC-011（必须）：网络页面、仓库文档、Skill 文本、工具输出和第三方 Agent 返回值作为带来源的数据处理，不赋予修改系统策略或审批的权力。执行器通过结构化工具与策略执行权限边界。安全模型不得依靠模型“识别所有提示注入”，也不得承诺应用已经读取的数据可以在撤权后从其记忆中收回；控制目标是限制新增访问、外传通道和执行副作用。

**溯源范围：** 6 处定义、7 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## STO（10 项）

### STO-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-002](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 资源对象、所有权与能力发现，第 1373–1373 行。

> - STO-001｜统一对象名称。 存储池的规范名称统一为 StoragePool；原稿资源表中的 Pool 是同一概念的旧简称，不形成第二种资源。每个 StoragePool、Volume、Snapshot、Backup 必须有稳定 ID、所有者、后端 ID、版本及生命周期状态；展示名称和挂载路径不能充当身份。引用关系必须可查询，仍被挂载、快照、恢复任务或备份链引用的对象不得静默删除。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-003](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 资源对象、所有权与能力发现，第 1375–1375 行。

> - STO-002｜诚实声明能力。 StorageDriver 必须提供能力发现，包括快照的一致性范围、组快照、配额、扩缩容、加密、校验、复制、只读挂载、访问模式及故障域。缺失能力返回 UnsupportedCapability；不得用非原子文件复制冒充原子快照。驱动选择、默认文件系统和具体 RAID/卷管理方案继续通过 ADR 与技术验证确定。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-004](../docs/06-storage-data-tablefs.md)；[V1-FND-004](../docs/03-host-updates-network-devices.md)；[V1-SEC-010](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 资源对象、所有权与能力发现，第 1377–1377 行。

> - STO-003｜宿主执行边界。 格式化、分区、挂载、加密解锁和设备移除由宿主特权代理执行，Storage Service 只提交经过授权的结构化请求。授权必须绑定设备稳定标识与当前拓扑，执行前重新确认目标；不得仅凭易变化的 /dev/sdX 名称操作。容器不得因申请卷而获得任意块设备权限。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-004](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 资源对象、所有权与能力发现，第 1379–1379 行。

> - STO-004｜卷生命周期。 规范状态为 Provisioning → Available → Attached → Detaching → Available，并支持 Degraded、ReadOnly、Unavailable、Deleting、Deleted。卷申请使用幂等请求 ID，挂载绑定应用身份、读写模式、UID/GID 映射和配额。重启后须先确认真实卷身份再启动依赖应用；卷丢失时报告 StorageUnavailable，不能创建空目录冒充原卷使应用初始化出第二套数据。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-005](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 资源对象、所有权与能力发现，第 1381–1381 行。

> - STO-005｜空间与故障。 数据、镜像缓存、临时文件、日志和关键状态应分别核算并设置可配置边界。空间或 inode 耗尽时拒绝新写入/新分配并告警，保留基本查询与恢复入口；回收只能作用于标为可重建且未被引用的缓存。I/O 错误、只读降级、SMART/后端健康告警、校验失败必须可见，不能自动把“驱动可访问”视为“数据可靠”。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-006](../docs/06-storage-data-tablefs.md)；[V1-STO-007](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 一致性、备份与恢复契约，第 1385–1396 行。

> STO-006｜区分快照保证与范围。 Snapshot 必须分别记录一致性保证、覆盖范围、参与资源版本、生成号、时间与协调证据： | 维度 | 必须表达的语义 | 不可作出的推断 | |---|---|---| | consistency=crash-consistent | 声明范围内相当于突然断电后的磁盘状态 | 不能认为所有应用事务已完成。 | | consistency=application-consistent | 所列应用按已验证协议完成刷新、写屏障或检查点 | 不代表未参与协调的应用也一致。 | |…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-006](../docs/06-storage-data-tablefs.md)；[V1-STO-008](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 一致性、备份与恢复契约，第 1398–1398 行。

> STO-007｜备份作为独立恢复集。 每个 Backup 必须记录：原资源 ID、应用与 Schema 版本、卷/对象清单、内容校验值、依赖的基础备份和增量链、必要事务日志、加密密钥引用、备份目标及故障域、恢复步骤。位于同一故障域的副本必须明确标记其保护范围；本地快照不能冒充可应对整机损毁的备份。数据与元数据不一定同一事务提交时，必须提供写屏障、版本清单或应用专用导出协议，将两者绑定到同一可恢复版本。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-008](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 一致性、备份与恢复契约，第 1400–1400 行。

> STO-008｜恢复流程。 采用 Planned → Capturing → Transferring → Stored → IntegrityChecked 的备份流程；实际恢复试验成功后单独记录 restore_verified_at、环境、应用启动检查与业务一致性结果，不能把校验和通过等同于可恢复。恢复默认采用 Preflight → RestoreToTarget → Validate → Activate，失败停在新目标并保留旧目标。空间不足等情况下的原位恢复须有独立计划、核实过的恢复集及覆盖授权，并明确…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-009](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 一致性、备份与恢复契约，第 1402–1402 行。

> STO-009｜RPO/RTO 与保留。 对宿主配置、任务状态、用户卷及可选数据服务分别登记恢复策略。RPO 指故障发生时相对最新可恢复点的最大允许数据损失时间；RTO 的起止点须在策略中固定并通过恢复演练测量。未做演练时只能标为目标值，不能声称已达标。策略必须说明容量上限、保留版本、依赖链保护、最后一个已验证恢复集的保护、备份失败告警与超限处理；不得为释放空间静默破坏仍在使用的增量链。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### STO-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-009](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › 一致性、备份与恢复契约，第 1404–1404 行。

> STO-010｜加密与密钥。 卷密钥、备份密钥、凭据和节点证书分开管理，密钥状态至少包括创建、活动、轮换、退役、撤销及销毁。轮换期间要明确旧备份的解密路径；恢复材料必须能够在源系统盘丢失后获得。密钥只通过受控引用传递，不写入日志、清单明文或模型上下文。密钥销毁前检查仍需保留的数据引用；不能把销毁在线密钥宣传成所有历史副本均已删除。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## TFS（16 项）

### TFS-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-001](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1449–1449 行。

> - TFS-001｜范围与可选性。 TableFS 是 StorageDriver/Volume 的可选后端，允许独立安装、卸载和迁移；宿主根分区、最小救援和基础卷不依赖 TableFS。卷未启用/服务未装时准确返回不支持或不可用状态，不将普通卷误识别为 TableFS 卷。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-002](../docs/06-storage-data-tablefs.md)；[V1-STO-002](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1451–1451 行。

> - TFS-002｜稳定身份与路径。 (volume_id, inode_id) 在卷生命周期内稳定，目录项使用 (parent_inode_id, name) 指向 inode；路径是解析结果而不是数据主键。文件 rename、移动、硬链接、删除路径与保留打开句柄分别遵守所声明的文件语义；目录移动不得强制改写所有子文件身份。跨卷移动不承诺原子。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-001](../docs/06-storage-data-tablefs.md)；[V1-TFS-002](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1453–1453 行。

> - TFS-003｜单一权威命名空间。 文件目录操作和目录表查询必须共用同一权威元数据、事务日志及提交序列，不能分别维护两套可独立修改的路径数据库。目录项冲突、循环移动、硬链接计数、符号链接和路径穿越必须在执行层正确处理；引用的 inode 和内容清单应可审计。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-003](../docs/06-storage-data-tablefs.md)；[V1-STO-006](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1455–1455 行。

> - TFS-004｜内容存放与原子发布。 大正文采用支持流式读取与随机访问的内容引用/分块或对象存储；允许小对象优化但阈值经测试决定。先完成内容持久化（或等效 WAL 保障），再提交可见内容版本及元数据引用；崩溃后不得出现“目录项已提交但内容永久缺失”的静默成功。后台回收未发布分块与被引用版本分开处理。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-003](../docs/06-storage-data-tablefs.md)；[V1-STO-006](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1457–1457 行。

> - TFS-005｜事务与持久化语义。 对同卷原子重命名、文件内容提交、元数据提交、fsync/fdatasync/目录持久化、断电恢复分别定义可测试保证；数据可见性不等于落盘持久性。文件操作与表 API 写入共享并发前提和提交序列；多文件/多卷/跨应用的原子性必须通过显式事务/快照能力协商，绝不能默许。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-006

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-TFS-004](../docs/06-storage-data-tablefs.md)；[V1-TFS-005](../docs/06-storage-data-tablefs.md)；[V1-TFS-009](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1459–1459 行。

> - TFS-006｜POSIX 能力矩阵。 用户态挂载（例如 FUSE）是候选；必须逐项标记 open/read/write/pread/pwrite/truncate/rename/link/symlink/unlink/stat/readdir/chmod/chown/xattr/locking/mmap/fsync、稀疏文件及原子追加的支持级别、限制和内核缓存行为。未支持操作返回明确 errno/错误，不得假装成功；在具备相关验证前不声明适合数据库 WAL、构建缓存或其他高度依赖 POSIX 的负载。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-002](../docs/06-storage-data-tablefs.md)；[V1-TFS-004](../docs/06-storage-data-tablefs.md)；[V1-TFS-006](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1461–1461 行。

> - TFS-007｜打开句柄和缓存。 open → unlink、同时读写、同一文件多句柄、目录遍历、句柄断线与恢复均须有明确定义。POSIX 挂载缓存、客户端属性缓存、表查询缓存必须有可验证失效/版本验证协议；不能因为表写入绕开挂载缓存而让程序长期看到无期限旧内容。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-001](../docs/06-storage-data-tablefs.md)；[V1-TFS-002](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1463–1463 行。

> - TFS-008｜表模型和查询。 基础 TableFS 至少提供按命名空间、父目录、文件类型、基础属性和版本进行的授权查询/分页，不要求安装 Dataset 才能读取文件目录表。查询应基于稳定查询 Schema、资源预算和确定的授权上下文；不把任意原始 SQL/表达式交给高权限宿主进程执行，也不把 path 当稳定 ID。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-007](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1465–1465 行。

> - TFS-009｜Dataset 映射。 Dataset 字段契约仍为 uid/name?/content/tag/score/vector?/extra_data?；允许 Dataset 行将 content 引用到同一内容版本，inode_id 与 (dataset_id, uid) 各自独立且不可混淆。大书、仓库等一条逻辑记录可以被导出/呈现为多个文件视图或单个内容对象；不强制把文件、目录和原始数据行按 1:1 绑定。双向写入、删除传播、Schema 与冲突策略必须按具体映射声明，不可自动覆盖另一方。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-007](../docs/06-storage-data-tablefs.md)；[V1-DAT-005](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1467–1467 行。

> - TFS-010｜智能查询目录。 可选 Smart Directory 从受授权的 View/Catalog 查询生成虚拟目录项；初期默认只读。须明确 readdir 的一致性窗口、结果数量限制、名称冲突规则、符号链接/目录遍历行为及索引水位，禁止把查询目录的移动、删除、复制直接隐式解释为修改真实记录。对无法获得授权或超出预算的列表应拒绝或明确截断，不能泄露未经授权的数量或文件名。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-011

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-TFS-007](../docs/06-storage-data-tablefs.md)；[V1-DAT-005](../docs/06-storage-data-tablefs.md)；[V1-REC-023](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1469–1469 行。

> - TFS-011｜索引一致性与删除。 文件名/目录元数据的权威查询在成功提交后必须符合声明的读一致性；全文、列式、JSON、向量等派生索引可以异步刷新，但返回水位及落后状态。撤销和删除在权威授权/删除屏障生效后，旧缓存/索引不得再绕过授权暴露在线内容；异步物理清理需结合快照与备份保留记录进度。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-005](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1471–1471 行。

> - TFS-012｜访问控制与去重隔离。 Linux UID/GID、用户命名空间、PsiOS Principal/Capability、ACL 与查询 API 必须映射到同一个资源授权裁决；容器、Agent、Shell、FUSE 和 Data Services 都不能绕过。跨主体搜索、统计、去重、哈希及查询时延不得无意泄露文件存在性；跨安全域内容去重仅在经验证不会改变所有权、隔离和密钥边界时启用。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-013

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-006](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1473–1473 行。

> - TFS-013｜快照和垃圾回收。 快照锁定命名空间提交序列、内容版本清单及所需日志，分别声明 crash-consistent/application-consistent 范围；索引如可重建，备份须保存重建配置。GC 必须保护活跃 inode、打开句柄、快照、备份与导出保留引用；压实/迁移失败不能让仍被引用的内容丢失。需能从独立环境检查元数据、内容校验和恢复关系。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-014

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-005](../docs/06-storage-data-tablefs.md)；[V1-TFS-008](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1475–1475 行。

> - TFS-014｜容量、故障与性能隔离。 配额、元数据/WAL 空间、内容池、索引池、内存缓存和后台压实分别预算；满盘、掉盘、只读降级、事务日志损坏、对象丢失、索引落后时返回可诊断状态。后台查询不能耗尽宿主恢复所需 CPU/内存/I/O；大目录分页、十万/百万级小对象、随机 I/O 和高写入放大需要与原生文件系统对照实测，不预设性能结论。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-015

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-STO-003](../docs/06-storage-data-tablefs.md)；[V1-TFS-008](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1477–1477 行。

> - TFS-015｜容器与集群边界。 TableFS 卷遵守 Volume 的生命周期、所有权、挂载授权和 ReadWriteOnce 等访问模式；同一卷单写者为首轮目标。跨节点访问、缓存一致性、共享多写与迁移需新的 fencing/复制/仲裁证据后单独开启；Controller 断联、租约过期不得放任两个 Writer 对同一卷写入。

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### TFS-016

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-TFS-006](../docs/06-storage-data-tablefs.md)；[V1-TFS-008](../docs/06-storage-data-tablefs.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十三、Storage & Data：存储、数据与恢复 › TableFS：文件视图与表引擎的统一契约（v0.3），第 1479–1492 行。

> - TFS-016｜出口与可恢复性。 必须支持不依赖原 TableFS 进程的版本化导出/导入（含目录结构、权限、文件属性、内容、软/硬链接关系、校验与可选 Dataset 映射声明）；恢复前检测目标容量、Schema、密钥及引用完整性，优先恢复到新目标验证后激活。卸载服务不得默认删除卷；元数据服务失效时保留安全只读检查、独立救援与导出路径，不能创建空的新卷冒充原卷。 实现方向（非最终选型）： 首轮单机 PoC 可评估 Rust 用户态服务 + FUSE 接口 + 带 WAL 的事务元数据引擎 + 本地内容文件/分…（完整原文见JSON与归档）

**溯源范围：** 5 处定义、20 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## UPD（6 项）

### UPD-01

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UPD-001](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-01：单一更新事务与状态机，第 1049–1053 行。

> UPD-01：单一更新事务与状态机 系统更新必须由一个权威控制器串行协调。生命周期为 Planned → Downloading → Verified → Staged → BootPending → TrialBoot → Committed；失败转入 Failed、RolledBack 或 RecoveryRequired，状态与原因持久化。重启后从已提交的检查点恢复，不能靠内存中的进度推断成功。 计划阶段必须检查平台适配、依赖、空闲空间、当前系统与数据 Schema、回退兼容性、供电条件及维护窗口。下载或校验…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UPD-02

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UPD-002](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-02：试启动与回退，第 1055–1059 行。

> UPD-02：试启动与回退 试启动必须有持久化尝试计数、可配置的健康判据及失败回退策略。启动器能力与用户空间健康确认需要成对验证，不能把“内核开始运行”当成整个部署可用。systemd-boot 提供启动计数与回退机制，但是否采用它以及覆盖哪些平台仍由 ADR 决定。F3 自动回退后应当暂停对同一失败制品的自动重试并记录故障，防止升级—回退循环。用户可明确选择重试或其他版本。对于未实现自动启动回退的平台，发布支持说明必须清楚标注限制，并通过可用的人工恢复场景验收，不能宣称具备自动恢复。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UPD-03

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UPD-003](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-03：三类升级分别建模，第 1061–1069 行。

> UPD-03：三类升级分别建模 | 操作 | 可以自动撤销的部分 | 必须另行验证的部分 | |---|---|---| | OS 部署更新 | 系统部署与对应启动项 | LocalConfig、CoreState、驱动兼容与数据访问 | | 应用制品更新 | 镜像/包版本、部署配置 | 持久卷格式、用户配置、外部副作用 | | 数据模式迁移 | 仅经验证可逆的迁移 | 数据丢失、格式收缩、其他服务兼容及恢复点 | 每个迁移必须声明来源/目标 Schema、前置条件、幂等标识、是否可逆、读写兼容范围、备份要求及恢复…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UPD-04

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-REC-008](../docs/05-recovery-consistency.md)；[V1-REC-018](../docs/05-recovery-consistency.md)；[V1-UPD-003](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-04：状态与数据的一致恢复，第 1071–1073 行。

> UPD-04：状态与数据的一致恢复 CoreState 与应用数据均须支持检查点和可验证恢复。恢复选择必须说明恢复点时间/版本、将丢失的后续修改以及受影响资源。恢复到新位置并验证后再切换应当优先于覆盖唯一原件。回滚后旧服务遇到更高 Schema 应拒绝写入并进入兼容性故障状态，不能自行猜测降级格式。

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UPD-05

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UPD-001](../docs/03-host-updates-network-devices.md)；[V1-UPD-002](../docs/03-host-updates-network-devices.md)；[V1-FND-005](../docs/03-host-updates-network-devices.md)；[V1-REC-016](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-05：恢复场景与降级行为，第 1075–1084 行。

> UPD-05：恢复场景与降级行为 | 故障 | 必须提供的行为 | |---|---| | 下载断网、校验不符、空间不足 | 保留当前部署，清楚报告失败，可清理未完成缓存 | | 部署期间断电 | 重启后识别完整/未完成部署，不把半成品设为已提交 | | 新内核或驱动无法启动 | 使用经验证的回退路径；保留诊断资料 | | CoreState 损坏 | 停止依赖它的修改操作，保留本机只读诊断与恢复入口 | | 系统盘损坏、数据盘仍在 | 从可信介质重装并核验/导入已有卷，保持数据归属 | | 远端仓库或签名服务不可…（完整原文见JSON与归档）

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UPD-06

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UPD-004](../docs/03-host-updates-network-devices.md)；[V1-REL-003](../docs/13-operations-release-governance.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 十七、Update & Recovery：系统、应用及数据迁移 › UPD-06：更新信任与版本支持，第 1086–1088 行。

> UPD-06：更新信任与版本支持 发布通道、自动下载、自动安装和自动重启必须分别可配置。安全修复的分发、回滚制品保留与已失陷版本封禁也须分别建模。恶意降级更新元数据与管理员授权恢复到旧系统属于不同动作；恢复旧系统不得顺带回退可信根和撤销记录。离线更新包必须携带所需验证材料，并明确处理元数据过期和不可信时间，不能默默关闭验证。F4

**溯源范围：** 6 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## UX（8 项）

### UX-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-001](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1657–1657 行。

> | UX-001 | 必须将首次启动设计为可完成的流程：选择已有或新建账户、网络、时间与地区、加密恢复方式、更新通道、管理访问和可选 Agent 模型提供者；模型配置可跳过，完成基础安装不需要购买 API 或联网注册。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-001](../docs/03-host-updates-network-devices.md)；[V1-SEC-013](../docs/04-security-identity.md)；[V1-CLU-002](../docs/12-cluster-coordination.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1658–1658 行。

> | UX-002 | 必须提供对应的无头初始化方式，配置中只使用短期引导凭据或安全的 Secret 引用；初始化完成后撤销一次性入口，禁止镜像携带通用密码、相同 machine identity 或所有安装共享的私钥。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-001](../docs/03-host-updates-network-devices.md)；[V1-API-009](../docs/02-common-contracts.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1659–1659 行。

> | UX-003 | 必须区分通知、建议、授权请求和故障告警；耗时操作显示进度来源、可取消范围和结果，不把模型生成的推测作为系统真实状态。失败时可直接查看 Operation 和脱敏诊断。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-002](../docs/03-host-updates-network-devices.md)；[V1-GUI-009](../docs/07-applications-packages-gui.md)；[V1-SEC-019](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1660–1660 行。

> | UX-004 | 必须使账户切换、退出登录、屏幕锁定和远程断连传播到关联授权与会话；后台任务是否继续按已授予的服务身份和任务策略判断，不借用已经结束的桌面登录。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-005

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-API-006](../docs/02-common-contracts.md)；[V1-TSK-015](../docs/08-tasks-events.md)；[V1-FND-006](../docs/03-host-updates-network-devices.md)；[V1-UX-002](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1661–1661 行。

> | UX-005 | 必须从 API/UI 文案层支持本地化、时区显示与可复制的稳定错误码；内部事件记录 UTC 时间与排序标记，任务调度记录用户时区和夏令时行为，租约超时采用适当的单调计时而非仅靠本地墙钟。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-002](../docs/03-host-updates-network-devices.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1662–1662 行。

> | UX-006 | 建议在 Argon 和 Shell 的核心流程验证键盘操作、屏幕阅读、焦点顺序、缩放、高对比度和输入法；中文输入、非 ASCII 路径、多显示器与混合缩放进入发布兼容矩阵。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-UX-002](../docs/03-host-updates-network-devices.md)；[V1-SEC-017](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1663–1663 行。

> | UX-007 | 必须提供明确的权限和资源页面，使用户能查看谁拥有访问、哪些 Agent/任务正在使用、授权何时到期、如何撤销，以及应用卸载后数据去向。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### UX-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-014](../docs/04-security-identity.md)；[V1-SEC-016](../docs/04-security-identity.md)；[V1-NET-003](../docs/03-host-updates-network-devices.md)；[V1-GUI-009](../docs/07-applications-packages-gui.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 二十八、用户体验、运维配置与后续能力边界 › 28.1 安装到日常操作的完整用户路径，第 1664–1664 行。

> | UX-008 | 必须为远程管理提供会话超时、重新认证、设备撤销、连接中断恢复和权限变更提示；公网开放由配置明确启用，配置丢失时不得自动扩大网络暴露。 |

**溯源范围：** 6 处定义、6 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

## WFL（14 项）

### WFL-001

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-001](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.1 权威对象与职责，第 1933–1933 行。

> WFL-001（必须）：统一 Project/Milestone/WorkItem/WorkflowDefinition/WorkflowRun/ApprovalRequest/ExternalLink 的稳定身份、属主、权限域、Schema 版本、创建/更新者和资源版本。只有对应可选服务被安装时才对外宣称可用，不扩大 Host Core 必装项。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-002

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-BASE-004](../docs/01-product-and-architecture.md)；[V1-TSK-001](../docs/08-tasks-events.md)；[V1-WFL-001](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.1 权威对象与职责，第 1935–1935 行。

> WFL-002（必须）：区分长期工作 WorkItem、目标 Task、编排 WorkflowRun 与单次执行 Run；其生命周期独立但须通过因果/关联 ID、事件和状态投影互相追溯，不能从多个系统维护相互覆盖的权威状态。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-003

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-011](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.1 权威对象与职责，第 1937–1937 行。

> WFL-003（必须）：WorkItem 支持标题、描述、归属、优先级、里程碑、父子/阻塞依赖、指派、验收条件、文档与交付物引用、外部 Issue 链接及明确的状态转换。工作项关闭以实际验证的交付证据为依据，不以 Agent 自报完成为准。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-004

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-002](../docs/11-workflows-projects.md)；[V1-WFL-003](../docs/11-workflows-projects.md)；[V1-WFL-010](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.1 权威对象与职责，第 1939–1939 行。

> WFL-004（必须）：WorkflowDefinition 声明触发条件（人工、事件、定时、关联 WorkItem）、步骤依赖、输入/输出 Schema、能力选择条件、预算/限时、重试/补偿策略和人工等待点。定义一旦用于运行，须绑定不可变版本或内容摘要；更新定义不悄悄改变在途实例。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-005

**迁移：** `superseded_by` / `Mapped`。旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。

**去向：** [V1-WFL-002](../docs/11-workflows-projects.md)；[V1-WFL-003](../docs/11-workflows-projects.md)；[V1-WFL-005](../docs/11-workflows-projects.md)；[V1-WFL-006](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.2 编排、委托与恢复，第 1943–1943 行。

> WFL-005（必须）：支持有向无环依赖、条件分支、并行与汇合、受限重试、审批等待及有界循环/动态子任务。调度器可按 Agent 能力、可用性、成本/Token、数据驻留规则与权限匹配，而不硬编码模型名称。存在回路时必须有显式次数/时间/资源上限。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-006

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-004](../docs/11-workflows-projects.md)；[V1-TSK-005](../docs/08-tasks-events.md)；[V1-REC-010](../docs/05-recovery-consistency.md)；[V1-REC-011](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.2 编排、委托与恢复，第 1945–1945 行。

> WFL-006（必须）：WorkflowRun 的关键状态、步骤依赖解除、任务接受、工具调用与审批动作必须可持久恢复。消费者依照 EVT-004 去重，执行器遵守 AGT-003 的副作用分类；对于不可安全重试的动作，采用结果核实或 OutcomeUnknown，而非盲目重新执行。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-007

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-SEC-004](../docs/04-security-identity.md)；[V1-TSK-014](../docs/08-tasks-events.md)；[V1-CHAT-009](../docs/10-conversations.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.2 编排、委托与恢复，第 1947–1947 行。

> WFL-007（必须）：Agent 自主分派任务或申请其他 Agent 帮忙时，实际工作委托须形成有权限范围、预算、关联原任务与取消路径的结构化子任务；聊天 @、DM 或普通消息仅构成交流，不直接产生未受控执行权限。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-008

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-007](../docs/11-workflows-projects.md)；[V1-WFL-014](../docs/11-workflows-projects.md)；[V1-SEC-005](../docs/04-security-identity.md)；[V1-SEC-007](../docs/04-security-identity.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.2 编排、委托与恢复，第 1949–1949 行。

> WFL-008（必须）：人工审批、受保护分支合并、部署和危险设备操作必须根据 System Policy 检查授权主体、审批对象版本与条件、有效期和审计记录；独立审查须验证独立执行者身份和证据，不把“另一个 Agent 回复了通过”当成代码审查已完成。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-009

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-008](../docs/11-workflows-projects.md)；[V1-WFL-009](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.2 编排、委托与恢复，第 1951–1951 行。

> WFL-009（必须）：运行器应提供暂停、继续、取消、重试被拒原因、资源不足处理及用户手动接管。关闭浏览器、群成员离线或模型额度耗尽不能擅自取消运行中的任务；控制器/节点宕机后可安全恢复或明确提示不能自动恢复。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-010

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-012](../docs/11-workflows-projects.md)；[V1-WFL-013](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.3 Project 与外部系统的协作，第 1955–1955 行。

> WFL-010（必须）：Forgejo/GitHub/CI 可通过连接器和 Webhook 与 Project/Workflow 关联；为每个字段声明权威来源。例如 Forgejo 的 PR 合并状态和 CI 结果以 Forgejo 实测为准，PsiFlow 的 WorkflowRun 状态以 PsiFlow 为准；双向同步要有 external_event_id、幂等键和循环防护。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-011

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-014](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.3 Project 与外部系统的协作，第 1957–1957 行。

> WFL-011（必须）：开发型工作流可以按 Plan → Implement → CI → Independent Review → Protected Merge → Verify → Close 编排；每一阶段把真实提交、测试记录和审批证据绑定至 Task/WorkItem。没有真实 PR/CI/权限确认，禁止从群内自然语言汇报将任务标成已合并或已发布。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-012

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-001](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.3 Project 与外部系统的协作，第 1959–1959 行。

> WFL-012（必须）：Workflow 和 Project API 可被人类、原生 Agent、第三方 Agent、Shell、CLI 同等受控访问。任务/项目权限不能由项目群成员身份无限传递，Project ACL 与 Room ACL 彼此独立；跨项目委托和跨 Agent 能力须再次检查资源及数据访问授权。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-013

**迁移：** `consolidated_into` / `Mapped`。按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。

**去向：** [V1-WFL-010](../docs/11-workflows-projects.md)；[V1-TSK-004](../docs/08-tasks-events.md)；[V1-TSK-010](../docs/08-tasks-events.md)；[V1-REC-009](../docs/05-recovery-consistency.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.3 Project 与外部系统的协作，第 1961–1961 行。

> WFL-013（必须）：工作流的历史、执行事件和 Artifact 采取各自保留/配额与可恢复策略；删除聊天记录不能破坏仍在执行的任务和持久审计，清理输出必须区分“已回收结果”与“没有执行记录”。敏感 Artifact 引用按访问时权限重新校验。

**溯源范围：** 3 处定义、12 处引用；1 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

### WFL-014

**迁移：** `consolidated_into` / `Mapped`。最小Task/Workflow/Chat能力与Project/Studio可选部署分开；后端尚未ADR批准。

**去向：** [V1-ARCH-003](../docs/01-product-and-architecture.md)；[V1-ARCH-004](../docs/01-product-and-architecture.md)；[11.11 验收和可选部署](../docs/11-workflows-projects.md)。

**选定原文：** [SRC-SPACE-V06](../sources/archive/space-unified-v0.6.md)，PsiOS｜统一架构与持续开发规划 · 合并 v0.6 › 三十三、PsiFlow：Project、Workflow 与 Agent 协作工作流（v0.5） › 33.3 Project 与外部系统的协作，第 1963–1963 行。

> WFL-014（必须）：所有三版支持最小 Workflow/Task 接口；完整 Project Service、中心化 WebUI、跨节点自动调度和可视化编排是按 Profile/可选服务加载的能力。编排与项目持久化由 ADR-22 验证；消息存储/投递由 ADR-21 验证；跨服务确认由 ADR-24 验证，均须完成性能、恢复和权限实测后选定，不规定 SQLite/外部 Broker 成为唯一方案。

**溯源范围：** 4 处定义、12 处引用；3 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。

