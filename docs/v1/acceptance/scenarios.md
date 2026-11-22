# 完整验收与审查场景

本文件从 [统一注册表](../registries/test-cases.json) 展开。135 个场景均为 **NotRun / NotVerified**；覆盖 245 条规范要求的验证计划，不表示这些要求已经通过。DocumentReview 是待执行的文档/证据审查；Contract、Integration、FaultInjection、Hardware 是所列实现或平台验证。以下前置条件、操作和结果均为计划，不是实际记录。执行与证据规则见 [验收说明](README.md)。

## SEC-V1

### SEC-V1-01｜两用户/两个安全域创建同名对象并交换 ID、分页、日志和搜索请求

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-001`、`V1-SEC-003`、`V1-DAT-005`
- **阶段：** M0、M2、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立两个独立安全域，各有同名 Resource、私密内容、附件与搜索索引；两套真实认证凭据权限互不相交。

**操作、审查步骤或故障注入**

1. 交叉替换 resource_id、conversation_id、分页游标与附件引用；分别走 CLI、HTTP、搜索和日志接口。
2. 保持资源总数/命名相近，比较拒绝响应、搜索计数和缓存行为，检查是否形成跨域枚举通道。

**通过条件及结论边界**

- 无跨域正文、片段、计数或秘密泄露；错误不帮助枚举

**必要证据**

- 授权决策与拒绝日志（脱敏）
- 跨域响应及负例断言
- 缓存/索引过滤配置和源数据清单

### SEC-V1-02｜子 Agent 扩大父任务资源或预算，使用显示名伪造用户

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-003`、`V1-SEC-004`、`V1-TSK-014`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立父 Task 的有限资源与预算委托，子 Agent 只能访问其中一个项目；另设同显示名但不同 Principal。

**操作、审查步骤或故障注入**

1. 由子 Agent 请求超范围目录、增加预算、继续委托未授权能力，并在请求体中填入父用户显示名或 principal_id。
2. 核对新 Grant/Delegation 和预算账本是否出现未授权扩张。

**通过条件及结论边界**

- 授权失败，无新 Grant；原责任链保留

**必要证据**

- 父子委托图与约束快照
- 实际认证主体与请求内容对照
- Grant、预算与审计账本差异

### SEC-V1-03｜两个 Operation 同时消费同一单次审批

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-007`、`V1-SEC-008`、`V1-WFL-007`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 创建只允许一次消费的 ApprovalDecision 与两个不同 operation_id，两个 Operation 的其他条件都有效。

**操作、审查步骤或故障注入**

1. 用并发屏障同时消费该决策；在消费提交和响应返回之间注入响应丢失。
2. 对获胜 Operation 重试，再让失败 Operation 重试；核对绑定不可被删除或自动返池。

**通过条件及结论边界**

- 仅一个绑定成功，另一个拒绝；同一 Operation 重试返回原绑定

**必要证据**

- ApprovalUse CAS 交错时间线
- 两个 Operation 的权威查询
- 回执丢失与同操作重试记录

### SEC-V1-04｜绑定审批后杀死引擎，恢复并重复派发

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-007`、`V1-WFL-004`、`V1-TSK-005`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备已批准待执行的持久 Workflow、DispatchIntent、Operation 和可观察外部测试端点。

**操作、审查步骤或故障注入**

1. 在审批绑定提交后、Workflow 保存进度前杀死引擎，恢复同一数据库并重投 outbox。
2. 按原 operation_id 与 origin_ref 查询，核对 Task、审批用量及外部动作计数。

**通过条件及结论边界**

- 查询同一 Operation；不重复消费、不创建新非幂等效果

**必要证据**

- 崩溃点与恢复检查点
- 审批绑定和 DispatchIntent 账本
- 测试端点效果计数与操作 ID

### SEC-V1-05｜撤销与准入并发，之后旧许可在分区资源端使用

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-009`、`V1-SEC-017`、`V1-SEC-018`
- **阶段：** M0、M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 为在线与可暂时离线的执行端分别配置已声明的撤权强制能力和有限许可有效期。

**操作、审查步骤或故障注入**

1. 使撤销与 ExecutionPermit 准入同时发生，再隔离一个执行端并使用旧许可。
2. 观测每个目标实际停流/fence/终止时刻与撤权响应，恢复网络后验证旧许可不能复活。

**通过条件及结论边界**

- 在线端拒绝；离线端按声明窗口失效，响应不能虚报全部强制完成

**必要证据**

- 策略/许可/撤销代次时间线
- 各资源端实际拒绝证据
- 声明离线窗口与测量结果

### SEC-V1-06｜审批后替换制品、改目标 spec，或仅更新心跳

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-005`、`V1-SEC-006`、`V1-SEC-009`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备含制品摘要和目标 spec 前提的已批准 OperationPlan，另有可独立变化的心跳/status 字段。

**操作、审查步骤或故障注入**

1. 分别替换制品字节、修改目标效果字段与只更新心跳，使用原审批执行。
2. 比对计划摘要、效果指纹和执行许可的处理差异。

**通过条件及结论边界**

- 效果变化拒绝旧审批；纯心跳不触发多余审批

**必要证据**

- 三组输入与摘要测试向量
- 审批与效果前提查询
- 执行拒绝/允许的权威判定

### SEC-V1-07｜文件校验后替换符号链接、切换挂载或传入恶意归档

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-011`、`V1-APP-009`、`V1-TFS-002`
- **阶段：** M0、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 配置允许写入的受限根目录与未授权宿主目录，启用可控文件/挂载竞争器和隔离归档解析环境。

**操作、审查步骤或故障注入**

1. 在验证后操作前替换 symlink、父目录或挂载点，并提交路径穿越、硬链接逃逸和恶意归档夹具。
2. 检查实际打开对象、写入目标和宿主目录校验清单。

**通过条件及结论边界**

- 拒绝越界对象，无宿主无授权写入

**必要证据**

- 文件对象/inode 或等效身份记录
- 竞争调度与归档样本摘要
- 宿主目录前后校验和

### SEC-V1-08｜合法 Agent 受恶意 README、工具结果或聊天诱导跨项目写/外发

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-012`、`V1-AGT-005`、`V1-WFL-007`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 合法 Agent 拥有有限项目写权和明确数据外发范围，输入中加入恶意 README、工具结果和聊天内容。

**操作、审查步骤或故障注入**

1. 诱导读取另一项目 Secret、上传到未经允许的提供者、改变保护分支或自行批准；从多种工具入口重复尝试。
2. 核对拒绝来自结构化计划/策略边界，而非仅模型口头承诺。

**通过条件及结论边界**

- 结构化范围/外发约束拒绝；没有因文本自授权限

**必要证据**

- 攻击夹具与来源标记
- OperationPlan、出站策略和工具调用记录
- 无越界效果的目标侧观察

### SEC-V1-09｜从恶意同站子域发起 Cookie 写请求和 WebSocket 连接

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-014`、`V1-SEC-015`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 管理站使用 Cookie 认证；建立不同 origin 的同站恶意子域、合法管理 origin 与 WebSocket 测试客户端。

**操作、审查步骤或故障注入**

1. 从恶意 origin 发起简单表单写、fetch 写与 WebSocket 升级，并使用合法会话 Cookie。
2. 对管理 GET 接口重复访问，验证没有业务状态变更；合法 CSRF/Origin 路径作为对照。

**通过条件及结论边界**

- Origin/CSRF/通道检查拒绝；GET 无状态变更

**必要证据**

- 请求 Origin/CSRF/认证头脱敏记录
- WebSocket 握手拒绝与合法对照
- 资源状态前后差异

### SEC-V1-10｜在聊天、日志、文件名中注入脚本和主动附件内容

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-015`、`V1-GUI-008`、`V1-CHAT-011`
- **阶段：** M0、M3、M4
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备包含脚本、事件处理属性、危险 URL 和主动附件的消息、日志及文件名，使用管理页面展示。

**操作、审查步骤或故障注入**

1. 依次访问列表、详情、搜索命中、通知和导出预览，触发所有相关渲染入口。
2. 监测管理 origin 的执行上下文、凭据读取和外发请求，不使用真实生产 Secret。

**通过条件及结论边界**

- 展示无法取得管理页执行权限或凭据

**必要证据**

- 注入样本摘要与渲染截图
- 浏览器隔离策略与执行/网络记录
- 未产生管理权限效果的断言

### SEC-V1-11｜登录前固定会话、提权后复用旧会话、注销后续连

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-014`、`V1-SEC-016`、`V1-SEC-018`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备登录前会话、登录后会话、提权会话和长连接客户端，所有会话 ID 可单独追踪。

**操作、审查步骤或故障注入**

1. 固定预登录会话后登录，提权后重放旧会话；注销或撤销后继续使用 HTTP 与已有长连接。
2. 核对轮换、敏感动作的新鲜认证要求和长连接失效传播。

**通过条件及结论边界**

- 旧会话不可再执行敏感操作；长连接遵守撤权

**必要证据**

- 会话代次/生命周期记录
- HTTP 与长连接拒绝日志
- 新鲜认证与注销操作证据

### SEC-V1-12｜客户端伪造代理身份头，Token 交给错误受众服务

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-003`、`V1-SEC-014`、`V1-API-017`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 配置可信代理边界和两个不同 audience 的资源服务，使用可追踪的测试令牌。

**操作、审查步骤或故障注入**

1. 从非可信连接伪造代理身份头，并把只针对服务 A 的令牌交给服务 B。
2. 同时测试正确代理链和正确 audience，避免把全面拒绝误判为安全实现。

**通过条件及结论边界**

- 拒绝冒名和跨受众重放

**必要证据**

- 代理信任配置
- 签发者/audience 验证判定
- 合法与非法请求对照

### SEC-V1-13｜持有 fd、mmap、设备队列和 GUI 输入后撤权

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-017`、`V1-SEC-019`、`V1-GUI-003`
- **阶段：** M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 预先打开受控 fd、mmap、设备队列和 GUI 输入通道，并声明各后端可用的撤销强制能力。

**操作、审查步骤或故障注入**

1. 撤权后继续发起已有句柄写、内存映射写与输入事件；分别尝试支持和不支持强制终止的后端。
2. 检查每目标 RevocationResult 与实际数据面是否一致，保留已发生外部效果。

**通过条件及结论边界**

- 每目标强制状态准确；未知/不支持不显示完全回滚

**必要证据**

- 真实数据面写/输入观察
- 每目标撤权结果及确认时间
- 未支持能力与残留范围清单

### SEC-V1-14｜审计满盘、凭据服务离线或策略不可验证

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-020`、`V1-SEC-021`、`V1-FND-005`
- **阶段：** M1、M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 部署独立本地救援路径；分别为审计、凭据服务和策略服务提供可控故障注入。

**操作、审查步骤或故障注入**

1. 使审计存储满/只读、凭据服务失联或策略代次无法验证，再发起敏感变更。
2. 在同一故障期间使用合法本地救援身份读取诊断与执行获准恢复。

**通过条件及结论边界**

- 新敏感操作拒绝；本机独立诊断/恢复仍可用

**必要证据**

- 拒绝敏感变更的审计/错误证据
- 容量/服务故障注入日志
- 独立救援成功路径记录

### SEC-V1-15｜规范化测试向量含重复键、Unicode、超大整数及数组

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-006`、`V1-API-006`、`V1-API-007`
- **阶段：** M0
- **验证方式：** Contract
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备至少两种语言实现和固定摘要向量，包含重复键、非法 Unicode、组合字符、数组重排、超大整数及不同 contract 域。

**操作、审查步骤或故障注入**

1. 运行规范化与摘要计算；对非法输入验证统一拒绝，对合法输入比对字节与摘要。
2. 将实际待执行参数重新生成可审阅内容，确认没有隐藏的解析差异。

**通过条件及结论边界**

- 各语言摘要一致或一致拒绝；被审阅内容与执行参数一致

**必要证据**

- 语言/库版本与完整测试向量
- JCS 字节和域前缀/摘要对照
- 解析拒绝及审阅-执行一致性断言

## REC-V1

### REC-V1-01｜全盘丢失，仅有撤权前旧备份，无网络和可信见证

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-004`、`V1-REC-005`、`V1-REC-006`
- **阶段：** M1、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 创建 T1 备份后在 T2 撤销凭据，随后模拟所有可信本地状态丢失；仅保留旧备份与独立合法救援材料。

**操作、审查步骤或故障注入**

1. 断开网络与可信见证，恢复 T1 后尝试远程登录、Agent 自动化、旧节点证书与本地救援。
2. 检查恢复模式和自动化激活屏障，禁止从旧快照推断撤销新鲜度。

**通过条件及结论边界**

- 默认 Quarantined；旧自动化/远程凭据不启用；合法本地救援可用

**必要证据**

- 备份/撤销/全损时间线
- 恢复模式与旧凭据拒绝记录
- 独立救援路径证据

### REC-V1-02｜恢复旧管理员名单，管理员已于备份后撤销

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-004`、`V1-REC-006`、`V1-SEC-016`
- **阶段：** M1、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 旧备份包含管理员 A，独立当前恢复政策已撤销 A，并由另一恢复主体持有授权证明。

**操作、审查步骤或故障注入**

1. 恢复旧管理员名单后让 A 申请最高恢复权限，再使用当前合法恢复主体执行限定动作。
2. 比对旧业务名单与恢复身份权威是否被隔离。

**通过条件及结论边界**

- 不凭旧名单给恢复最高权限；采用独立恢复身份与范围

**必要证据**

- 独立恢复身份配置
- 两主体请求与判定
- 未复活旧管理员的授权差异

### REC-V1-03｜所有者重建信任，但一个外部服务不可达

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-007`、`V1-REC-017`
- **阶段：** M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 恢复集涉及本机、集群及两个外部服务，其中一个外部服务不可达。

**操作、审查步骤或故障注入**

1. 由所有者重新登记本机信任代次，并尝试恢复连接器和自动化。
2. 保持不可达端离线，验证系统逐端报告待轮换/待核实状态而非全局成功。

**通过条件及结论边界**

- 本机新代次可建立；外部旧凭据标未处理，相关自动化继续阻塞

**必要证据**

- 新旧恢复代次清单
- 逐外部凭据处置状态
- 被阻塞自动化及原因

### REC-V1-04｜Workflow 快照旧于 Task 接受，或审批使用账本旧于效果记录

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-008`、`V1-REC-011`、`V1-TSK-005`、`V1-SEC-007`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备不同恢复点的 Workflow、Task、Operation 与 ApprovalUse 备份，外部效果保持可观察。

**操作、审查步骤或故障注入**

1. 恢复较旧 Workflow/审批账本与较新 Task/Operation 记录，启动协调器但保持效果屏障。
2. 按原 origin_ref 与 operation_id 对账后推进；核对 Task 数和审批消费总量。

**通过条件及结论边界**

- 按稳定 origin/operation 对账；不产生重复 Task/审批消费

**必要证据**

- 各恢复组件的水位/摘要
- 对账决策记录
- Task/ApprovalUse/外部效果唯一性证明

### REC-V1-05｜备份后外部 PR/部署已成功，恢复前快照并补跑

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-010`、`V1-REC-011`、`V1-WFL-013`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 备份时 PR/部署尚未完成，备份后测试远端已成功且本地回执可丢失。

**操作、审查步骤或故障注入**

1. 恢复备份并触发原定补跑；分别提供可查询远端与不可查询远端。
2. 验证有证据时链接既有对象，无证据时保持 Unknown 而不是换键再执行。

**通过条件及结论边界**

- 不自动重放；查询确认或保留未知效果

**必要证据**

- 远端操作身份及结果
- 恢复后派发/查询流水
- 未知效果阻塞记录

### REC-V1-06｜幂等保留窗口短于灾难恢复窗口

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-012`、`V1-API-011`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 使用比恢复时间跨度更短的幂等保存窗口，准备已执行但正文/索引已过期的操作。

**操作、审查步骤或故障注入**

1. 恢复旧任务并重发原键，再尝试由重试逻辑换新键。
2. 检查过期命名空间、墓碑和跨服务对账屏障是否共同阻止重复效果。

**通过条件及结论边界**

- 旧键被拒绝/对账；换新键仍不能绕过效果屏障

**必要证据**

- 窗口配置与过期证据
- 原键/新键请求及拒绝原因
- 外部效果计数

### REC-V1-07｜元数据有附件/结果引用而对象丢失

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-009`、`V1-REC-017`、`V1-TSK-010`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 恢复清单引用结果和附件对象，故意移除一个必需对象并保留元数据。

**操作、审查步骤或故障注入**

1. 验证恢复集并尝试激活依赖该对象的 Task/Message/Workflow。
2. 读取缺失结果，检查错误明确指向缺失引用且未返回伪造空成功。

**通过条件及结论边界**

- 不伪造空结果；范围明确且错误可诊断，激活受限

**必要证据**

- 恢复清单与缺失对象清单
- 依赖激活决策
- 读取错误和资源状态

