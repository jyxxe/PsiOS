# 15｜验收方法、证据与发布判据

## 1. 三种验证的边界

**V1-TEST-001｜文档检查不能冒充产品验收。** 本包的静态检查只证明文档链接、编号、JSON结构、Schema和示例满足所列规则。它不能证明真实数据库事务、进程隔离、授权、网络撤销、fencing、文件持久性或外部幂等已经实现。产品场景注册表保持 `NotRun`；源码复用与实现保持 `NotVerified`。

| 验证层 | 输入 | 可以得出的结论 | 不能得出的结论 |
|---|---|---|---|
| 文档包静态检查 | Markdown、JSON、Schema、示例、清单 | 可解析、引用存在、规定字段/状态一致、例子按结构通过或被拒绝 | 实际服务具有该安全性或可用性 |
| 合同/并发/安全测试 | 确切构建的服务、数据库与适配器 | 所测实现对指定情形满足合同 | 未测试后端、平台与规模也满足 |
| 故障/恢复/发布验收 | 确切镜像、硬件/VM、真实外部测试端点 | 声明故障范围内可恢复，阈值与证据对应 | 任意灾难下零丢失、全协议互通或无限规模 |

JSON Schema只能验证对象形状与一部分局部条件。授权、CAS、current epoch、命名空间未过期、对象归属、外部效果、图的可达性/有界性、摘要认证和存储故障范围需语义验证或真实系统实验。

## 2. 证据记录

**V1-TEST-002｜证据绑定确切对象。** 每次运行保存 `test_id, requirement_ids, source_commit, build_artifact_digest, edition, profile_id, environment, prerequisites, steps, fault_injection, expected, observed, evidence_refs, reviewer, executed_at, result, limitations`。版本字符串和截图上的绿色标记不能替代完整提交SHA与制品摘要。模板见[evidence-template.json](../acceptance/evidence-template.json)。

上述是逻辑信息项，实际JSON模板使用以下确定路径，不能另生成一份互不对应的格式：

| 逻辑信息 | 模板字段路径 |
|---|---|
| 测试、覆盖要求、运行身份 | test_id、requirement_ids、execution_id |
| source_commit / build_artifact_digest | product.commit_sha / product.artifact_digest |
| edition / profile_id | environment.edition / environment.profile_ref |
| prerequisites | setup.fixture_manifest_ref、setup.precondition_results |
| steps / fault_injection | execution.steps / execution.fault_injection_points |
| expected / observed | assertions[].expected / assertions[].observed |
| evidence_refs | assertions[].evidence_refs及required_evidence[]中的uri/digest |
| reviewer / executed_at | review.reviewer_principal_id / timing.started_at与finished_at |
| result / limitations | result / limitations |

测试计划注册表的`status`表示计划当前验收汇总；单次实际执行证据的`result`表示该次结果。两者通过test_id与execution_id关联，不能让模板默认NotRun被误当为某次已经执行。

结果枚举：

| result | 进入条件 |
|---|---|
| NotRun | 尚未执行；observed/evidence为空，不放推测结果 |
| Passed | 按预先确定场景执行，全部断言通过，环境、日志和摘要齐全 |
| Failed | 至少一个断言失败；保存失败和残余影响，不以重试最后一次成功抹掉 |
| Blocked | 依赖、访问、设施或实现缺失使测试无法完成；明确阻塞原因 |
| NotApplicable | 该发布Profile确实不含该能力，附理由与审批记录；不能用于隐藏已宣传功能 |

产品测试中人工核实必须有可归属身份；模型生成的说明只能作辅助记录。只保存哈希但丢失实际日志不构成可复核证据。含秘密/个人数据的证据按权限保留，报告使用受控引用，不把原始数据塞进公开日志。

## 3. 测试分层与故障注入

**V1-TEST-003｜重点验证不能从局部成功推导的性质。** 分层测试包括编码/解析、跨语言摘要、并发状态模型、授权否定用例、跨服务故障恢复、真实资源/设备强制以及完整用户流程。只测试“调用返回成功”不够；必须观察持久状态、资源端真实效果及恢复后的行为。