### REC-V1-08｜对象存在而引用丢失，或导入到另一租户

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-009`、`V1-SEC-001`、`V1-STO-002`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备仅有对象而无完整引用的导出，以及尝试导入另一安全域的同名对象。

**操作、审查步骤或故障注入**

1. 扫描孤立对象并申请认领；在确认所有权、摘要、权限之前尝试读取或加入索引。
2. 分别验证合法认领、归属不可证明和跨域未经授权三种分支。

**通过条件及结论边界**

- 隔离认领并验证归属，不泄露其他域对象

**必要证据**

- 对象摘要及来源链
- 认领授权与隔离记录
- 跨域访问负例

### REC-V1-09｜恢复旧 Controller，旧 Worker 仍可写共享卷

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-013`、`V1-REC-014`、`V1-CLU-005`
- **阶段：** M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 旧 Worker 保持共享卷写入通道，Controller 恢复旧快照；资源端支持可观察 fence。

**操作、审查步骤或故障注入**

1. 恢复 Controller 并提出替代调度；延迟资源端安装新 fence 的确认。
2. 确认前旧 Worker 可能仍可写，但新 Worker 不得获得写权；安装后验证旧写被拒。

**通过条件及结论边界**

- 未确认 fencing 前不重新调度；旧写被资源端拒绝

**必要证据**

- 共享卷实际写记录
- FenceReceipt 持久化证据
- 调度启用前后顺序

### REC-V1-10｜Conversation 备份尾部缺失但客户端持有旧 ACK

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-015`、`V1-CHAT-005`、`V1-CHAT-007`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 客户端持有尾部消息 Accepted 回执，恢复备份故意缺失该尾部和部分发送映射。

**操作、审查步骤或故障注入**

1. 恢复会话权威并让客户端按旧 seq/client_send_id 重同步与重发。
2. 检查 HistoryGap、恢复世代/命名空间拒绝边界及游标失效，不把旧空洞分配给新消息。

**通过条件及结论边界**

- 不复用同一身份空间确认序号；报告 HistoryGap 并安全重同步

**必要证据**

- 客户端 ACK 与备份水位对照
- 序号/发送命名空间恢复记录
- HistoryGap 与重同步响应

### REC-V1-11｜恢复时断电、满盘、损坏增量、错误密钥

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-016`、`V1-REC-018`、`V1-REC-020`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 保留原目标卷与可校验恢复集，准备损坏增量、错误密钥及容量不足夹具。

**操作、审查步骤或故障注入**

1. 在恢复各持久阶段注入断电/满盘，再分别导入坏增量和错误密钥。
2. 重启恢复器，核对检查点、旧目标可用性和已写范围，不自动将空卷视作成功。

**通过条件及结论边界**

- 保留检查点与旧目标；不假装空卷恢复成功

**必要证据**

- 故障点与恢复检查点
- 旧/新卷校验清单
- 精确错误与残留范围

### REC-V1-12｜升级到新 Schema 后回退旧宿主/服务

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-018`、`V1-UPD-003`、`V1-UPD-004`
- **阶段：** M1、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 应用数据与 SecurityState 已升级，保留不支持新 writer Schema 的旧宿主/服务制品。

**操作、审查步骤或故障注入**

1. 切换旧宿主/服务并尝试写新数据、恢复旧授权缓存或自动启动旧任务。
2. 验证兼容门槛和本地救援；不可逆数据迁移不凭 OS 回退隐式撤销。

**通过条件及结论边界**

- 拒绝不兼容写入；安全代次不回退，本地救援可用

**必要证据**

- reader/writer 兼容矩阵
- 旧程序写拒绝与安全代次
- 部署/数据版本和救援证据

### REC-V1-13｜时钟回拨、重启、休眠后恢复旧期限

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-019`、`V1-FND-006`、`V1-CLU-007`
- **阶段：** M1、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备有绝对期限、boot_id 绑定期限及计入休眠期限的三类许可。

**操作、审查步骤或故障注入**

1. 时钟回拨、长时休眠、重启后逐一尝试使用旧许可和续租。
2. 核对期限未从持久的剩余时长重新起算，时间不可证实的敏感准入停止。

**通过条件及结论边界**

- 不延长旧授权/租约；只在有效证据下恢复准入

**必要证据**

- 时钟/boot_id 与期限日志
- 许可/租约拒绝记录
- 恢复前后有效期计算

### REC-V1-14｜部分服务激活后取消恢复或另一服务失败

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-016`、`V1-REC-017`、`V1-REC-018`
- **阶段：** M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 恢复计划涉及多个服务，允许一个先激活并产生可观察受控效果。

**操作、审查步骤或故障注入**

1. 在其他服务激活失败或用户取消时中止恢复。
2. 查询各服务与效果状态，验证未把已激活服务的事实统一改成未执行。

**通过条件及结论边界**

- 报告已激活范围与残留效果，不虚报全部回滚

**必要证据**

- 逐服务激活检查点
- 取消与失败时间线
- 残留效果及后续处置记录

### REC-V1-15｜从独立介质、独立备份恢复有状态应用

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-008`、`V1-REC-021`、`V1-STO-008`、`V1-STO-009`
- **阶段：** M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 使用独立介质上的完整备份与独立恢复工具，原系统盘不可用；记录业务写入与测量起点。

**操作、审查步骤或故障注入**

1. 在目标硬件恢复有状态应用，核验内容、日志、Schema、权限和业务查询。
2. 实测数据缺口与总恢复时间，分列等待密钥、人工操作和服务重新验证所耗时间。

**通过条件及结论边界**

- 业务数据校验通过；记录真实 RPO/RTO 及密钥/人工等待范围

**必要证据**

- 独立恢复工具/介质版本
- 业务校验与数据缺口清单
- 分阶段 RPO/RTO 测量原始数据

### REC-V1-16｜备份后删除消息/数据并撤销公开共享，再恢复旧备份

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-023`、`V1-DAT-005`、`V1-CHAT-012`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 备份后删除消息/数据并撤销共享；一组保留可信删除屏障，另一组故意无法证明删除历史。

**操作、审查步骤或故障注入**

1. 分别恢复两组旧备份，重建搜索索引、共享链接和通知。
2. 验证已知删除先于发布过滤；未知删除历史保持隔离而不恢复外部可见性。

**通过条件及结论边界**

- 已知删除屏障先于发布；未知删除历史不自动恢复分享/索引可见性

**必要证据**

- 删除/保留/共享撤销记录
- 恢复后索引与公开访问负例
- 未知历史隔离判定

## AT-STA

### AT-STA-001｜卷失联、设备路径交换、恢复克隆后启动应用

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-STO-002`、`V1-STO-004`、`V1-FND-004`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两块可交换路径的测试卷具有不同稳定 ID 和可校验数据；应用只绑定其中一卷。

**操作、审查步骤或故障注入**

1. 使目标卷失联并交换设备路径，再恢复同内容克隆并启动应用。
2. 核对挂载身份与克隆授权，不允许在挂载失败处自动创建空目录后继续。

**通过条件及结论边界**

- 不挂错卷、不以空目录代替、不复用旧身份授权

**必要证据**

- 设备稳定 ID/卷身份清单
- 实际挂载与应用启动记录
- 原数据和克隆数据校验

### AT-STA-002｜写入中断电、满盘、只读、短写；文件和目录分别 fsync

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-STO-006`、`V1-TFS-003`
- **阶段：** M2、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 测试后端声明了 write/fsync/rename/父目录 fsync 的持久故障范围，拥有可重放文件发布负载。

**操作、审查步骤或故障注入**

1. 在写入、短写、文件同步、rename 和目录同步各边界断电；另行注入满盘与只读错误。
2. 用独立工具重启校验已确认内容与命名空间，按声明边界区分允许丢失和违规丢失。

**通过条件及结论边界**

- 已声明持久范围内可恢复；错误可见，未声明范围不假成功

**必要证据**

- 各同步系统调用及返回值
- 故障点/断电方式
- 重启后文件和目录校验清单

### AT-STA-003｜原系统盘丢失后恢复有状态应用；缺日志/错密钥/坏增量链

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-STO-008`、`V1-REC-008`、`V1-REC-017`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 原系统盘不可用，仅有独立备份、工具、恢复密钥及授权主体；准备缺日志和损坏增量变体。

**操作、审查步骤或故障注入**

1. 先验证完整集并恢复有状态应用；分别移除必需日志、替换密钥、破坏增量链再次恢复。
2. 恢复数据与重新激活权限分开，核对错误集未进入业务可写状态。

**通过条件及结论边界**

- 独立工具验证并拒绝不完整恢复集；激活有独立授权

**必要证据**

- 备份清单及链校验
- 独立工具业务验证输出
- 激活授权与拒绝记录

### AT-STA-004｜相同节点两个 Writer，以 RWO 附件申请排他访问

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-STO-003`、`V1-CLU-005`
- **阶段：** M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 同一节点两个进程申请 RWO 附件，二者均可尝试同一共享数据写；资源端 fence 可观察。

**操作、审查步骤或故障注入**

1. 让两个进程竞争租用并使旧租约到期，延迟旧进程退出。
2. 检查 RWO 访问模式没有被误当单进程排他锁；新 Writer 激活以前必须有有效强制证据。

**通过条件及结论边界**

- 不能据 RWO 自认单 Writer；资源 fence 阻断旧写

**必要证据**

- 附件模式与 Writer 身份
- fence 安装/拒旧证据
- 并发实际写日志

### AT-STA-005｜Dataset 大记录、null score、任意 JSON extra_data 与精确比较

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-DAT-001`、`V1-DAT-002`、`V1-DAT-003`
- **阶段：** M2、M5
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立稳定 uid 的记录集合，含超大 content、null/missing score、各类 JSON 根值和超精度整数。

**操作、审查步骤或故障注入**

1. 导入、更新、查询并导出记录，使用明确的 JSON 路径与精确比较运算。
2. 核对 uid 不变、根值类型保留、missing/null 分开，整数未被浮点舍入。

**通过条件及结论边界**

- 保留主身份/根值，missing/null/大整数语义正确

**必要证据**

- 输入/输出样本及摘要
- 精确查询期望与实际
- 记录版本和大内容引用

### AT-STA-006｜评分或索引构建时修改原文/模型/过滤定义

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-DAT-004`、`V1-API-003`
- **阶段：** M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 评分与向量计算绑定内容、模型和过滤定义版本，可暂停异步计算提交。

**操作、审查步骤或故障注入**

1. 计算开始后修改原文、模型配置或视图过滤定义，再释放旧结果提交。
2. 验证 CAS 拒绝旧写或明确保存为旧版本派生；新内容不能显示为已具备旧分数/向量。

**通过条件及结论边界**

- CAS拒旧写或保存旧版本结果，新正文不冒用旧score/vector

**必要证据**

- 派生输入版本与 digest
- CAS/旧版本存档记录
- 新旧查询结果对照

### AT-STA-007｜删除/撤权与旧索引、缓存、分页、固定View竞争

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-DAT-005`、`V1-TFS-005`、`V1-API-014`
- **阶段：** M2、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立旧索引、缓存、分页快照和固定 View；主体随后可被撤权或源记录删除。

**操作、审查步骤或故障注入**

1. 在检索、翻页和读缓存时提交删除/撤权，保持索引异步落后。
2. 测试在线结果过滤与离线残留声明，检查摘要/计数是否泄露无权记录。

**通过条件及结论边界**

- 当前权限屏障有效；离线和既有数据面残留准确报告

**必要证据**

- 删除水位/授权代次
- 各读取入口的响应
- 索引滞后与残留报告

### AT-STA-008｜FUSE 与 Catalog 并发写、mmap、rename、unlink后读

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-TFS-004`、`V1-TFS-009`
- **阶段：** M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 后端 Profile 逐项声明 FUSE、Catalog、mmap、rename 和 unlink 后句柄语义。

**操作、审查步骤或故障注入**

1. 并发经 FUSE 与 Catalog 写同对象，持有 mmap 和打开句柄执行 rename/unlink。
2. 按 Profile 验证缓存失效、版本冲突和仍打开对象行为；未支持操作返回明确错误。

**通过条件及结论边界**

- 缓存/版本符合Profile，不支持项明确失败

**必要证据**

- 并发交错脚本与版本
- 句柄/mmap 实际读写记录
- 逐项 POSIX 能力结果

### AT-STA-009｜GC扫描后创建pin/快照/新引用，同时发布内容

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-TFS-006`、`V1-API-002`
- **阶段：** M2、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立内容对象、快照/pin 引用账本与可暂停于扫描/标记/删除间的 GC。

**操作、审查步骤或故障注入**

1. GC 扫描后并发创建新 pin、快照或发布引用，在删除/提交边界崩溃。
2. 恢复后检查仍被引用对象存在且引用可核验，孤立对象符合回收规则。

**通过条件及结论边界**

- 无仍被引用对象误删，重启后可核实保留关系

**必要证据**

- GC 标记与引用代次
- 原子发布/pin 记录
- 对象存在性及内容摘要

### AT-STA-010｜非UTF-8文件名、同名智能目录、游标过期、符号链接越界

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-TFS-002`、`V1-TFS-007`、`V1-API-014`
- **阶段：** M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备非 UTF-8 名称、与智能目录同名普通条目、越界 symlink 及有限游标保留期。

**操作、审查步骤或故障注入**

1. 列举/编码/解码名称并往返导出，竞争创建同名条目；让分页游标过期后继续。
2. 检查名称可逆、授权范围与明确续传失败，不能以空页伪装完成。

**通过条件及结论边界**

- 名称可逆、授权不泄露、旧游标不假续传

**必要证据**

- 名称字节与编码向量
- 路径解析/权限负例
- 游标过期和重同步记录

### AT-STA-011｜独立导出/重导入权限、软硬链接、稀疏数据和Dataset映射

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-DAT-006`、`V1-TFS-006`
- **阶段：** M2、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 数据集中包含权限、软硬链接、稀疏文件和 Dataset 映射；准备不依赖原 daemon 的导出工具。

**操作、审查步骤或故障注入**

1. 导出后关闭原服务，在独立环境重导入并运行同一内容/元数据校验清单。
2. 逐项报告目标不支持的权限或链接语义，不静默丢失。

**通过条件及结论边界**

- 同一校验清单可复核；损失项明确；不依赖原daemon

**必要证据**

- 导出 manifest 与工具版本
- 独立导入验证清单
- 损失/转换项及授权映射

### AT-STA-012｜单机/集群分区旧Writer复活与新恢复世代

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-TFS-008`、`V1-CLU-005`、`V1-REC-013`
- **阶段：** M4、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立双候选 Writer、可网络分区的控制面与资源端，保存旧恢复世代。

**操作、审查步骤或故障注入**

1. 使旧 Writer 失联但继续运行，恢复控制面为新世代后尝试启用新 Writer。
2. 分别测试资源端支持与不支持可靠 fence，检查实际双写风险。

**通过条件及结论边界**

- 无双写；能力不足保持RecoveryRequired

**必要证据**

- 资源端写入与 fence 日志
- 恢复 incarnation/epoch
- RecoveryRequired 或成功转交证据

### AT-STA-013｜OCI只读根、run/tmp挂载、UID映射、缺必需限制

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-002`、`V1-APP-003`
- **阶段：** M2
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 应用清单要求只读根、指定 run/tmp 写点、明确 UID 映射和一项后端可缺失的必需限制。

**操作、审查步骤或故障注入**

1. 运行应用尝试写根、宿主路径、未授予卷和声明写点；再用缺失必需限制的后端启动。
2. 核对实际 namespace/UID/挂载和拒启条件。

**通过条件及结论边界**

- 只给声明的写点，不扩大宿主权限，能力不足拒启

**必要证据**

- OCI/运行时有效配置
- 实际写权限负例
- 能力不足拒启记录

### AT-STA-014｜安装/升级各步骤中断，旧迁移仍运行时锁到期

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-005`、`V1-APP-006`、`V1-CLU-005`
- **阶段：** M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 应用升级包含可暂停的持久数据迁移，旧迁移进程能在控制面租约到期后存活。

**操作、审查步骤或故障注入**

1. 在安装/升级提交点逐个中断，恢复调度器并使旧迁移继续持有写通道。
2. 无可靠 fence 时等待恢复；有 fence 时确认旧写被拒后仅运行一个迁移。

**通过条件及结论边界**

- 无双迁移和误删卷；fence或显式等待恢复

**必要证据**

- 生命周期/迁移检查点
- 旧新迁移进程与写证据
- 卷身份和未误删验证

### AT-STA-015｜新权限更新、不可逆Schema迁移失败、卸载重装

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-002`、`V1-APP-006`、`V1-APP-007`、`V1-UPD-003`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 旧应用获有限权限，新版本申请更多权限并含不可逆 Schema 迁移；卷由独立用户数据生命周期管理。

**操作、审查步骤或故障注入**

1. 升级时不批准新增权限并注入迁移失败，再执行应用回退、卸载与重装。
2. 核对权限未自动扩张、旧 writer 不破坏新数据、卸载默认保留卷。

**通过条件及结论边界**

- 不沿用扩权；回退不损数据；默认保留卷

**必要证据**

- 新旧权限差异与决策
- 数据兼容/恢复记录
- 卷保留和重装绑定

### AT-STA-016｜DEB/RPM/Alpine/Android混淆、ABI错、离线缺依赖

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-008`、`V1-APP-011`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备各目标架构/发行环境的 DEB、RPM、Alpine 和 Android 代表样本及离线依赖缺失夹具。

**操作、审查步骤或故障注入**

1. 分别导入到匹配与不匹配环境，验证格式、ABI 和依赖错误。
2. 核对实际兼容等级和宿主包库变化，不允许修改宿主公共依赖凑成功。

**通过条件及结论边界**

- 准确拒绝/声明兼容等级；不污染宿主包库

**必要证据**

- 样本摘要与目标矩阵
- 兼容拒绝/运行记录
- 宿主包库前后差异

### AT-STA-017｜解压路径穿越、恶意symlink、tar bomb、脚本外连

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-009`、`V1-SEC-011`、`V1-NET-003`
- **阶段：** M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 隔离解析/构建环境设字节、文件数、CPU 和网络上限，包含虚构敏感数据诱饵。

**操作、审查步骤或故障注入**

1. 导入路径穿越、恶意 symlink、解压炸弹和安装脚本外连样本。
2. 观察解析阶段及安装器的实际写目标、资源消耗和出站流量。

**通过条件及结论边界**

- 解析阶段即隔离；有界失败；无宿主写/秘密外泄

**必要证据**

- 恶意样本摘要
- 资源上限/终止与网络记录
- 宿主目录和秘密诱饵未泄露断言

### AT-STA-018｜依赖源变化、基础层更新、安全修复重建与GC竞争

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-010`、`V1-REL-001`、`V1-TFS-006`
- **阶段：** M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立锁定依赖来源的制品与共享基础层，至少一个旧发布仍被应用引用。

**操作、审查步骤或故障注入**

1. 更改依赖源/基础层并发布安全修复重建，同时执行 GC。
2. 核对新字节形成新摘要，旧引用层仍在，来源与 SBOM 可追溯。

**通过条件及结论边界**

- 来源锁可复核，新摘要不覆盖旧发布，引用层不误删

**必要证据**

- 来源锁/SBOM/重建参数
- 新旧制品 digest
- 引用账本与 GC 结果

### AT-STA-019｜单窗口可捕获但键盘可跨窗；只读Viewer发输入

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-001`、`V1-GUI-002`
- **阶段：** M3、M4
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 后端仅能限定单窗捕获但输入可作用整个会话，另有仅观察 Viewer。

**操作、审查步骤或故障注入**

1. 申请 SingleApplication 控制并尝试键盘切窗，再从只读 Viewer 注入输入。
2. 核对实际授权范围声明与拒绝行为，不能用窗口裁剪伪装输入隔离。

**通过条件及结论边界**

- 不能声明虚假SingleApplication或观察者写入

**必要证据**

- 捕获/输入范围矩阵
- 真实焦点和输入目标记录
- 只读 Viewer 拒绝证据

### AT-STA-020｜两远程输入者、键按下后断网/接管、IME未提交

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-003`、`V1-SEC-019`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两远程控制端竞争同一输入租约，存在按下未释放的键和未提交 IME 组合状态。

**操作、审查步骤或故障注入**

1. 在按键/IME 中途断网并由第二控制端或本地用户接管，再重连旧控制端。
2. 验证旧输入序列被丢弃，释放/清理动作不成为新用户的意外键入。

**通过条件及结论边界**

- 单一租约；旧输入丢弃；安全清理无新用户误输入

**必要证据**

- 输入租约/序列/代次
- 接管与重连事件轨迹
- 实际应用文本/按键状态

### AT-STA-021｜锁屏/切用户/缩放/热插拔/PipeWire重启及重连

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-004`、`V1-GUI-010`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 配置多屏混合缩放桌面、稳定会话身份与可重启 PipeWire；应用生命周期独立于 Viewer。

**操作、审查步骤或故障注入**

1. 依次锁屏、切用户、改缩放、热插拔、重启 PipeWire 并重连 Viewer。
2. 用旧布局坐标、node ID、serial 和输入租约提交动作，核对被重新验证或拒绝。

**通过条件及结论边界**

- 旧布局/ID/serial/租约不误用，应用与Viewer寿命分离

**必要证据**

- 布局/流 serial/会话代次
- 旧输入拒绝与重新授权记录
- 应用与 Viewer 生命周期对照

### AT-STA-022｜Portal v4/v5/v6、RemoteDesktop组合、EIS/Notify

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-005`
- **阶段：** M3、M4
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备实际支持不同 Portal 版本与输入路径的后端矩阵，记录桌面/Portal/PipeWire 版本。

**操作、审查步骤或故障注入**

1. 测试 ScreenCast v4/v5/v6 与 RemoteDesktop 的实际组合及支持的 EIS 或 Notify 输入路径。
2. 验证仅声明可用能力，持久化范围按组合真实语义处理，不能混用输入通路。

**通过条件及结论边界**

- 能力准确降级，组合持久化正确，输入通路不混用

**必要证据**

- D-Bus 能力与版本探测
- 各组合调用/响应记录
- 降级和明确拒绝矩阵

### AT-STA-023｜restore_token消费后响应或新令牌写入丢失

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-006`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备可消费一次的 restore_token 与持久令牌保存机制，不把令牌明文放入证据。

**操作、审查步骤或故障注入**

1. 在上游消费 token 后丢响应，以及收到新 token 但本地未提交时崩溃。
2. 恢复后尝试旧 token，核对不盲目重复消费，残留会话可查询或进入用户重新选择。

**通过条件及结论边界**

- 不盲复用旧token；进入可解释等待，残留会话可核实

**必要证据**

- 脱敏 token 世代与消费时间线
- 崩溃点与本地持久记录
- 恢复等待/新会话核实证据

### AT-STA-024｜NativeWeb恶意脚本、Cookie、postMessage和代理SSRF

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-008`、`V1-SEC-015`、`V1-NET-003`
- **阶段：** M3、M4
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- NativeWeb 应用和管理页使用隔离 origin，配置受限代理、Cookie 和 postMessage 来源策略。

**操作、审查步骤或故障注入**

1. 在应用页执行恶意脚本，尝试读管理 Cookie、跨 origin 发消息并令代理访问内网/元数据地址。
2. 测试合法应用交互作为对照，核对代理权限与管理会话未串用。

**通过条件及结论边界**

- 管理origin隔离，凭据不串应用，代理不出范围

**必要证据**

- origin/Cookie/消息策略
- 浏览器及代理请求记录
- SSRF 与管理操作负例

### AT-STA-025｜仅授权播放但后端同时开放麦克风；未授权剪贴板

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-002`、`V1-GUI-009`
- **阶段：** M3、M4
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 用户只批准播放/捕获中的有限范围，后端实际可能将麦克风或剪贴板与之组合授予。

**操作、审查步骤或故障注入**

1. 请求只播放而调用组合能力；尝试未授权剪贴板与麦克风读取。
2. 无法分离范围时准确拒绝或要求针对真实范围授权，检查 UI 与底层权限一致。

**通过条件及结论边界**

- 如实拒绝/重新授权真实范围，不能用UI虚构隔离

**必要证据**

- 用户批准范围与后端能力
- 实际媒体/剪贴板数据面观察
- 拒绝或扩范围授权证据

### AT-STA-026｜编码器/显存/带宽耗尽、排队输入与流撤权

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-GUI-007`、`V1-DEV-002`、`V1-SEC-018`
- **阶段：** M3、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 为编码器、显存、带宽、排队输入与宿主救援各配置可观察预算。

**操作、审查步骤或故障注入**

1. 耗尽流资源并注入网络积压，在积压时撤销流和输入权限，再恢复带宽。
2. 验证有界退化、队列清理和不重放撤权前输入，独立诊断保持可达。

**通过条件及结论边界**

- 有界退化，停止后不重放，宿主救援保持可用

**必要证据**

- 资源/队列时序与限制配置
- 撤权后输入拒绝与流终止
- 本机救援操作记录

## AT-TSK

### AT-TSK-001｜提交原子性

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-002`、`V1-TSK-003`、`V1-API-012`
- **阶段：** M0、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Broker 使用可注入故障的持久库，Task 输入清单、pin、配额与 outbox 可分别观测。

**操作、审查步骤或故障注入**

1. 在容量预留、去重记录、Task 记录和 outbox 的提交边界逐一杀进程。
2. 重启后从权威查询接受状态与引用，不依赖客户端缓存。

**通过条件及结论边界**

- 在容量预留、Task、去重记录和 outbox 各边界杀进程；不能出现接受成功却没有可恢复 Task

**必要证据**

- 事务故障点/数据库恢复日志
- Accepted 回执与权威 Task 对照
- 容量/pin/outbox 一致性检查

### AT-TSK-002｜ACK 丢失

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-004`、`V1-TSK-005`、`V1-WFL-004`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Workflow 已提交稳定 DispatchIntent，Broker 能保存任务但模拟网络可丢回执。

**操作、审查步骤或故障注入**

1. 在 Broker 接受后丢 ACK，并分别重传、重启 Workflow 和使 Task 链接保存失败。
2. 用原 origin_ref、键、摘要和代次查询后链接任务。

**通过条件及结论边界**

- Broker 接受后丢回执；重传、查询与 Workflow 重启均链接同一 Task

**必要证据**

- 三条故障时序
- 稳定提交身份和请求摘要
- 唯一 Task 与链接记录

### AT-TSK-003｜过期防重放

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-004`、`V1-API-011`、`V1-REC-012`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 已执行 Task 的正文和结果可独立回收，提交命名空间有明确期限。

**操作、审查步骤或故障注入**

1. 清理结果和去重明细，过期提交窗口，再从旧副本查询并重放旧键。
2. 检查窗口拒绝边界和 Indeterminate/Expired 的处理，不产生新业务效果。

**通过条件及结论边界**

- 回收任务结果、过期去重窗口、恢复旧副本；旧键不得被当作新工作

**必要证据**

- 保留策略与 GC 记录
- 查询/重放响应
- 效果端唯一计数

### AT-TSK-004｜取消竞争

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-006`、`V1-API-013`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Broker 提交与 CancelOrigin 共用权威串行化点，派发器可暂停于发送前后。

**操作、审查步骤或故障注入**

1. 安排取消先于提交、提交先于取消、取消持久后网络中旧提交到达三个交错。
2. 丢失两侧回执后重启协调者并对账全部来源键。

**通过条件及结论边界**

- Submit 与 CancelOrigin 两种顺序交错；要么 Task 被取消，要么提交被墓碑拒绝

**必要证据**

- 提交/取消串行化顺序
- 取消墓碑和 Task 状态
- 无未跟踪任务的查询清单

### AT-TSK-005｜迟到结果

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-009`、`V1-CLU-004`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Task 有被替换旧 Run 与当前新 Run，旧 Run 持有可验证远端成功回执。

**操作、审查步骤或故障注入**

1. 新 Run 领取后提交旧 Run 的状态更新与效果回执，并尝试释放共享预留。
2. 验证当前状态/新资源不受旧提交影响，效果证据进入独立对账。

**通过条件及结论边界**

- 旧 Run 在新 Run 领取后返回外部成功证据；当前状态不被覆盖，效果事实进入对账

**必要证据**

- 新旧 claim/Run 代次
- 迟到结果拒绝及证据记录
- 资源预留所有权

### AT-TSK-006｜未知效果

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-007`、`V1-TSK-008`、`V1-API-009`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 外部测试 API 可在提交写成功后断开响应；没有可靠业务幂等保证。

**操作、审查步骤或故障注入**

1. 发写请求、丢回执并取消 Task，查询 lifecycle 与 outcome_knowledge。
2. 尝试普通失败重试与人工换键，确认均不能绕过未知效果核实。

**通过条件及结论边界**

- 发出写请求后断网并取消；查询保留 Unknown，不能显示全量回滚或自动重试

**必要证据**

- 外部真实写入与本地超时
- 正交状态查询
- 重试拒绝和核实路径

### AT-TSK-007｜事件恢复

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-011`、`V1-TSK-012`、`V1-TSK-013`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 事件系统具备有限历史、durable inbox/outbox、处理器版本和隔离队列。

**操作、审查步骤或故障注入**

1. 注入重复、乱序、毒消息、游标过期和消费者升级，在业务/消费提交边界重启。
2. 完整重新列表后接续事件，核对关键终态、删除事实与外部效果数量。

**通过条件及结论边界**

- 重复、乱序、毒消息、游标过期和消费者版本变化；不丢权威终态，不重复效果

**必要证据**

- 事件 ID/游标和 inbox 记录
- 死信及重放授权
- 权威对象与消费者投影差异

### AT-TSK-008｜有界资源

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-003`、`V1-TSK-013`、`V1-OPS-004`
- **阶段：** M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 分别配置任务数、结果字节、日志、审计和救援保留配额。

**操作、审查步骤或故障注入**

1. 逐项填满容量并并发提交任务，再让已接受 Task 达到终态。
2. 检查准确拒绝新工作且已接受必要状态仍能提交，本机诊断可用。

**通过条件及结论边界**

- 队列、结果、日志和审计分别满额；准确拒绝且保留本地诊断/救援资源

**必要证据**

- 分类容量/预留曲线
- 拒绝码与终态落盘证据
- 救援资源测量

### AT-TSK-009｜定时与预算

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-014`、`V1-TSK-015`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Schedule 指定 IANA 时区、DST、misfire 和重叠政策；父预算只够部分并行任务。

**操作、审查步骤或故障注入**

1. 跨 DST 重复/缺失时刻触发，长停机后补跑，并让子任务并发争抢最后余额。
2. 核对 trigger_id 与 Schedule 修订、补跑上限和预算预留，不因重启重置额度。

**通过条件及结论边界**

- DST 双时刻、长停机补跑、并行子任务争用最后预算；触发唯一、补跑有界

**必要证据**

- 触发逻辑时刻和稳定 ID
- 停机补跑决策
- 预算树/预留/结算账本

## AT-AGT

### AT-AGT-001｜第 62 步后崩溃

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-002`、`V1-TSK-008`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 构造至少 63 个可观察步骤的 Agent Task，已完成效果有独立 Operation 与外部计数。

**操作、审查步骤或故障注入**

1. 在第 62 步确认后、下一步检查点前崩溃，恢复同版本，再尝试不兼容运行版本恢复。
2. 核对已完成效果不再发出，不兼容状态不被原地继续。

**通过条件及结论边界**

- 只恢复未确认工作，已完成效果不重复，版本不兼容明确阻止

**必要证据**

- 执行清单和检查点
- 工具/Operation 时间线
- 版本兼容拒绝与外部计数

### AT-AGT-002｜Skill 更新替换工具

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-002`、`V1-AGT-004`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 已接受 Run 固定 Skill/Tool 摘要，同时准备申请更大权限的新 Skill 版本。

**操作、审查步骤或故障注入**

1. 替换注册表默认 Skill 和工具实现，恢复旧 Run，并尝试通过更新继承新增能力。
2. 验证继续固定原版本或显式迁移，Grant 不随文本更新扩张。

**通过条件及结论边界**

- 已接受 Run 仍绑定原版本或经过显式迁移；权限不扩大

**必要证据**

- 新旧 Skill/Tool 摘要
- Run 固定引用/迁移记录
- 权限差异与拒绝

### AT-AGT-003｜本地模型失败

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-006`、`V1-SEC-012`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 任务包含 local_only 数据，路由同时配置本地和云模型，但未授权云端传输该数据。

**操作、审查步骤或故障注入**

1. 使本地模型失效并触发路由回退；监测出口和模型调用参数。
2. 再测试预先授权同范围备选模型，核对路由能在授权边界内继续。

**通过条件及结论边界**

- local_only 输入不会通过自动回退上传到云端

**必要证据**

- 数据策略与候选路由
- 出站请求观测
- 暂停原因与合法备选对照

### AT-AGT-004｜双协议时代

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-008`、`V1-AGT-009`、`V1-AGT-010`
- **阶段：** M0、M3
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 部署分别固定 MCP 2026-07-28 与 2025-11-25 的测试适配器，记录传输/认证/方法能力。

**操作、审查步骤或故障注入**

1. 分别发送现代逐请求元信息、legacy 初始化与错代字段；现代直接请求和 discover 都测试。
2. 核对不支持能力明确拒绝，旧会话、路由头和客户端自报信息不成为授权。

**通过条件及结论边界**

- 新旧握手、元信息、Tasks 方法分别成功或准确拒绝，身份不串用

**必要证据**

- 协议版本/方法矩阵
- 双代际 wire 记录（脱敏）
- 认证与错误映射