| 风险域 | 必须覆盖的注入 | 关键观察点 |
|---|---|---|
| 准入/审批 | 两Operation竞争一次批准；批准后计划或head改变；撤销与准入并发 | CAS唯一绑定；执行端前提；旧决定不能执行新效果 |
| Task/Workflow | 接受后ACK丢失；Cancel先到；数据库恢复点不同；去重窗口过期 | 同一任务关联、取消墓碑、未知提交阻塞、不能换键重发 |
| 效果与补偿 | 对端成功但本地超时；取消后迟到成功；补偿超时 | Unknown保留；效果证据追加；不将部分补偿报告全量回滚 |
| Conversation | 两Writer；成员退出竞态；正文回收后旧发送；通知跳序 | 资源端fencing、统一日志裁决、命名空间拒旧、独立处理游标 |
| 安全恢复 | 授权撤销后恢复旧备份；独立安全记录全丢；克隆同时上线 | Quarantined；独立恢复身份；不复活旧凭据；新身份不掩盖外部历史 |
| 存储/TableFS | 元数据/对象提交交错断电；GC与新pin竞态；旧fd、rename、fsync | 不丢已声明持久数据；无悬垂可见引用；旧世代访问拒绝；能力矩阵准确 |
| 网络/更新 | 旧回滚定时器迟到；管理连接被旧会话误确认；试启动失败；Schema不兼容 | generation约束；新连接验证；救援可用；二进制与数据回退分开 |
| 设备/GUI | 热拔、挂起、用户接管、锁屏、Portal撤销、token消费后崩溃 | 实际输入停止、旧流/句柄拒绝、时间和session世代不复用 |
| 第三方协议 | 不支持的能力；通知丢失；远端completed但业务错误 | 准确版本/能力；不假定幂等；协议完成与业务成功分开 |
| 发布与供应链 | 恶意PR/包；签名回放；密钥轮换；SBOM与真实制品不符 | 隔离解析、构建凭据不外泄、认证来源与完整性分离、拒绝未授权制品 |
| 容量与退出 | 元数据/WAL/审计空间满；大量小文件/队列；独立导出重建 | 保留诊断/恢复空间；拒绝可解释；导出足够独立恢复并验证 |

故障实验在隔离VM、测试卷、专用设备和获准外部测试环境执行。涉及真实写、格式化、密钥轮换或发布的实验另建OperationPlan；不能为证明安全而破坏用户生产数据。

## 4. 数值阈值

**V1-TEST-004｜阈值必须预先冻结。** RPO、RTO、撤权最长传播、离线许可时限、队列容量、日志/结果保留、恢复余量、输入停止延迟和P95/P99延迟由具体Profile与ADR确定。当前未测数值用 `TBD`，不能填写虚构性能承诺。尚无有限阈值的相关自动化能力不能进入发布支持列表。

RPO按实际故障与恢复集测量；仅恢复旧备份时不得声称“Accepted消息永不丢”。RTO包含发现、获取密钥、恢复介质、对账和恢复服务的实际边界；不只计数据拷贝。撤权指标区分控制面受理、逐资源Enforced和无法撤销的既有效果。

## 5. 覆盖与追踪

[需求注册表](../registries/requirements.json)列出v1规范条款及其文本；[测试注册表](../registries/test-cases.json)保存待运行场景；[问题修正台账](../registries/findings.json)记录旧文依据、严重度、纠正位置和验证方向。旧测试仍见[历史测试映射](../migration/legacy-tests.md)，不因新测试编号替换而失去原有验收目标。

**V1-TEST-005｜覆盖可查但不伪造一对一。** 一条测试可能覆盖多个需求，一个需求也可能需要多种后端/故障证据。文档能说明的条款可经审阅验证；事务、授权、持久性和设备性质必须经实现实验。注册表中的文档映射表示“有对应设计”，不表示产品已满足，更不自动计算为测试通过率。

首轮发布逐项审查：支持列表内每个能力至少有正常路径、拒绝路径、恢复路径；所有相关P0安全问题有实现证据或功能被禁用；无法对未知效果做安全恢复的自动写流程保持关闭。

## 6. 文档包复核方法

安装 `tools/requirements.txt` 指定的文档校验依赖后，从包根运行 `python tools/validate_package.py`。此命令只读当前包，打印检查结果；不会调用PsiOS、外部服务、格式化设备或执行归档源码。无需联网解析Schema；所有 `$ref` 在包内注册。

复核项目：JSON严格解析/重复键、Schema语法和本地引用、正反示例、十进制计数范围、受限摘要向量、规范ID唯一、内部文件链接、注册表目标、工作项依赖及压缩清单。文档全量合订本和HTML是由分章生成的派生物，不可手改它们产生第二套规范。

包根的验证报告明确记录实际检查通过数和未运行的产品验收。缺少证据时如实失败或阻塞，不能为使包“全绿”删除未解决的风险。

## 7. 浏览器桌面工作闭环

新增 AT-SHL-001～015 对应第 07 章 V1-SHL-001～012：从登录桌面、应用并排工作、文件交给 Agent，到离开窗口继续任务、通知返回成果。还需覆盖布局与授权恢复、账户隔离、NativeWeb 限制、远程输入接管、无图形后端、输入法、无障碍、窄屏与性能预算。

这些场景与原有 GUI、Task、Chat 和安全测试组合执行；单次本机状态页演示、静态截图或文档检查不能替代完整桌面验收。测试注册表和场景阅读版同步维护，当前均为 NotRun / NotVerified。