### AT-AGT-005｜MCP completed + isError

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-011`、`V1-WFL-005`
- **阶段：** M3
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 远端 Tasks 测试服务可返回 completed 且 result.isError=true；下游配置仅成功后执行的写步骤。

**操作、审查步骤或故障注入**

1. 注入该结果并重投通知，查询本地 Task 与 Workflow 后继。
2. 核对没有因为协议结束或 HTTP 成功释放下游效果。

**通过条件及结论边界**

- 映射为业务失败，不能触发“成功后合并”等下游步骤

**必要证据**

- 远端响应样本摘要
- 本地状态映射
- 后继未派发证据

### AT-AGT-006｜A2A ACK 丢失

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-012`、`V1-TSK-008`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- A2A 测试实现不提供 Send Message 幂等；发送可产生可观察效果且回执可被丢弃。

**操作、审查步骤或故障注入**

1. 发送后丢 ACK，触发适配器重连/重启与查询恢复。
2. 检查没有仅凭同 messageId 自动重发，结果不可查时进入待核实。

**通过条件及结论边界**

- 无经过证明的发送幂等时不盲重发；外部 Task 可对账

**必要证据**

- 适配器能力声明
- A2A 请求/查询轨迹
- 外部效果数量及 Unknown

### AT-AGT-007｜自报 clientInfo 冒名

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-009`、`V1-API-017`
- **阶段：** M0、M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两套真实认证身份权限不同，现代 MCP 请求允许提供 clientInfo 等自报元信息。

**操作、审查步骤或故障注入**

1. 低权限身份把自报名称改为管理员或另一 Agent，再请求敏感工具。
2. 比对实际 Principal 与审计，确保信息字段不改变授权。

**通过条件及结论边界**

- Principal 仍由认证通道建立，伪造信息无授权效果

**必要证据**

- 认证主体与元信息对照
- 工具拒绝与审计
- 合法管理员请求对照

### AT-AGT-008｜云模型与上下文恢复

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-002`、`V1-AGT-006`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 云模型仅提供可变别名/公开信息，没有不可变权重摘要；检查点记录该证据限制。

**操作、审查步骤或故障注入**

1. 在提供者可能更新模型的两个时间点恢复上下文，并切换能力不兼容模型。
2. 验证 ProviderReported 和兼容判断，不声称确定复现或虚构权重版本。

**通过条件及结论边界**

- 公开修订不完整时如实声明可复现性限制，不虚构完全相同运行

**必要证据**

- 提供者实际返回信息
- Run 模型清单与 assurance
- 兼容拒绝/重计划记录

## AT-CHAT

### AT-CHAT-001｜同键重发且 ACK 丢失

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-004`、`V1-CHAT-005`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 活动发送命名空间内准备稳定 client_send_id 与完整请求摘要。

**操作、审查步骤或故障注入**

1. 消息提交后丢 Accepted，重发原请求；随后保留同键改变正文、mentions 或附件版本。
2. 核对原 message_id、incarnation、seq 和摘要，冲突请求不能改写消息。

**通过条件及结论边界**

- 同一个 message_id/seq；不同摘要冲突

**必要证据**

- 发送请求及摘要
- 持久去重映射/回执
- 冲突响应与消息修订

### AT-CHAT-002｜正文清除后重放

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-005`、`V1-CHAT-012`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 消息正文已清除，独立防重放边界仍有效；另准备已过期发送命名空间。

**操作、审查步骤或故障注入**

1. 重发被清理消息的旧键，并伪造客户端时间或恢复旧本地发送缓存。
2. 检查返回合规墓碑或过期拒绝，没有新 message_id。

**通过条件及结论边界**

- 旧发送命名空间拒绝或返回合规墓碑，不创建新消息

**必要证据**

- 删除和窗口边界
- 重放请求/响应
- 消息总数与身份清单

### AT-CHAT-003｜发言与移除成员并发

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-002`、`V1-CHAT-003`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 成员处于 Active，消息与成员操作进入同一权威日志，发送事务可暂停。

**操作、审查步骤或故障注入**

1. 在发言提交点前后分别移除成员，延迟投递与正文读取。
2. 按实际序列检查发送接受/拒绝，并验证撤权阻止后续读取及附件下载。

**通过条件及结论边界**

- 依权威顺序一致裁决；提交后撤权阻止后续访问

**必要证据**

- 成员/消息提交序列
- membership_revision
- 读取与投递拒绝

### AT-CHAT-004｜两 Writer/网络分区

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-006`、`V1-REC-013`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两 Writer 竞争同一 Conversation；一侧可与权威存储分区，但保留旧 epoch。

**操作、审查步骤或故障注入**

1. 两侧同时发送并尝试分配序号，随后安装新 fence、切换 Writer 并恢复网络。
2. 检查旧写者提交被拒、分区侧仅 PendingLocal，不能用本地 ACK 触发 Task。

**通过条件及结论边界**

- 旧 epoch 在存储端拒绝；分区侧只能 PendingLocal

**必要证据**

- 存储端 epoch 检查
- 消息提交与 PendingLocal
- 正式 Task 派发清单

### AT-CHAT-005｜收到高序号且中间丢通知

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-008`、`V1-API-014`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 接收者有可见消息范围与 durable 通知游标，通知可按指定 seq 丢失或重排。

**操作、审查步骤或故障注入**

1. 跳过中间通知先送高序号，提交 ReadCursor/Processed 并重连拉取缺口。
2. 确认读/处理游标不越过未确认可见区间，重复投递不重复效果。

**通过条件及结论边界**

- 可恢复拉取，不能误报此前全部已读/已处理

**必要证据**

- 通知和游标序列
- 缺口拉取/重同步记录
- 处理去重与 Task 引用

### AT-CHAT-006｜DM 邀请第三人

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-001`、`V1-CHAT-002`
- **阶段：** M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立两个主体的 DM，包含第三主体无权读取的历史与附件。

**操作、审查步骤或故障注入**

1. 邀请第三主体并尝试直接修改 DM 成员集合，随后按授权建立新 Room。
2. 核对原 DM 不扩员，新 Room 仅包含显式批准的历史引用。

**通过条件及结论边界**

- 新 Room 与明确的历史共享检查；原 DM 不自动扩员

**必要证据**

- 会话身份/成员差异
- 历史共享授权
- 第三主体读取负例

### AT-CHAT-007｜附件上传完成、消息失败

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-004`、`V1-CHAT-011`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 上传服务与消息库分离，附件具有暂存、校验和保留凭据；GC 可暂停。

**操作、审查步骤或故障注入**

1. 附件完成后令消息提交失败，再令消息成功后丢 pin 确认；与孤立回收并发。
2. 验证失败消息不显示 Delivered，未引用对象有界回收，已引用对象不会误删。

**通过条件及结论边界**

- 不产生假送达；孤立保留有界回收，已引用附件不误删

**必要证据**

- 附件状态/pin 账本
- 消息事务与投递记录
- GC 后引用完整性

### AT-CHAT-008｜退出再加入

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-002`、`V1-CHAT-008`
- **阶段：** M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 主体有旧 Membership、未处理请求和 ReadCursor；新邀请采用更窄历史政策。

**操作、审查步骤或故障注入**

1. 退出后重新加入，重放旧 delivery receipt、旧唤醒和读取游标。
2. 核对新成员实例独立，旧 inbox 不自动变成新授权工作。

**通过条件及结论边界**

- 新 membership 实例，不复活旧授权、旧唤醒或旧读取游标

**必要证据**

- 新旧 membership_id/范围
- 投递/处理去重键
- 旧历史访问拒绝

### AT-CHAT-009｜递归 @ 与桥接回流

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-009`、`V1-CHAT-010`、`V1-AGT-007`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- A/B 两 Agent、跨平台桥接和有限根预算/跳数/自动轮次均可追踪。

**操作、审查步骤或故障注入**

1. 产生 A→B→A 的互相提及和桥接回流，中途由用户暂停主动发言。
2. 检查同根因果链计数、预算结算与停止后新派发数量；既有 Task 另按取消政策处理。

**通过条件及结论边界**

- 同根预算与跳数限制，暂停后不再自动新建派发

**必要证据**

- 因果链与 root_causation_id
- 预算/速率/跳数记录
- 暂停后派发及既有 Task 状态

### AT-CHAT-010｜全盘损毁与旧备份

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-007`、`V1-REC-015`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 客户端已保存高于备份水位的 Accepted；模拟唯一日志介质全损后仅有旧备份。

**操作、审查步骤或故障注入**

1. 恢复并重建日志身份空间，重连旧 Writer 和客户端旧游标。
2. 验证新 incarnation、生效 fence、HistoryGap 和 RPO 报告；序号不跨 incarnation 排序。

**通过条件及结论边界**

- 按声明 RPO 报告缺口，旧序号/命名空间不被无检查重启

**必要证据**

- 旧 ACK/备份/新世代对照
- 资源端拒旧与游标失效
- 缺口/RPO 说明及重同步

## AT-WFL

### AT-WFL-001｜动态展开后引擎崩溃

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-002`、`V1-WFL-004`、`V1-WFL-010`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Map/BoundedLoop 已持久展开部分步骤，外部来源列表可在运行期间改变。

**操作、审查步骤或故障注入**

1. 在展开提交后引擎崩溃，修改外部列表后恢复运行。
2. 核对继续使用原输入清单、迭代路径和 step_key，不按新列表重复派发。

**通过条件及结论边界**

- 使用原输入快照与步骤键恢复，不按新列表重复展开

**必要证据**

- Definition/输入快照摘要
- 展开实例与 DispatchIntent
- 重启前后步骤身份对照

### AT-WFL-002｜未选分支与 Join

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-003`、`V1-WFL-005`
- **阶段：** M0、M3
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 定义含 Condition、未选分支和 AllSuccess/AllSettled Join，另有必需前驱可失败。

**操作、审查步骤或故障注入**

1. 逐一覆盖分支被跳过、必需前驱失败、重复终态和永不满足依赖。
2. 检查 Skipped 仅代表明确不执行，Join 不因通知计数凑齐而错误成功。

**通过条件及结论边界**

- Skipped 明确不参与；必需失败不能被跳过假装成功

**必要证据**

- 分支持久判定
- Join 输入状态/版本
- 失败/超时诊断

### AT-WFL-003｜AnySuccess 败者仍在写

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-005`、`V1-WFL-006`、`V1-TSK-007`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- AnySuccess 并行分支都可能产生远端写；一个先成功，一个回执可延迟或丢失。

**操作、审查步骤或故障注入**

1. 首分支成功后保持败者运行，再丢其回执；尝试释放要求单一效果的后继。
2. 只有核实、授权接受多效果或完成补偿条件后才推进。

**通过条件及结论边界**

- 先核实/取消/补偿败者，不能声称只有一个效果

**必要证据**

- 获胜者选择记录
- 败者 Operation/Unknown
- 后继阻塞与补偿证据

### AT-WFL-004｜审批后计划或 head 改变

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-007`、`V1-WFL-014`、`V1-SEC-009`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 门禁批准绑定固定计划、制品与 head SHA，状态心跳可单独更新。

**操作、审查步骤或故障注入**

1. 分别修改目标/计划与仅修改心跳后复用审批执行。
2. 核对效果变化拒绝，非效果变化不产生无意义再审批；重审也使用原权威对象。

**通过条件及结论边界**

- 原决策/许可不能执行新效果；非效果心跳不误使其失效

**必要证据**

- 计划/效果摘要对照
- ApprovalDecision/Permit 查询
- 拒绝与合法继续证据

### AT-WFL-005｜派发与取消交错

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-004`、`V1-WFL-008`、`V1-TSK-006`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 步骤处于 Dispatching，Broker 端接受结果未知且网络可交错取消。

**操作、审查步骤或故障注入**

1. 在所有派发/取消持久边界注入丢 ACK 或重启，并对原来源键查询。
2. 验证 CancelOrigin 覆盖未分配 task_id 的情况，不增加 attempt_generation 或换键漏出工作。

**通过条件及结论边界**

- CancelOrigin 与提交权威去重，未知提交不生成新键

**必要证据**

- 跨服务时序
- 来源键/代次和取消墓碑
- Task 链接与取消结果

### AT-WFL-006｜补偿失败或超时

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-009`、`V1-TSK-008`、`V1-API-009`
- **阶段：** M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 原流程已产生两个可补偿效果和一个不可逆效果，补偿也有自己的 Operation。

**操作、审查步骤或故障注入**

1. 令一个补偿成功、一个超时或失败，再取消/恢复 Workflow。
2. 检查残留清单、compensation_status 和 outcome_knowledge，原效果历史不删除。

**通过条件及结论边界**

- 保留残留项与 Unknown，禁止写“完整回滚”

**必要证据**

- 原效果与补偿关联
- 独立补偿授权/回执
- 残留与 Unknown 查询

### AT-WFL-007｜外部 ETag/Git SHA 乱序

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-012`、`V1-WFL-013`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 连接器声明 OpaqueETag 或 CommitGraph 修订语义，提供随机 ETag、分叉提交和可乱序 Webhook。

**操作、审查步骤或故障注入**

1. 按与字典序相反的真实版本顺序投递事件，并模拟漏投与人工修改。
2. 回源查询后处理冲突，检验未按字符串大小或通知到达顺序覆盖现状。

**通过条件及结论边界**

- 不使用字典序判断最新；回源权威读取与条件写

**必要证据**

- 外部权威读取与修订类型
- Webhook 重排夹具
- 同步/冲突账本

### AT-WFL-008｜CI 成功但 head 已变化

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-014`、`V1-API-004`
- **阶段：** M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- PR 的 CI/Review 已对 head A 成功，平台可提供 head 条件合并或明确不支持。

**操作、审查步骤或故障注入**

1. 门禁读取 A 后把 PR 推进到 B，再执行原合并动作。
2. 验证原证据无法授权 B；有条件写时被拒，无等效强制机制时转受限流程而非声称原子。

**通过条件及结论边界**

- 后续合并阻止，旧证据不冒充新提交通过

**必要证据**

- CI/Review 运行身份与 head
- 竞争修改和条件写响应
- 合并/阻塞事实

### AT-WFL-009｜工作流/Task 备份不在同一恢复点

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-010`、`V1-REC-008`、`V1-REC-011`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Workflow 备份早于某 Task Accepted，审批与外部效果记录也可有不同水位。

**操作、审查步骤或故障注入**

1. 恢复不一致恢复集并重启引擎，保持跨服务效果屏障。
2. 先对账原 origin/Task/Operation，再修复引用；缺记录不能直接重新派发。

**通过条件及结论边界**

- 对账屏障阻止重派发，恢复引用并保留未知效果

**必要证据**

- 恢复清单和组件水位
- 对账及引用修复记录
- 未重复派发证明

### AT-WFL-010｜未安装 Project/Studio

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-001`、`V1-BASE-001`、`V1-ARCH-004`
- **阶段：** M3
- **验证方式：** Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 安装三版声明的最小 Core/Agent/Chat/Workflow，故意不安装 Project Service 与 Studio。

**操作、审查步骤或故障注入**

1. 通过 CLI/API 执行本地 Task、最小工作流和群聊/DM，分别调用未安装 Project 接口。
2. 断开中心服务后继续本地获准工作，核对错误码区分未安装与依赖故障。

**通过条件及结论边界**

- 本地 Task、Agent、最小工作流与 Chat 仍正常可用

**必要证据**

- 三版/Profile 安装清单
- 基本用户路径记录
- 未安装/失效响应和本地独立运行

## AT-CORE

### AT-CORE-001｜重复消息跨 Dispatcher、Task 与 Operation 到达

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-API-010`、`V1-TSK-005`、`V1-TSK-011`、`V1-CHAT-009`
- **阶段：** M0、M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 同一安全域内准备消息触发的 Workflow、Task 和可计数外部测试写操作；跨服务均可丢 ACK 或重复投递。

**操作、审查步骤或故障注入**

1. 在消息接受、派发接受、Operation 回执及消费提交处逐一丢 ACK；重启所有消费者后重复发送同一逻辑链。
2. 再以相同字符串键从另一安全域发送合法独立工作，并在原域修改请求摘要重发。

**通过条件及结论边界**

- 同一逻辑链只产生一个被接受的 Task 与一个可确认业务效果；重复通知可存在，但不重复消费审批或预算。
- 跨域工作不串用身份；同键异摘要返回冲突；查询仍受当前权限限制。

**必要证据**

- 消息/client_send_id、origin_ref、task_id、operation_id 关联表
- 事务边界故障时间线
- 外部效果计数、审批与预算账本

### AT-CORE-002｜派发取消与节点关机的并发顺序

- **主规范：** [docs/08-tasks-events.md](../docs/08-tasks-events.md)
- **要求：** `V1-TSK-006`、`V1-TSK-016`、`V1-DEV-004`、`V1-OPS-003`
- **阶段：** M0、M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 某步骤已写 DispatchIntent，旧 Worker 可持有写资源；取消、outbox 发送和节点关机均可由屏障交错。

**操作、审查步骤或故障注入**

1. 先分别使取消早于/晚于 Broker 接受，再在节点停止流程中保留旧 Worker 写入通道。
2. 延迟执行停止或 fence 确认，观察是否提前释放可触发替代调度的租约；最终重连对账。

**通过条件及结论边界**

- 取消墓碑或原 Task 取消可追踪，不漏出迟到任务。
- 旧环境未安全停止/fence 之前不激活替代 Writer；不能用关机请求或 kill 返回值宣称业务回滚。

**必要证据**

- 取消域与来源键状态
- 关机/Drain 各步骤时间线
- 资源端 fence 和实际写入记录

### AT-CORE-003｜未知外部效果跨取消、恢复和人工重试保留

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-010`、`V1-REC-011`、`V1-TSK-007`、`V1-WFL-008`、`V1-CLU-006`
- **阶段：** M2、M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 外部写已可能成功但回执不可用，备份记录早于最后确认；普通自动重试与人工重试入口都可调用。

**操作、审查步骤或故障注入**

1. 取消 Task 后恢复旧 Workflow 快照，再尝试自动补跑和人工更换幂等键。
2. 提供后来获取的可信外部成功证据并进行对账，验证只是更新知识与后续计划。
3. 在远端测试端已接受非幂等动作但回执丢失后，对共享卷安装新 fence 并隔离旧 Worker；随后尝试由新 Run 重发同类远端动作，区分卷写权与外部效果。

**通过条件及结论边界**

- Failed/Cancelled 的生命周期可以与 Unknown 同时可见，依赖保持阻塞。
- 新键、新 Task 或新 Run 不能抹去旧效果风险；迟到证据被保存，终止历史不被重写。
- 资源端拒绝旧共享卷写入并不取消此前远端动作；网络隔离也不能证明远端未接受。未知外部效果持续阻塞自动重放，新 Run 不能以已 fencing 为由再执行。

**必要证据**

- 生命周期/结果知识修订轨迹
- 所有重试入口的拒绝与决策
- 外部核实证据及其摘要
- 独立远端效果计数、共享卷 fence 回执与新 Run 重放拒绝的对照时间线

### AT-CORE-004｜审批并发消费及执行前目标改变

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-007`、`V1-SEC-008`、`V1-SEC-009`、`V1-WFL-007`
- **阶段：** M0、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两个 Operation 共享单次 ApprovalDecision，批准计划绑定固定资源版本；执行端能原子校验该前提。

**操作、审查步骤或故障注入**

1. 同时消费审批，在获胜绑定提交后杀进程，恢复后重试；执行前再修改效果相关资源版本。
2. 另跑只更新心跳的对照，检查权威判定和许可签发。

**通过条件及结论边界**

- 只有一个 Operation 取得单次绑定，原 Operation 重试不额外消费，绑定不自动返池。
- 旧批准不能执行变更后的效果；无关心跳不造成无必要失效；执行端前提失败被准确记录。

**必要证据**

- CAS 竞争与恢复记录
- 计划摘要/资源效果版本
- ExecutionPermit、审计和效果端条件写证据

### AT-CORE-005｜旧控制面恢复及资源端世代拒绝

- **主规范：** [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md)
- **要求：** `V1-CLU-004`、`V1-CLU-005`、`V1-CLU-008`、`V1-REC-013`
- **阶段：** M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 控制面备份较旧，至少一个旧 Worker 仍运行；卷/设备服务分别提供支持与不支持 fence 的 Profile。

**操作、审查步骤或故障注入**

1. 恢复控制面并提出新 incarnation，故意不让一个资源端收到新世代；让旧 Worker 继续写并提交迟到结果。
2. 持久安装 fence 后再准入新 Worker；最后重新连接旧 Worker。

**通过条件及结论边界**

- 只有已确认安装新世代屏障的资源可转交；不可达或能力不足的资源保持 RecoveryRequired。
- 迟到结果不能覆盖新 Run，可信外部效果证据仍进入对账；同世代 epoch 比较不跨资源。

**必要证据**

- 各资源 FenceReceipt 和存储持久值
- 新旧 Worker 实际写轨迹
- 领取/结果版本与恢复决策

### AT-CORE-006｜列表分页到 watch 无缺口及删除重同步

- **主规范：** [docs/02-common-contracts.md](../docs/02-common-contracts.md)
- **要求：** `V1-API-014`、`V1-API-015`、`V1-CLU-009`
- **阶段：** M0、M2、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 权威服务支持有限快照/事件保留与分页，客户端建立本地投影；可并发新增、修改、删除和撤权。

**操作、审查步骤或故障注入**

1. 在第一与最后一页之间大量写入；分页完成后从所给起点 watch，注入断连和重复事件。
2. 使游标过期或恢复到旧控制面备份，然后重新列表并接续 watch。

**通过条件及结论边界**

- 分页使用同一快照且与 watch 之间没有未覆盖变更；重复事件幂等应用。
- 过期/恢复游标明确失效，完整重同步清除已删除对象与无权缓存，不以空页假装没有变化。

**必要证据**

- 快照/游标/恢复世代协议轨迹
- 权威和投影最终集合差异
- 删除/撤权与重复事件断言

### AT-CORE-007｜容量准入包含去重、审计和终态预留

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-API-012`、`V1-API-018`、`V1-TSK-003`、`V1-OPS-004`
- **阶段：** M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- Profile 明确请求、队列、WAL、正文、附件、结果、墓碑、审计及终态容量和救援保留。

**操作、审查步骤或故障注入**

1. 分别填满不同空间并高并发提交 Task/消息，模拟已接受任务产生最大允许结果；同时触发 GC 和取消。
2. 尝试启用没有明确定义关键硬限的生产 Profile。

**通过条件及结论边界**

- 新工作在必要持久容量不足时准确拒绝，不能先 Accepted 后因无元数据空间丢任务。
- 已接受任务可保存必要终态/审计，防重放与引用不被随意清理，宿主救援可用；未定义硬限的生产 Profile 不激活。

**必要证据**

- 分类配额/预留/用量曲线
- 接受与拒绝回执和持久记录
- GC/终态/救援检查

### AT-CORE-008｜网络确认与旧回退定时器的代次竞争

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-NET-001`、`V1-NET-002`、`V1-API-003`
- **阶段：** M1、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 本地控制台可用；网络配置有稳定 generation，准备保持不断的旧长连接与可重新建立的管理连接。

**操作、审查步骤或故障注入**

1. 应用配置 A，只保留旧长连接，验证其不能确认新配置；再完成新配置 B 的新认证连接确认。
2. 随后触发 A 的旧回退定时器、重放 A 的 token，并在确认提交边界断电。

**通过条件及结论边界**

- 只有绑定当前 operation_id/配置摘要/generation/期限的新连接能确认。
- A 的定时器或旧 token 不覆盖已提交 B；恢复按持久状态决定，不让网络错误阻断本机救援。

**必要证据**

- 网络配置代次和 Operation 日志
- 新旧连接与 token 绑定（脱敏）
- 实际路由/DNS 与断电恢复记录

### AT-CORE-009｜试启动回退与不可逆数据迁移

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-UPD-001`、`V1-UPD-002`、`V1-UPD-003`、`V1-UPD-004`
- **阶段：** M1、M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 新宿主制品经过验证，应用数据含不兼容新 writer Schema；旧部署与独立救援可用。

**操作、审查步骤或故障注入**

1. 在 Staged、BootPending、TrialBoot 和健康确认提交点分别断电；令同一新摘要连续试启动失败。
2. 在持久数据已经迁移后回退旧二进制，尝试写数据和复活旧授权。

**通过条件及结论边界**

- 启动计数/检查点恢复明确，不无限重装失败摘要；平台不支持自动回退时如实使用人工恢复路径。
- OS 回退不假装数据回退；旧 writer 拒绝不兼容写入，安全代次不倒退，救援可用。

**必要证据**

- 引导计数/部署摘要/状态机轨迹
- 数据 Schema 兼容和实际写拒绝
- 失败摘要抑制及恢复日志

### AT-CORE-010｜Artifact 发布中的文件与目录持久边界

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-STO-006`、`V1-TFS-003`、`V1-TSK-010`、`V1-CHAT-011`
- **阶段：** M2、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 后端声明原子 rename 与文件/目录同步保证；Task 结果和 Chat 附件采用不可变引用。

**操作、审查步骤或故障注入**

1. 在内容写入、校验、文件同步、rename、目录同步和引用提交各边界断电。
2. 恢复后对照已发出的 Accepted/Succeeded 回执，尝试读取并校验所有被引用对象。

**通过条件及结论边界**

- 已确认范围内内容与命名空间均可恢复；未达到发布持久边界的对象不被标成可用成功。
- 孤立暂存有界回收，已引用对象不误删，短写/目录同步错误可见而非吞掉。

**必要证据**

- 系统调用/后端事务故障点
- 回执与对象清单
- 独立内容摘要及引用完整性检查

### AT-CORE-011｜不可信 PR 的 CI 权限及签名制品绑定

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-REL-001`、`V1-REL-002`、`V1-REL-005`、`V1-WFL-014`
- **阶段：** M3、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 使用独立测试执行器、受保护签名阶段和发布门禁；PR 可修改构建脚本，但没有生产凭据。

**操作、审查步骤或故障注入**

1. 从恶意 PR 尝试访问发布密钥、宿主 root、用户卷和受限网络；在 CI 通过后替换待签名制品或提交。
2. 提交带旧测试结果/不匹配 commit 的支持矩阵，验证发布门禁。

**通过条件及结论边界**

- 不可信代码无法获得生产/签名秘密或宿主特权；合法测试仍可完成。
- 签名与发布只能绑定被验证的 commit/artifact，旧历史报告或文档自检不充当本版本产品验收。

**必要证据**

- 执行器隔离与秘密访问负例
- commit/artifact/SBOM/签名关联
- 发布门禁拒绝与合法路径

### AT-CORE-012｜宿主救援独立于可选服务和模型

- **主规范：** [docs/01-product-and-architecture.md](../docs/01-product-and-architecture.md)
- **要求：** `V1-ARCH-001`、`V1-ARCH-003`、`V1-FND-002`、`V1-FND-005`、`V1-OBS-001`
- **阶段：** M1、M3、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 三发行版分别配置已声明最小 Profile；合法本地恢复身份和必要磁盘材料可用。

**操作、审查步骤或故障注入**

1. 同时停止模型、网络、Controller、容器引擎、Project 和可选 Dataset/TableFS 服务。
2. 经本地文本/外部可信救援路径登录，读取启动故障、磁盘状态和任务阻塞信息。

**通过条件及结论边界**

- HostManageable 与 EditionReady/可选负载状态分别准确展示，本机诊断恢复不依赖这些服务。
- 加密保护仍需合法材料；不能为救援新增未授权通用后门或无限重启所有容器。

**必要证据**

- 三版/Profile 服务清单
- 故障注入与就绪状态
- 本地救援完整操作记录

### AT-CORE-013｜磁盘和设备路径复用的执行前校验

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-FND-004`、`V1-DEV-001`、`V1-API-004`
- **阶段：** M1、M2
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两块可热插拔测试盘具有不同稳定身份但相近容量；已批准安装/格式化计划绑定其中一盘。

**操作、审查步骤或故障注入**

1. 在预览批准后、实际写前交换路径或替换设备，另测试同设备分区指纹变化。
2. 对摄像头/USB/GPU 设备重置后复用路径，尝试使用旧 generation 的租用。

**通过条件及结论边界**

- 执行端拒绝目标身份/指纹不匹配，不格式化替换盘。
- 旧设备租用不因路径相同恢复权限，重新计划/授权有明确资源依据。

**必要证据**

- 稳定设备标识/拓扑与计划
- 实际阻断时点及设备代次
- 所有测试盘前后校验

### AT-CORE-014｜休眠、时钟回拨和重启不延长许可

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-FND-006`、`V1-REC-019`、`V1-CLU-007`、`V1-SEC-008`
- **阶段：** M1、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备证书绝对有效期、boot_id 绑定租约、计入休眠的输入许可及受限离线 Grant。

**操作、审查步骤或故障注入**

1. 回拨墙钟、休眠越过期限、重启并恢复旧剩余时长后分别执行操作；跨节点交换单调时钟数值做负例。
2. 恢复可信时间后重新申请许可，检查旧许可仍不可复活。

**通过条件及结论边界**

- 每种期限按其声明时钟域验证，旧剩余时长不重新起算；跨节点单调数值不作为可比时间。
- 无法证明有效时停止新增敏感效果，同时保留独立本地只读诊断。

**必要证据**

- boot_id/时钟类型/期限计算
- 敏感操作拒绝与合法重申请
- 休眠/重启原始时间线

### AT-CORE-015｜DNS、IPv6、代理和重定向出口绕过

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-NET-003`、`V1-SEC-012`、`V1-AGT-006`、`V1-GUI-008`
- **阶段：** M1、M3、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 配置只允许测试公共目的地的 Agent/应用/代理出站策略，保留可观察内网、link-local 和元数据诱饵。

**操作、审查步骤或故障注入**

1. 尝试 DNS 重绑定/重解析、IPv6 地址、跨 origin 重定向、代理转发和本机目标；令网络驱动失效。
2. 带虚构凭据测试重定向，确认不随请求流向未经授权 origin。

**通过条件及结论边界**

- 实际出口路径都遵守同一资源/数据策略，不因 IPv4 规则、代理或驱动失败获得 host network。
- 受限目标与凭据外发被拒，合法流量仍可用，错误原因可诊断。

**必要证据**

- 策略/解析/重定向轨迹
- 网络抓取与诱饵访问记录
- 驱动故障降级和合法对照

### AT-CORE-016｜节点实际准入、资源预留与救援保额

- **主规范：** [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md)
- **要求：** `V1-CLU-011`、`V1-DEV-002`、`V1-DEV-003`、`V1-OPS-004`
- **阶段：** M2、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 多个调度器候选读取相同空闲广告，节点具有真实 CPU/内存/PID/IO/设备限制与独立救援预算。

**操作、审查步骤或故障注入**

1. 并发放置超过实际容量的工作；在预留后崩溃、取消、过期，再让旧 Run 迟到释放资源。
2. 运行内存/PID/IO/显存压力负载并检查本地诊断响应。

**通过条件及结论边界**

- 节点二次准入与预留限制过量执行，旧 Run 不能释放新 owner 资源；泄漏按原 claim 对账。
- 声明硬限的资源实际受控，未具备显存硬隔离的后端不伪称多租户强保证，救援可用。

**必要证据**

- 广告/预留/实际用量与强制配置
- claim 所有权和回收日志
- 压力期间诊断与资源端证据

### AT-CORE-017｜节点身份克隆、权限移交和人工接管

- **主规范：** [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md)
- **要求：** `V1-CLU-001`、`V1-CLU-002`、`V1-API-005`、`V1-OPS-005`
- **阶段：** M1、M4
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 集群只获节点明确授权资源，另有用户文件和手动管理资源；准备包含旧 node_id 的克隆镜像。

**操作、审查步骤或故障注入**

1. 克隆节点尝试复用原私钥入群，轮换正式证书并重放 bootstrap；对受管资源切换 Manual 后由控制器重试旧 spec。
2. 退役或强制退出不可达节点，检查残留工作与凭据范围。

**通过条件及结论边界**

- 重复身份/引导重放不能取得新资源权限，证书轮换受域与受众限制。
- Manual 期间不被旧控制写覆盖，入群/退役不隐式接管或删除用户数据；不可达残留明确报告。

**必要证据**

- 身份注册/轮换/拒绝日志
- 资源授权与控制模式修订
- 退役残留清单

### AT-CORE-018｜恢复旧数据不得恢复已撤销分享和索引可见性

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-REC-023`、`V1-DAT-005`、`V1-CHAT-012`、`V1-OBS-004`
- **阶段：** M2、M3、M5
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 旧备份包含私聊附件、Dataset 记录和曾公开共享的链接；备份后删除/撤权，部分删除屏障仍可信。

**操作、审查步骤或故障注入**

1. 恢复旧数据，重建索引/摘要/缓存并启动通知和导出；对不能证明删除历史的对象单独测试。
2. 使用原共享链接和跨域检索请求访问恢复对象。

**通过条件及结论边界**

- 已知删除和撤权在发布前生效，旧索引/缓存/摘要不泄露内容或计数。
- 未知删除历史保持隔离，不自动重开外部分享；需要保留的受限证据不等于无限保存完整私聊。

**必要证据**

- 删除/共享/保留屏障
- 各访问入口负例
- 恢复后的索引、缓存和受限证据清单

### AT-CORE-019｜跨语言合同、摘要和未知安全字段

- **主规范：** [docs/02-common-contracts.md](../docs/02-common-contracts.md)
- **要求：** `V1-API-006`、`V1-API-007`、`V1-API-008`、`V1-API-016`、`V1-SEC-006`、`V1-IF-002`
- **阶段：** M0、M3
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 两种语言 SDK 与服务端使用同一合同；准备 TaskSubmit、ChatSend、Plan 和大序号的固定向量。

**操作、审查步骤或故障注入**

1. 输入重复键、非法 Unicode、数组顺序变化、大于 2^53 的十进制字符串及未知权限字段；交换 contract 域摘要。
2. 生成符合 JSON Schema 但签名无效、摘要错误、引用不存在或预算不足的请求。
3. 对所有十进制计数字段跨语言/SDK/存储往返测试 0、9007199254740993、u64 最大值，提交负数、前导零、u64 最大值加一；检验同对象 observed_generation 不大于 spec_generation，并检查 UI 格式化不修改传输或持久值。

**通过条件及结论边界**

- 合法输入跨语言摘要一致；非法编码、未知安全语义与错误域摘要一致拒绝，不发生精度截断。
- 结构通过不等于执行获准，运行时继续检查身份、引用、CAS、预算和实际 fence。
- 安全整数边界之外的计数仍逐字符或以无损整数精确保留；语法合法但超 u64 的值在语义层拒绝，禁止先转 JavaScript number 后比较；非法代数关系被拒绝。

**必要证据**

- 跨语言版本与向量字节
- Schema 与语义拒绝对照
- 实际鉴权/摘要/预算执行判定
- 计数字符串跨语言、SDK、存储和 UI 往返向量，u64 越界及跨字段拒绝记录

### AT-CORE-020｜跨服务配置部分应用与可恢复维护

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-OPS-001`、`V1-OPS-002`、`V1-OPS-003`、`V1-API-009`
- **阶段：** M2、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 一次配置变更影响网络、设备和应用，配置期望 revision 已提交；每目标可观测实际 revision 与 Operation。

**操作、审查步骤或故障注入**

1. 让一项目标成功、一项失败、一项结果未知，然后重启协调器并请求维护/取消。
2. 在此期间提交后续配置并注入旧补偿回执，核对版本与所有权。

**通过条件及结论边界**

- Applied 仅在全部必要目标满足条件时成立；部分失败/未知清楚列出已生效与未处理范围。
- 恢复从持久 Operation 对账，旧补偿不覆盖新 revision；维护到安全点/fence 后才释放资源。

**必要证据**

- 期望/实际配置与来源表
- 逐目标 Operation/补偿/未知证据
- 维护和恢复时序

## AT-COMP

### AT-COMP-001｜规范、Schema、旧编号与变更流程的文档审查

- **主规范：** [docs/00-reading-and-authority.md](../docs/00-reading-and-authority.md)
- **要求：** `V1-GOV-001`、`V1-GOV-004`、`V1-GOV-005`
- **阶段：** M0、M6
- **验证方式：** DocumentReview
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 固定待审文档包的版本和内容摘要，取得分章规范、Schema 编辑源、生成 Schema、旧源文档映射及 ADR/工作项登记。
- 准备受控审查副本，分别植入一个字段枚举冲突、一个语义冲突、一个缺少专用写合同的通用 spec，以及两份来源中同名的 P0-04。

**操作、审查步骤或故障注入**

1. 逐对象对照字段、枚举、必填、摘要范围、状态与跨对象语义；分别运行静态检查并进行独立人工审查，记录工具无法发现的语义冲突。
2. 核对冲突项是否明确阻止对应契约冻结，要求先修订统一包及例子，不允许实现者自行选择较宽松版本。
3. 按 source_document + old_id 回查两份 P0-04 的原文、目标工作项及迁移理由；复核 ADR-21～25 的主题、旧 ADR-22 分拆及交叉依赖。
4. 追踪一项模拟高影响合同变更，从影响/反例、设计/Schema 草案、独立复核、正式 ADR 决策、代码到制品验收和支持矩阵，核对缺失步骤不能被聊天同意替代。

**通过条件及结论边界**

- 全部植入冲突都进入带出处的问题记录并保持对应合同未冻结；正文和 Schema 没有自动互相覆盖的规则。
- 没有机器 Schema 的对象仍有明确字段和语义；ResourceEnvelope 的开放 spec 不被当成任意资源创建接口。
- 来源不同的同名旧 ID 不被合并，ADR 编号和主题连续可追溯。
- 本项 Passed 仅表示所审版本的文档一致性和流程记录达到审查条件，不证明变更已实现、已合并或已发布。

**必要证据**

- 受控缺陷副本及逐项检出记录
- 原文—迁移目标对照、ADR 主题表及审查者签认
- 模拟变更的完整流转记录与尚缺证据清单

### AT-COMP-002｜验收证据、完整矩阵及发布声明的文档审查

- **主规范：** [docs/15-acceptance-and-evidence.md](../docs/15-acceptance-and-evidence.md)
- **要求：** `V1-GOV-002`、`V1-SEC-022`、`V1-REC-022`、`V1-TEST-001`、`V1-TEST-002`、`V1-TEST-003`、`V1-TEST-005`、`V1-REL-004`
- **阶段：** M0、M6
- **验证方式：** DocumentReview
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 取得所有规范要求、SEC-V1/REC-V1 等原矩阵、完整场景注册表、证据模板、支持矩阵草案及历史归档报告。
- 准备负例证据：只有静态校验成功、历史测试数字、错误 commit/制品、仅日志哈希无文件、无复核人、缺故障步骤，以及已宣称能力被标 NotApplicable。

**操作、审查步骤或故障注入**

1. 逐行将安全和恢复矩阵映射到保留原目标的测试计划，审查前置条件、操作、资源端观察及必要证据；不能只比较 ID 是否出现在文本中。
2. 逐项审查每条新规范是否有适当验证方式：纯定义/治理可审文档；事务、权限、资源强制、设备和持久性必须指向真实实现或故障实验。
3. 按模板实际路径核验 source commit、artifact、edition/profile、前提、故障、expected/observed、证据引用、复核和结果；验证上列不完整证据不能支持产品 Passed。
4. 对声明功能逐项检查正常、拒绝、恢复路径，并检查 parser、并发、权限、故障、供应链和平台类别是否按真实攻击面适用；记录未测类别及发布阻断。
5. 核对文档检查、合同实验和产品验收的结论范围；历史记录只保留作者和来源，不提升当前实现状态。

**通过条件及结论边界**

- 所有矩阵目标与规范都有可执行或可审查去向；一个测试覆盖多要求时，每一要求对应具体动作和预期，不能按 ID 数量推导满足率。
- 不完整、不可取回、对象不匹配和错误 NotApplicable 的证据被退回；失败历史不会被最后一次成功覆盖。
- 未运行的产品场景保持 NotRun，缺实现证据保持 NotVerified；文档检查通过不得获得安全、硬件或恢复支持声明。
- 此项是对计划与证据质量的文档审查；REL-004/TEST-003 所列实际测试仍须分别执行，审查结果不能替代它们。

**必要证据**

- 逐条覆盖审查及原矩阵对照
- 正反证据包与拒收理由、模板字段映射
- 按功能和测试类别的发布缺口表
- 独立复核者及审查范围

### AT-COMP-003｜Profile、ADR、有限阈值与发布降级门槛审查及集成检查

- **主规范：** [docs/14-roadmap-and-decisions.md](../docs/14-roadmap-and-decisions.md)
- **要求：** `V1-GOV-003`、`V1-GOV-008`、`V1-PLAN-001`、`V1-PLAN-003`、`V1-PLAN-004`、`V1-TEST-004`、`V1-REL-006`
- **阶段：** M0、M1、M6
- **验证方式：** DocumentReview、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备明确 edition、硬件/VM、后端、协议和制品的候选 Profile；一个完整有限预算版本、一个缺期限/容量/撤权阈值的版本、一个未启用能力版本。
- 取得候选 ADR 的比较实验、许可、维护成本、退出迁移、决策者、重新打开条件，以及 API/Schema 维护和退役政策；无证据项显式 TBD。
- 在隔离发布/准入测试环境中配置无法 fencing、图形只能读取、第三方不支持取消、TableFS 不满足持久性的后端。

**操作、审查步骤或故障注入**

1. 审查新增必装服务的依赖、常驻资源、故障影响和救援成本；把 Helium 与其他版的相同授权/审计/去重/恢复不变量逐项对照。
2. 提交缺少有限数值的 Profile 和未验收组合到候选发布准入流程；查询禁用模块并调用其功能，核对 ServiceNotInstalled/UnsupportedCapability 等准确状态。
3. 分别触发上述受限后端，观察是否停止重新分配/自动重放、只读或拒绝图形控制、选择经验证的普通存储，并检查降级计划没有扩大权限或数据损失范围。
4. 检查各阶段仅承认确切组合；缺决策者、版本、实验制品或退出条件的 ADR 不得变 Accepted；没有实际支持窗口的维护政策不得被写成已冻结。
5. 核对 RPO/RTO、撤权、队列/保留、恢复余量和延迟的单位、起止边界与有限值；移除一项强制阈值，重新检查相关自动化能力的发布资格。

**通过条件及结论边界**

- 缺有限阈值、未解决决策证据或未测组合的功能不进入支持列表；未安装/未支持准确暴露，不伪造空结果或成功。
- 无法证明旧执行端失效、安全新鲜或远端效果的路径停在相应阻塞状态，降级不放宽权限、不删除 Unknown、不自动删除用户数据。
- Profile 精简保持公共安全语义；增加必装服务有量化成本与恢复影响说明。
- 维护政策包含弃用、兼容窗口、渠道、补丁责任、支持期限和导出路径；网络 API 稳定不被误写为 C/C++ ABI 或任意版本升级保证。
- ADR 与阈值审查只证明决策材料完整；真实准入/降级行为必须有本项 Integration 证据才能被接受。

**必要证据**

- Profile 正反例及发布/功能准入结果
- ADR 决策材料、维护政策和复杂度成本表
- 受限后端的真实资源状态、拒绝与降级日志
- 阈值测量边界及未支持组合清单

### AT-COMP-004｜代码来源、模块复用与责任分配的文档审查

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-ARCH-005`、`V1-GOV-006`、`V1-GOV-007`
- **阶段：** M0、M6
- **验证方式：** DocumentReview
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 取得可访问的真实仓库只读快照及完整 commit SHA；访问不足时仅准备固定摘要的归档并标 SourceArchiveOnly。
- 准备 Host、Identity、Task/Event、Storage、Agent、Conversation、Workflow、Project、Cluster、GUI、Data、Release 模块清单及待审工作项。

**操作、审查步骤或故障注入**

1. 逐模块核对来源路径、依赖、项目级 license、构建和测试条件、旧外部地址、授权默认值、持久格式及宿主/root 权限，记录 reuse/adapt/rewrite/exclude 的理由和必要改造。
2. 将源归档中的 crate 名、历史 passed 数字与本次实际获取的 commit 区分；无法访问当前 HEAD 时明确 Blocked 或 SourceArchiveOnly，不补写“最新”。
3. 对每模块核对输入输出、字段权威、升级、失败恢复和维护责任；尝试将缺真实 owner 的工作项推进 InProgress，审查是否被治理流程拒绝。
4. 对高影响变更检查独立复核人及适用故障试验；缺项目许可或依赖证据时阻止该模块被宣称可直接接纳和分发。

**通过条件及结论边界**

- 每个复用结论绑定可核实来源和明确范围；归档存在及旧测试记录不提升为 PsiOS v1 实现或当前 HEAD 证明。
- 缺 owner、独立复核或必要来源证据的事项保持待指派/阻塞；角色占位不冒充实际责任人。
- 许可证不以依赖库许可代替项目许可；未知项保留并给出解除阻塞所需证据。
- 本项 Passed 只确认所审资料的完整性和归属；不证明被复用代码已通过当前产品验收。

**必要证据**

- 仓库/归档摘要与真实获取时间、commit 记录
- 逐模块复用和许可证表、旧地址/权限/格式差异
- 责任分配、独立复核及工作项流转证据

### AT-COMP-005｜宿主平台、驱动扩展、Secure Boot 与 TPM 恢复矩阵

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-FND-001`、`V1-UPD-005`、`V1-FND-007`、`V1-REL-004`
- **阶段：** M1、M4、M6
- **验证方式：** Integration、FaultInjection、Hardware
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 为每个拟支持组合准备确切 CPU 架构/最低指令集、BIOS/UEFI、Secure Boot、内核、固件、驱动、VM 或实机；只测试计划列出的组合，不假定所有硬件相同。
- 镜像提供源 SHA、锁文件、仓库快照、工具链、Profile 和摘要；具备独立救援介质、测试卷和可用恢复密钥，TPM/主板替换在专用测试设备进行。
- 准备正确和错误 ABI/签名的 HostExtension，以及不含 TPM 的明确支持 Profile；不可逆固件变更须有已批准的实验与恢复边界。

**操作、审查步骤或故障注入**

1. 从同一镜像启动两个干净目标，检查 machine-id、节点私钥、安装 token、旧会话及站点地址均未被克隆；模拟随机数未就绪，观察长期密钥生成是否被阻止。
2. 按矩阵测试安装、启动、独立救援、升级和旧部署回退，记录真实引导和解锁行为；独立重建比较制品差异并记录非确定性来源。
3. 通过 HostExtension 路径安装/回退受控驱动，提交错 ABI/错签名/无来源扩展，并让普通 Package Adapter 请求宿主模块安装。
4. 分别改变内核、Secure Boot 策略和固件测量条件，验证旧部署与救援仍按声明路径解锁；在专用设备执行 TPM/主板替换，使用合法恢复材料重新建信任。
5. 测试无 TPM Profile，以及恢复材料不可取得的负例；检查界面不把 TPM 存在当远端撤销历史证明，也不承诺丢失全部解密材料仍能恢复。

**通过条件及结论边界**

- 支持声明精确到真实平台/固件/驱动组合；签名有效、VM 启动或上游 Linux 支持不被扩大为未测硬件通过。
- 长期身份逐机生成；随机数未就绪时不生成可用长期密钥，旧镜像秘密和站点配置不泄漏到新节点。
- HostExtension 的来源、签名、ABI、许可、设备范围与回退限制可查；错误扩展和普通包绕过宿主模块入口被拒绝。
- TPM 测量变化、设备替换与固件不可逆部分有真实恢复证据及限制；OS rollback 不被报告为固件也已回退。
- 无 TPM 的支持路径按 Profile 工作；缺解密材料如实阻塞，未扩大数据访问权。

**必要证据**

- 完整平台矩阵、制品 provenance 和重建差异
- 启动/解锁/救援日志及硬件、固件、驱动版本
- 去身份化检查和随机数未就绪负例
- HostExtension 许可/签名/ABI 检查及模块绕过拒绝记录
- TPM/主板替换和恢复材料使用的脱敏证据

### AT-COMP-006｜同盘状态分域及安全权威在回退恢复中的保持

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-FND-003`、`V1-REC-002`
- **阶段：** M1、M2、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 在同一物理盘建立 Boot、SystemDeployment、LocalConfig、Security、Core、Service、用户数据、派生缓存和 RecoveryMaterial 的明确逻辑清单与权限/配额。
- 准备已变更的 /etc、/var、启动状态、用户数据、SecurityState 撤销与审批用量，以及更旧的 Core/系统部署快照；保存独立恢复材料。

**操作、审查步骤或故障注入**

1. 对各域分别写入可辨识记录并触发真实后端更新/回退，观察实际保留、合并或回退的字节和版本，不能用目录名推断。
2. 从旧 CoreState 恢复含旧授权投影的对象，保持较新的 SecurityState；尝试通过 Core 缓存重新授予已撤销 Grant 或重复消费批准。
3. 使共享磁盘的某域配额/inode 耗尽并在检查点注入中断，检查跨域故障影响是否与清单一致，独立诊断和恢复材料是否仍可用。
4. 核对备份/恢复清单、访问控制和迁移责任，检查同进程或共库实现没有生成第二套可写安全事实。

**通过条件及结论边界**

- 实际 /etc、/var、密钥、BootState 和用户数据行为与该后端 Profile 一致；系统回退不被误报为这些域同时原子回退。
- 授权、撤销和审批用量仍只有 SecurityState 权威；旧 Core 投影不能恢复权限或覆盖当前安全代次。
- 同盘并不丢失逻辑权限、配额、Schema 和恢复依赖边界；域故障影响有实测记录且必要救援预算保留。
- 不能核验安全新鲜度时保持隔离，不用恢复了数据或系统可启动代替授权证明。

**必要证据**

- 状态域—物理存储—权限—恢复依赖表
- 更新/回退前后各域版本和内容校验
- 旧授权投影重放拒绝及审批权威账本
- 配额/中断注入与本机救援记录

### AT-COMP-007｜类型化宿主 Gateway 与执行隔离失败的封闭行为

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-BASE-002`、`V1-SEC-010`、`V1-REL-004`
- **阶段：** M0、M2、M3
- **验证方式：** Contract、Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备普通应用、原生 Agent、第三方工具和独立管理员调试身份；列出宿主可信组件与各运行环境的目录、设备、网络和能力授权。
- Host Broker 动作已登记参数 Schema、目标类型、效果、授权、取消、查询与撤权能力；能够使沙箱创建及能力强制后端失败。

**操作、审查步骤或故障注入**

1. 以各普通身份调用已授权卷/设备/网络动作并对照管理员路径，检查返回的 Operation、取消范围、效果和授权记录。
2. 提交任意 shell/root 执行、未知动作、恶意参数，以及宿主根、引擎 Socket、全部设备和会话总线挂载请求。
3. 在运行准入时使沙箱创建失败或移除所需隔离能力，观察是否准确拒绝，检查宿主进程、挂载、网络 namespace 和实际文件变化。
4. 在撤权和取消后尝试继续相同设备/卷动作；单独启用获准的管理员调试入口，验证它需要独立身份与审计且不会变成 Agent 默认路径。

**通过条件及结论边界**

- 普通执行只能经获准的类型化动作及受限资源发生；未知参数或越权目标在效果前被拒绝。
- 没有通用 root shell、管理 Socket 或整机设备的隐式授权；OCI 标签本身不替代实际隔离证据。
- 沙箱/隔离失败返回明确失败或 UnsupportedCapability，绝不静默改到宿主执行。
- 可信宿主组件与容器/运行环境责任明确；独立管理/救援可用但不成为绕过审计的自动化后门。

**必要证据**

- Broker 能力定义和结构正反例
- 真实进程/挂载/网络/设备访问清单
- 故障前后宿主文件校验与拒绝日志
- 管理员调试独立授权及审计记录

### AT-COMP-008｜原生 Agent 独立执行链与不依赖内部思维链的审计

- **主规范：** [docs/09-agents-models-protocols.md](../docs/09-agents-models-protocols.md)
- **要求：** `V1-AGT-001`、`V1-AGT-003`
- **阶段：** M2、M3
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 原生 Agent、一个版本固定的第三方 Adapter、CLI 和统一 Broker 均连接同一受控工具；使用相同资源、委托、预算及效果计划。
- 模型测试端只提供普通结构化输出和 Token 流，不暴露内部思维链；结果存储、身份服务和独立本机诊断可查询。

**操作、审查步骤或故障注入**

1. 从三个入口执行相同受限任务，追踪 Task 到调度器、Runtime/Adapter、OperationPlan、资源端及 Artifact 发布；核对意图在执行前经过当前授权、预算和效果前提校验。
2. 返回格式错误、越权、预算超限及修改目标的模型意图，检查被拒时没有实际工具效果；流式预览中断后查询正式结果。
3. 在工具返回后、最终 Artifact 发布前杀死 Agent；分别断开模型、聊天和可选 Project/Dataset，重启后依据持久 Task/Operation 对账。
4. 从本机登录读取日志、检查存储并进入独立恢复；审查运行日志是否包含目标、计划、工具调用、结果引用、效果证据和用户说明，同时不要求或记录模型内部思维链。

**通过条件及结论边界**

- 原生和第三方 Agent 都经统一 Broker 与相同权限/预算边界；模型输出本身不产生权限或副作用授权。
- 正式结果仅在校验、持久发布 Artifact 后可用，流式 Token 或会话内存不冒充最终输出。
- Agent/模型/聊天故障不阻止登录、诊断、存储检查和独立恢复；Task 日志与查询不依赖存活的 Session。
- 有限日志足以复核目标、操作与真实结果；未提供内部思维链不构成测试阻塞，敏感上下文不被无边界复制。

**必要证据**

- CLI/原生/第三方同任务的权威链路对照
- 结构化意图拒绝与资源端无效果证明
- 崩溃点、Artifact 发布与恢复查询
- 不含内部思维链的审计样本及离线救援记录

### AT-COMP-009｜对象身份、域与写入权威的合同及跨服务验证

- **主规范：** [docs/01-product-and-architecture.md](../docs/01-product-and-architecture.md)
- **要求：** `V1-BASE-004`、`V1-ARCH-002`、`V1-API-001`、`V1-SEC-002`、`V1-TSK-001`、`V1-IF-001`
- **阶段：** M0、M2、M3
- **验证方式：** DocumentReview、Contract、Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备两个身份签发域与两个安全域中的同名资源，以及 Conversation、WorkItem、WorkflowRun、Task、Run、Operation、Event、Artifact 和全部安全字典对象。
- 登记各对象权威、资源信封与领域读视图的唯一映射，准备一次合法 authority 转交、投影缓存、可丢通知的 outbox 通道及旧版本客户端。

**操作、审查步骤或故障注入**

1. 审查安全字典每类对象的稳定 ID、trust/security domain、版本、创建来源和审计引用，并核对 Approval 旧视图与 CapabilityDescriptor 投影不构成第二权威。
2. 更改显示名、节点地址和 authority，在转交前后查询资源；删除再建、克隆及恢复时验证原身份是否按各自规则保留或新建，而不是按路径、IP、PID 或 JSON-RPC id 推导。
3. 尝试用群成员身份、WorkItem 指派、请求自报 security_domain、CapabilityDescriptor 或扁平 task_id 覆盖不匹配的信封/授权身份；向只读 status 投影和通用 spec 发写请求。
4. 接受 Task 后尝试原地改目标/输入，随后按合法新 Task + supersedes/retry_of 方式修正，核查旧 Task 的取消、结果和审计仍可读。
5. 丢弃/重排投影更新并恢复消费者，检查可靠 outbox 重放、来源修订及缓存对账；尝试从旧投影反向改写权威事实。

**通过条件及结论边界**

- 身份与位置、当前管理者、显示名及权限分开；资源 ID 不被复用，跨域相似字符串不能授予访问。
- 各对象只有规定权威写入；请求、Accepted 回执和受授权读投影互不混用，信封与领域 ID 映射一致。
- Conversation/WorkItem/WorkflowRun/Task/Run/Operation 分别保存各自事实；成员和指派不自动授予执行或外部合并权。
- 已接受 Task 目标不可原地替换；新目标产生新 Task 并保留关系与旧证据。
- 丢通知后投影可恢复，旧投影不能覆盖新源事实；历史 Approval 视图和能力描述不能自行改写授权。

**必要证据**

- 逐对象字典/字段权威及信封映射审查表
- 迁移、删除再建、克隆和跨域访问的 ID 记录
- 非法写与输入变更拒绝及新旧 Task 关系
- outbox/投影源修订、故障交错及最终权威对照

### AT-COMP-010｜Secret 分用途轮换、受众验证与备份解密连续性

- **主规范：** [docs/04-security-identity.md](../docs/04-security-identity.md)
- **要求：** `V1-SEC-013`
- **阶段：** M2、M3、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 使用虚构秘密为 API、节点身份、发布签名、卷和备份分别建立导入、活动、轮换、退役、撤销、销毁和恢复记录；有历史备份及密钥引用清单。
- 配置受限代调用服务、确需短期凭据的第三方测试程序、独立受众 OAuth 服务，以及日志/环境/转储/同用户读取的隔离检测能力。

**操作、审查步骤或故障注入**

1. 经 SecretRef 调用工具并检查模型输入、事件、环境导出、普通日志、崩溃转储及其他进程可见面；分别用错误主体、用途和受众请求秘密使用。
2. 轮换 API/节点认证与卷/备份解密材料，重放旧认证凭据并读取应保留的历史备份；在更新引用或回执处注入崩溃后恢复。
3. 在密钥仍被保留备份引用时申请销毁，随后按显式保留/恢复政策处理；检查在线销毁报告没有声称所有历史副本已抹除。
4. 执行网络 OAuth 负例：错误 issuer/受众/范围/过期令牌及跨服务 token 透传；测试所选 code+PKCE 与公共客户端 refresh 轮换或 sender constraint，重放旧 refresh token。
5. 对本机 STDIO/IPC 路径验证自身认证与权限，不把网络 OAuth 前提套用成虚构握手。

**通过条件及结论边界**

- 秘密正文不进入模型和通用事件；凭据授予有短期限、最小范围及明确受众，环境变量不被当成保密证明。
- 旧认证权限被撤销，同时合法保留的历史备份仍能由适当解密材料恢复；两种生命周期没有互相替代。
- 崩溃后秘密版本与引用可对账；仍有恢复依赖的销毁请求受阻或进入明确获准流程。
- OAuth 校验按所选协议及每次请求执行，错误受众/范围和非法刷新重放被拒绝；不跨服务盲转 access token。
- 销毁和恢复报告注明实际覆盖与残留副本，不能从在线删除推导全部历史明文消失。

**必要证据**

- 按用途区分的密钥版本/引用/轮换账本（无秘密正文）
- 进程、日志、转储与模型输入泄漏检查
- 旧凭据重放拒绝和历史备份解密校验
- OAuth/本机认证正反测试记录及崩溃恢复证据

### AT-COMP-011｜三发行版首次启动、会话切换及中文可访问性路径

- **主规范：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)
- **要求：** `V1-UX-001`、`V1-UX-002`
- **阶段：** M1、M4、M6
- **验证方式：** Integration、FaultInjection、Hardware
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备 Helium、Neon、Argon 的确切 Profile，分别包括文本/无头管理和已声明 Shell/KDE 桌面；配置键盘、屏幕阅读器、中文 IME、缩放/对比度及混合 DPI 多屏设备。
- 首次启动无模型、断公网但有本机控制台；准备两个普通用户、有限桌面授权、服务身份后台任务以及可撤销的一次性无头初始化入口。

**操作、审查步骤或故障注入**

1. 只用键盘和屏幕阅读完成账户、网络、区域/时区、加密恢复、更新和管理暴露配置，跳过模型；无头版完成后重放一次性初始化凭据。
2. 在中文 IME 组合态、非 ASCII 文件/路径、多屏焦点和不同 DPI/缩放下完成核心导航、输入、确认和错误恢复；保存焦点、读屏标签及输入提交证据。
3. 在交互操作和后台任务并行时锁屏、注销、切换用户并恢复显示，核查桌面/输入授权撤销及后台任务按独立服务政策继续或停止。
4. 让耗时操作部分成功、取消或进入 Unknown，核对 Shell/CLI 显示的 operation_id、实际进度来源、取消范围、错误码、时区与资源状态。
5. 检查通知、建议、审批、真实告警及权限/卸载页面，确保可见主体、委托、期限和撤销范围准确，独立卷/备份/共享的保留去向不因卸载而被省略。

**通过条件及结论边界**

- 首次配置无需模型完成；无头一次性入口完成后不可复用，图形失败时仍有受控文本/救援路径。
- 已声明核心路径能够用所列辅助方式实际完成；IME、非 ASCII、焦点、多屏和缩放不造成越界输入或无法完成的确认。
- 交互会话失效传播至桌面授权，旧登录不能续权；后台服务行为由自身身份与现行政策决定。
- 界面准确区分建议、审批和实际状态；取消/撤权/卸载显示真实范围，不把受理等同完成或把应用移除等同删数据。
- 结果仅覆盖实测版本与设备组合；缺可访问性或显示支持的组合不被纳入支持矩阵。

**必要证据**

- 按版/设备/输入方式的逐步可完成性记录
- 读屏、焦点、IME、时区和稳定错误码证据
- 初始化凭据失效及会话/服务授权日志
- 故障时 Operation/资源真实状态与界面对应截图或录屏（脱敏）

### AT-COMP-012｜聊天外部桥接责任及 Shell、CLI、SDK 端到端一致性

- **主规范：** [docs/10-conversations.md](../docs/10-conversations.md)
- **要求：** `V1-CHAT-013`、`V1-CHAT-014`
- **阶段：** M3、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 配置一个版本固定的外部 Agent/聊天测试平台及身份绑定；分别准备支持和不支持撤回、逐成员历史、可靠投递的能力 Profile。
- 三版按其已声明入口提供本机会话，配置可丢 ACK/重排通知的桥接器、已授权本机历史和有限自动回复预算。

**操作、审查步骤或故障注入**

1. 从 Shell、CLI、SDK/API 分别完成会话/DM 列表、线程、提及、查询、发送、成员管理、Task 关联、归档、导出和暂停自动回复，对照同一权威记录。
2. 经外部桥接转发消息，检查外部 Principal 绑定、端点身份、协议全名/版本、数据流向、原始来源及新发送者责任，阻断冒充真人的展示/发送。
3. 丢失桥接 ACK 后重启适配器并制造回流，验证桥接 ID 和回流检测持久化；调用未支持的撤回/历史/可靠投递语义。
4. 断开 Controller、模型、Dataset 和 Project/Studio 后，通过本机 CLI 管理及读取已获准历史；检查本地待发送、服务器接受、投递、已读、处理与 Task 状态分开。

**通过条件及结论边界**

- 同一会话从各入口执行相同授权和状态合同；可选增强服务失联不阻止合法本机记录、CLI 管理和救援。
- 外部来源与新发送者责任清楚可查，不伪装成人类手工发言；能力声明绑定确切协议版本及真实测试。
- 适配器重启后仍能识别桥接回流；未知投递如实展示并提供核实入口，不自动变已读/已处理。
- 上游缺少撤回、成员历史或可靠投递时返回 UnsupportedCapability 或明确限制，不声称与本机语义等价。
- 暂停自动回复后不新增自动派发；手动管理、导出和任务查询仍遵循当前权限。

**必要证据**

- 入口功能—权威记录对照表
- 外部身份/版本/端点/数据流登记和桥接责任链
- ACK 丢失、重启、回流与不支持能力的实际响应
- 断依赖后的本机管理及状态展示记录

### AT-COMP-013｜WorkItem 依赖、验收状态与真实独立审查身份

- **主规范：** [docs/11-workflows-projects.md](../docs/11-workflows-projects.md)
- **要求：** `V1-WFL-011`
- **阶段：** M3、M4
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 创建包含项目/里程碑、优先级、父子/阻塞关系、指派、目标资产、文档、Task/Workflow、外部 Issue 和验收证据的 WorkItem。
- 准备两个不同显示名但同后台 Principal 的 Agent、一位真正符合分离策略的审查者，以及成功 Workflow、绿色 CI、已合并代码但尚缺验收条件的交付。

**操作、审查步骤或故障注入**

1. 形成合法依赖图并加入循环/悬空阻塞关系，检查循环检测和可解释阻塞原因；配置项目附加状态并核对其规范状态映射。
2. 变更指派者/责任人后尝试读取未授权卷、Secret 和执行合并，检查责任变化没有隐式授予权限。
3. 分别仅凭 Workflow 成功、CI 绿色、合并或部署完成申请 Verified/Closed，再补齐逐条验收证据并让授权责任者接受结束。
4. 在需要独立审查的策略下，以两个 Agent 名称但同一后台身份批准，再用符合策略的真实独立 Principal 完成审查。

**通过条件及结论边界**

- WorkItem 保存规定业务关系与证据引用，依赖循环被拒绝或明确阻塞，附加状态有稳定映射。
- 指派只改变责任归属，不新增数据、密钥、工具或合并授权。
- Verified 需要各验收条件的有效证据，Closed 需要授权责任者接受；外部 CI/合并/部署事实分别记录。
- 独立性按真实 Principal 和具体策略判断，两个显示名共享万能后台账号不能通过独立审查。

**必要证据**

- 依赖/状态正反夹具与阻塞结果
- 指派前后授权差异及越权拒绝
- 验收条件—证据—Verified/Closed 变更链
- 真实 Principal 独立性及审批审计

### AT-COMP-014｜基础存储独立性与 TableFS 双入口唯一权威

- **主规范：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)
- **要求：** `V1-BASE-003`、`V1-STO-001`、`V1-TFS-001`
- **阶段：** M2、M5
- **验证方式：** Contract、Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备普通文件系统 Volume 与可选 TableFS 后端，分别在未安装 Dataset/Knowledge/Embedding、已安装、后端断联三种环境中运行。
- TableFS 登记 inode/dentry/content/chunk/record_link/query_view 权威结构及索引水位，准备可观察的 POSIX 和 Catalog 客户端。

**操作、审查步骤或故障注入**

1. 在 Data Services 未安装时完成宿主启动、卷创建/挂载、基础读写、备份及救援；独立运行 TableFS 基础文件与 Catalog，不隐式安装模型或 Dataset。
2. 分别访问未安装服务、已安装但断联的服务及不支持特性，检查 ServiceNotInstalled、StorageUnavailable、UnsupportedCapability 不被合并为空数据。
3. 从 POSIX 与 Catalog 两入口创建/重命名/读取同一对象，暂停投影索引后查询已提交文件；检验名称唯一、目录无环、实体 ID 不因路径变化而变化。
4. 删除对象后在同世代创建新对象，验证身份不复用；显式建立 Dataset 映射并改变内容版本，核查映射/视图带版本且不会成为另一套原文权威。
5. 以未授权域访问 chunk 标识和去重命中信息；核对 Knowledge 仅使用获准 Dataset 视图，Embedding 仍为独立受出口政策控制的服务。

**通过条件及结论边界**

- Host Storage、Storage Service 和可选 Data Services 保持独立职责；基础文件和救援无需 Dataset、Knowledge 或模型可用。
- TableFS 双入口访问同一事务/命名空间权威，异步索引不能决定已提交文件是否存在。
- 实体、路径、内容版本、Dataset 记录映射和派生视图不混同；名称、身份与目录约束真实生效。
- 服务缺失、失联和不支持的错误可区分；跨域哈希/去重与私有内容不泄露。

**必要证据**

- 服务依赖和未安装环境的完整运行记录
- 三类错误的正反请求与权威状态
- 双入口事务、inode/路径/版本及索引水位对照
- 映射版本与跨域 chunk/去重泄漏负例

### AT-COMP-015｜容量、一致恢复范围及全局与子步骤状态分离

- **主规范：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)
- **要求：** `V1-STO-005`、`V1-STO-007`、`V1-REC-001`、`V1-REC-003`、`V1-IF-003`
- **阶段：** M2、M5、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 建立数据/WAL 分卷的测试数据库与另一未参与一致性钩子的应用；准备应用一致与崩溃一致两种计划、明确 scope/coordination/capture_interval 和独立恢复目标。
- RecoveryCoordinator 与目标服务使用各自持久权威，支持按 recovery_id + action_key 去重；目标包括可成功激活的卷、失联服务与有 Unknown 外部效果的 Task。
- 分别设置数据、WAL、缓存、临时、索引、日志、审计、备份及 inode 配额和恢复保留；准备稀疏、压缩、去重、复制数据用于计量核对。

**操作、审查步骤或故障注入**

1. 在数据库持续写入时按两种协议捕获多卷恢复集，阻断一个钩子或 WAL 成员，检查是否拒绝或使用事先获准且明确标记的降级。
2. 核对逻辑/实际占用、预留和各复制副本计量；注入 inode/元数据空间满、短写、I/O 错误和只读降级，观察准入、告警及保留空间。
3. 从恢复集启动协调，丢失目标回执、重发同 action_key 并让一个目标失败；核对目标服务实际权威回执而不是恢复清单中的预期状态。
4. 先使存储子操作 Activated，再保持安全新鲜度未知、另一个服务未就绪和外部效果 Unknown；查询全局 restore_phase、data_validation、security_mode、activation_scope 与业务状态。
5. 取消部分激活后的恢复，并在原 Coordinator 重启后继续查询；检查已经激活范围、残留效果和独立业务状态不会被统一覆盖。

**通过条件及结论边界**

- 一致性只用 crash_consistent/application_consistent 配合实际 scope/协调协议；未参与应用不获得应用一致性承诺，未协调捕获间隔如实披露。
- 配额、预留、稀疏逻辑大小和真实占用分别准确；满盘拒绝新分配但保留诊断/恢复，不自动删除用户卷。
- 恢复动作幂等、分目标部分进度可查，没有虚构跨服务全局事务；Coordinator 不替代 Task、消息或审批权威。
- 存储 Activated 不等同全局恢复完成；数据校验、安全模式、效果知识和可激活范围分别报告，Unknown 不被取消或恢复成功清除。
- 每个 I/O 故障关联具体资源与 Operation，真实效果与历史业务生命周期不因恢复展示便利而被重写。

**必要证据**

- 多卷成员/版本/水位、钩子与捕获区间及应用校验
- 逐类容量/预留/物理占用与 inode/I/O 故障证据
- recovery_id/action_key 请求、去重、outbox 和目标权威回执
- 部分激活/取消/重启的多维状态及残留效果对照

### AT-COMP-016｜应用身份、实例健康及开发者完整生命周期

- **主规范：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)
- **要求：** `V1-APP-001`、`V1-APP-004`、`V1-DEVX-001`、`V1-IF-003`
- **阶段：** M2、M3、M4、M6
- **验证方式：** Contract、Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 准备固定 release_spec_ref/制品摘要的 ApplicationRelease、两个 Deployment 版本、可重复启动的 Runtime，以及隔离 PackageEnvironment 和开发 SDK 模板。
- 为开发与生产分别建立身份、Secret、域名、状态和制品；配置可失败的 readiness/health 探针、受授权调试端口及保留数据卷。

**操作、审查步骤或故障注入**

1. 按模板完成安装、运行、升级、数据迁移、卸载保留数据与恢复；验证导入产物归 Application 权威，PackageEnvironment 不修改宿主依赖/包数据库。
2. 反复重启和复用宿主 PID，检查 Application 稳定 ID、不可变 Release 摘要、Deployment 代次和 RuntimeInstance 启动身份分别保持正确。
3. 使进程存在但 readiness/health 失败、进程退出但已安装制品存在，以及安装/升级中断，逐入口查询安装操作、Deployment、desired_state、Runtime、health 和 readiness。
4. 用 SDK/CLI/Shell/Agent 对同一任务执行幂等重发、分页/watch、取消、Unknown 和能力协商，检查结构类型、稳定错误与权限边界一致。
5. 扫描模板的默认管理员 token；尝试用开发凭据/调试端口访问生产域，制造依赖冲突和实验接口版本不兼容，检查隔离或明确拒绝。

**通过条件及结论边界**

- 应用、发布、部署、实例和包导入各有清楚身份与权威；重启不改应用 ID，PID/标签不替代真实实例或制品摘要。
- 安装、进程、健康和就绪状态独立；旧 Running 记录没有证据时不能自动变成 Healthy/Ready。
- 开发到恢复链路保留数据与真实权限，依赖冲突不通过改宿主公共环境解决。
- SDK 各入口复用共同合同，示例仅使用 SecretRef 和最小权限；实验接口明确不稳定，调试与生产身份/状态隔离。
- 同名 Succeeded/Cancelled/Stopped 仅用于所属对象，不让某局部成功掩盖应用未就绪或恢复未完成。

**必要证据**

- Application/Release/Deployment/Runtime 及 PackageImport 关系图和版本记录
- 生命周期操作、数据恢复校验及探针失败状态
- SDK/多入口请求和错误对照
- 模板秘密检查、调试端口和跨环境拒绝记录

### AT-COMP-017｜NAT 主动连接、双向身份验证及单 Controller 滚动升级

- **主规范：** [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md)
- **要求：** `V1-NET-004`、`V1-CLU-003`、`V1-CLU-010`、`V1-CLU-012`
- **阶段：** M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 在受控 NAT/代理后配置主动连接 Worker，与单 Controller 的明确版本组合互连；具备真实信任根、短期 bootstrap 及可轮换正式节点身份。
- 准备本地独立任务、需要在线审批的任务、共享卷写者和已发出的外部副作用；可分区网络并保留 Worker 进程继续运行。
- 准备兼容及不兼容的控制/Worker/检查点版本，升级后设备/驱动/Runtime/fence 能力可变化。

**操作、审查步骤或故障注入**

1. 建立主动连接，使用伪造 Controller、错误节点受众、过期 bootstrap 和轮换旧凭据进行互证负例；核对连接成功并不自动授予未分配资源。
2. 关闭 Controller 或隔离网络，查询 Member.Unreachable，并检查本地网络配置、日志和已有合法本地服务仍可用；共享写或需要权威新提交的调度停止。
3. 将节点置 Draining，在现有工作到安全点或完成资源 fence 前尝试发新 claim 和释放资源；强制 Left 后清点未完成负载、密钥、卷与外部效果。
4. 逐节点滚动升级，故意发送未知任务/不兼容检查点；重启后重新协商设备、驱动、Runtime 和 fence 能力，检查旧能力广告被失效。
5. 让旧 Worker 重新加入，核对恢复代次、正式身份和资源端写权限；演示单 Controller 中断期间的新调度暂停与恢复后的对账。

**通过条件及结论边界**

- NAT/代理只是传输方式；节点与集群身份都经验证，bootstrap 不成为永久通用 API Key，正式权限可轮换且按资源受众限制。
- 断联不扩权；Unreachable 不推断进程停止，合法本地服务继续不等于集群能继续权威提交。
- Draining 停止新 claim，只有安全点或资源端 fence 后可转交；Left/Ready 不清除残留效果或自动恢复旧写权。
- 不兼容任务不被领取，节点仍可本地管理；升级重新验证能力与检查点，不用旧广告冒充新版本可用。
- 单 Controller 只报告实测可用性和中断范围，不宣称 HA；未来共识、多 Controller 需要独立实验。

**必要证据**

- NAT/代理拓扑、版本与双向身份正反握手
- Controller 停止/分区下成员、claim、租约和真实写入时间线
- Drain/Left/重加入及残留清单
- 滚动升级兼容矩阵、检查点拒绝和能力重协商记录

### AT-COMP-018｜观测过载、强制审计失败关闭与可操作告警

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-OBS-002`、`V1-OBS-003`、`V1-OBS-005`
- **阶段：** M2、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 为诊断日志、指标、trace 和审计分别配置用途、保留、CPU/内存/磁盘/出口预算，准备高影响测试 Operation 和本机受控救援入口。
- 准备可控存储/inode 满、备份过期、证书临期、队列积压、Unknown、旧索引、撤权未完成、GPU/OOM、时钟异常与审计损坏的测试夹具及确定性手册。

**操作、审查步骤或故障注入**

1. 产生大量高基数 ID 和可采样 trace，断开集中导出端并耗尽观测配额，检查日志/trace 丢弃量、指标标签、普通工作负载和本机诊断。
2. 在高影响 Operation 执行前后分别阻断审计持久写，检查先记录意图后执行、真实结果关联政策版本，以及无法保存必要审计时的新变更准入。
3. 依次触发全部告警夹具，制造重复、短暂恢复再复发及长期持续故障，检查去重、抑制和恢复通知是否仍显示实际持续原因。
4. 从每条严重告警只用手册及本机工具定位资源/Operation 和实施获准处置；禁用 Agent 和远程告警后重复关键诊断。
5. 核查 Live/Ready/Degraded/Unavailable 与真实读写能力，验证普通追加日志的完整性声明没有扩大成抵御恶意 root。

**通过条件及结论边界**

- 四类数据不互相替代；高基数 ID 不造成无限指标标签，观测过载有界且导出失败不阻塞普通负载。
- 必要审计不能被降采样；写不了审计时新高权限效果被拒绝，已有/未知效果和审计丢失如实记录，本机受控救援可用。
- 所有列出的严重条件都有实际触发、资源关联与确定性手册；抑制不隐藏持续故障，恢复通知以真实恢复为依据。
- 健康状态以声明探针和资源观察支撑；进程存活不被等同服务可写，Agent 解释不是唯一修复路径。

**必要证据**

- 四类数据预算、关联键及高基数负载曲线
- 审计持久提交点与真实资源效果时间线
- 全告警条件的触发/抑制/恢复与手册执行记录
- 断集中观测和无 Agent 时的本机诊断证据

### AT-COMP-019｜更新信任轮换、泄漏、离线验证与时间失信

- **主规范：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)
- **要求：** `V1-REL-003`
- **阶段：** M1、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 选定并固定更新验证方案及具体版本，准备合法、过期、撤销、错误签名和旧元数据包；信任根与签名密钥在隔离测试域。
- 具备独立恢复材料、密钥轮换/泄漏/丢失的应急计划，以及可断网、回拨时间和中断信任根提交的实验环境。

**操作、审查步骤或故障注入**

1. 在在线和离线安装路径验证真实制品字节、签名链、通道、元数据期限和可信时间；提交过期、撤销及重放旧元数据包。
2. 执行签名密钥正常轮换及信任根更新，在持久提交前后中断并重启，核查新旧验证材料和明确恢复目标。
3. 模拟签名密钥泄漏/丢失，按应急计划撤销并恢复合法发布验证；尝试使用被撤销旧材料重新签发或更新。
4. 令受信时间无法取得或回拨、离线材料不完整，检查更新拒绝/隔离/救援行为；查验声明没有因提及 TUF/SLSA 就标记认证等级。

**通过条件及结论边界**

- 不受信、过期、撤销或来源不符的更新不能进入可执行部署；离线路径保留必要来源与期限验证。
- 轮换中断后有可恢复、可审计的信任状态；不通过关闭签名或时间检查完成更新。
- 泄漏/丢失演练证明既定应急范围，撤销旧材料后重放失败；救援不恢复旧 Grant 或旧信任权限。
- 时间不可证明时准确拒绝或按明确受限恢复流程处理，不能静默忽略元数据过期。

**必要证据**

- 固定方案/版本、测试根与材料摘要
- 在线/离线验证正反例及真实制品校验
- 轮换提交点中断和应急恢复账本
- 失信时间、撤销重放与受限救援记录

### AT-COMP-020｜结构合法但引用或授权无效时的运行时拒绝

- **主规范：** [docs/16-interface-catalog-and-glossary.md](../docs/16-interface-catalog-and-glossary.md)
- **要求：** `V1-IF-004`
- **阶段：** M0、M2、M3、M6
- **验证方式：** Contract、Integration
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 为 WorkflowDefinition、RecoverySetManifest、ExecutionPermit、ApplicationManifest/ReleaseSpec 分别准备 Schema 通过的正常对象及带合法形状但失真引用的对象。
- 失真分支覆盖：不存在/错摘要/错版本/无读取权的资源、未知 AST 引用或权限、伪造来源、过期许可/策略代次及不可查的 RuntimeVerified 证据。

**操作、审查步骤或故障注入**

1. 先按固定 Schema 验证形状，再经真实领域入口解析所有必要 ResourceRef 并检查对象类型、存在性、完整性、来源和当前权限，记录两个验证层的不同结果。
2. 提交字段结构合法但图引用/类型/有界性或步骤权限无效的 Workflow，检查编译或准入阶段在任何派发前拒绝。
3. 提交可解析但来源未认证或对象缺损的恢复清单，以及结构合法但伪造、过期、错误 policy_epoch/operation/plan 的 ExecutionPermit，尝试恢复激活和实际执行。
4. 让 ApplicationManifest 的 release_spec_ref 缺目标、指向错类型或摘要不符，再提供完整 ReleaseSpec 但虚构兼容 evidence_ref，尝试形成 Deployment 或支持声明。

**通过条件及结论边界**

- Schema 通过仅是结构检查；任何关键引用、来源、编译、权限或当前前提无效时不发生实际派发、恢复激活或副作用。
- Workflow 的图和类型/权限验证、恢复清单真实性、许可真实性/新鲜度分别由对应权威验证。
- ApplicationManifest 必须解析并验证完整 ReleaseSpec 才可形成 Deployment；虚构 RuntimeVerified 证据不成为实际兼容结论。
- 拒绝原因可诊断且不泄露跨域对象，文档示例的占位摘要和身份不会被误当可用认证材料。

**必要证据**

- 结构通过的失真夹具及分层验证结果
- 真实引用解析、类型/权限/摘要/来源拒绝记录
- 领域准入和资源端无效果证明
- Application ReleaseSpec 与兼容证据查验记录

### AT-COMP-021｜按阶段扩展的可恢复最小业务闭环

- **主规范：** [docs/14-roadmap-and-decisions.md](../docs/14-roadmap-and-decisions.md)
- **要求：** `V1-PLAN-002`
- **阶段：** M1、M2、M3、M4、M6
- **验证方式：** Integration、FaultInjection
- **状态：** NotRun；实现 NotVerified

**前置条件**

- 只选择一个确切 VM/硬件 Profile、一个真实有状态 OCI 应用和受控数据卷，固定制品、身份、授权、预算、备份介质和空白隔离恢复目标。
- 按顺序准备原生 Agent、一个版本固定的第三方 Agent、Chat/最小 Workflow，以及单 Worker/一个 GUI Profile；Data Services 和重型 Project/Studio 均可不安装。

**操作、审查步骤或故障注入**

1. 首先由本机用户发起计划，经 Policy、持久 Operation/Task 运行 OCI 应用写授权卷，完成审计查询、独立备份及在空白目标恢复，并核验实际业务数据。
2. 在基础链路注入容器不可用、坏网络、旧安全备份和旧程序写新 Schema，证明独立管理、隔离恢复及拒绝不兼容写入仍成立。
3. 第二阶段让 CLI、原生及第三方 Agent 执行同一受限任务，注入授权冲突、预算满、外部 ACK 丢失及取消，核对 Unknown 和效果账本不被入口差异掩盖。
4. 第三阶段以群聊/DM 有界触发 Workflow，注入重复消息、CancelOrigin 先到、审批并发和跨库恢复；核查同一业务派发与审批使用。
5. 最后才引入 Worker 分区和 GUI 人类接管，验证旧写拒绝/不能 fencing 时停止迁移及输入权限交接；逐阶段保存门槛和明确未扩展范围。

**通过条件及结论边界**

- 首个闭环真实完成从授权计划到业务数据独立恢复；没有同时引入未验证 HA、全部包或 TableFS 才能运行的隐含前提。
- 每一步扩展保持此前的权限、预算、持久性、恢复和未知结果不变量；失败阻止该阶段支持声明，不倒推之前未测组合也通过。
- 可选模型/Project/Dataset 不成为基础管理和恢复前提，第三方与原生路径采用同一受限工具合同。
- 聊天/工作流重复和跨库恢复不重复副作用；Worker/GUI 扩展以资源端真实写入和输入停止证据为准。

**必要证据**

- 各阶段固定范围、进入/退出门槛及制品清单
- 真实有状态应用业务校验和独立恢复证据
- 跨入口授权/预算/Unknown 及消息/审批派发账本
- Worker 写端 fence、无法迁移拒绝和 GUI 接管证据
