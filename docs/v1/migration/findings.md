# v0.x → v1 问题与修正台账

共89项独立登记：P0 40项、P1 45项、P2 4项。它们包括原文错误、含义歧义及需要补成可执行合同的设计缺口，不是已确认的源码漏洞数量。

每条`CorrectedInV1`仅说明本包文字或定义已修正/边界已明确；实现均为`NotVerified`，相关产品验收均需按acceptance执行。缺失历史报告无法由文字修订补回，台账保留其证据缺口。

| ID | 优先级 | 类型 | 问题 | 修正位置 |
|---|---|---|---|---|
| FIX-CORE-001 | P1 | 缺口 | 输入材料不是一份完整、可独立使用的同版本规范：仅加载名为v0.6的Project合集会缺少Space独有章节和12个具体WF验收定义，检索内嵌旧版标题又可能把历史语境当作当前独立规范。原导入版已披露差异，本项是… | [migration/v0-to-v1.md](../migration/v0-to-v1.md) |
| FIX-CORE-002 | P1 | 歧义 | 已有来源优先级说明，但缺少一套发布包内正文、Schema、ADR和不同阅读副本发生冲突时必须同步修订并阻断契约冻结的规则。同一版本的不同入口可能长期携带不同有效内容，使实现者按入口选择契约。 | [docs/00-reading-and-authority.md](../docs/00-reading-and-authority.md) |
| FIX-CORE-003 | P1 | 错误 | 相同P0-04/P0-05在两份现行来源中指向不同工作，按裸编号导入会错误合并或覆盖任务。P0-03也只有部分范围重叠，不能取其中一份后宣称覆盖两份。 | [migration/id-conflicts.md](../migration/id-conflicts.md) |
| FIX-CORE-004 | P2 | 错误 | ADR-22/22是重复编号笔误，无法准确表达Chat与Workflow两个并行接口草案，可能让Chat决策遗漏或继续传播旧编号错误。 | [migration/id-conflicts.md](../migration/id-conflicts.md) |
| FIX-CORE-005 | P2 | 错误 | 29是v0.4阶段的过期计数，若据此计算覆盖率会漏计v0.5新增的XAT-30～37；v0.6又增加38～45，不能继续沿用29作为当前分母。 | [migration/README.md](../migration/README.md) |
| FIX-CORE-006 | P1 | 歧义 | Chat/Workflow编号已纠正，但旧ADR-22的宽范围与新增ADR-23仍有Project/Repo/CI责任重叠；没有拆分说明时，两个ADR可能分别决定同一外部字段权威或重复选择同步方案。 | [docs/00-reading-and-authority.md](../docs/00-reading-and-authority.md) |
| FIX-CORE-007 | P1 | 缺口 | 对象概念已提出，跨模块外部字段命名和若干共同类型却未形成唯一机器契约；客户端可能把camelCase/snake_case别名当两个字段，或对resourceVersion同时采用可排序数字与opaque值。这… | [docs/02-common-contracts.md](../docs/02-common-contracts.md) |
| FIX-CORE-008 | P1 | 缺口 | 通用计数若默认映射为JavaScript number，超过安全整数范围后会丢失低位，使不同值相等、大小比较错误或计量不准。此项针对全API的整数传输类型，不重复审批摘要算法问题，也不把Dataset任意精度J… | [docs/02-common-contracts.md](../docs/02-common-contracts.md) |
| FIX-CORE-009 | P1 | 缺口 | 只返回一个revision仍不足以构成完整初始同步：每页读取最新数据时，并发插入/删除可导致重复或遗漏；若最后一页后才取watch起点，中间变更可能永久缺失。本项是通用资源列表协议，区别于消费者inbox事务和… | [docs/02-common-contracts.md](../docs/02-common-contracts.md) |
| FIX-CORE-010 | P1 | 歧义 | “单调时钟”并不唯一决定挂起期间是否耗尽期限。若应持续到期的许可使用不计系统挂起时间的CLOCK_MONOTONIC，设备睡眠后可能保留本应消耗的授权/租约余量；原有墙钟回拨和重启规则不能单独消除这个差别。 | [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md) |
| FIX-CORE-011 | P1 | 歧义 | 更新专用状态中仍把“需要恢复”与实际执行阶段/回退完成并列，缺少明确的机器映射，无法准确表示停在Staged还是TrialBoot、是否已回退以及同时需要人工恢复。此项只处理Update领域，不重复Task的O… | [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md) |
| FIX-CORE-012 | P0 | 缺口 | 流水线逻辑上分阶段并不保证来自PR的任意构建/测试代码无法读取同执行器中的发布Secret或宿主挂载。若沿用有高权限凭据的runner，即使最终签名另有审批，不可信代码仍可能事先窃取凭据或修改待签制品。 | [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md) |
| FIX-CORE-013 | P1 | 缺口 | 本次静态解析发现29处相对报告链接没有归档目标，部分历史验证说明无法仅靠此ZIP完整复核。现存XenonLab-Old报告不能自动替代缺失的上一轮XeLab报告。 | [migration/xelab-reuse-boundary.md](../migration/xelab-reuse-boundary.md) |
| FIX-CORE-014 | P1 | 歧义 | 该ZIP是多来源历史模块集合，缺少Git对象/历史和当前远端验证，不能把任一元数据SHA当作整包唯一HEAD。内包摘要与自带metadata相同，只能证明归档内部清单一致，不能证明当前仓库来源或PsiOS v1… | [migration/xelab-reuse-boundary.md](../migration/xelab-reuse-boundary.md) |
| FIX-CORE-015 | P1 | 缺口 | 写入fencing正确也不保证观察缓存正确。恢复数据库可能把revision退到客户端已见版本之前，旧watch/成员/调度缓存若继续等待更高revision，可能长期遗漏删除或维持不再存在的资源投影。此项关注… | [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md) |
| FIX-SEC-001 | P0 | 歧义 | 系统审批与 Workflow 请求缺少唯一权威和引用关系，可能产生双审批事实。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-002 | P0 | 缺口 | 未定义同一单次审批被两个并发 Operation 消费时的原子性。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-003 | P0 | 缺口 | 检查与真正副作用之间的竞争窗口未规定可保证的顺序，可能把检查完成误当作权限仍有效。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-004 | P0 | 缺口 | 摘要的编码与解释规范未定义，重复JSON键、数值精度或跨语言序列化可造成批准内容与执行内容不一致。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-005 | P1 | 歧义 | 外部认证账户与Principal的绑定键未规定，按邮箱/显示名合并可能串身份。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-006 | P0 | 缺口 | 子代理范围缩减已有原则，但预算分配如果各自复制完整父额度仍可放大总消耗。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-007 | P0 | 歧义 | 把可组合的强制机制和不可逆效果混成一个互斥枚举，无法描述多种能力同时存在。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-008 | P0 | 缺口 | 没有规定控制面受理与逐目标强制完成的聚合条件，可能在尚有失联端时虚报完全撤权。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-009 | P1 | 缺口 | 通用Shell管理会话的固定会话、注销、空闲/绝对期限、管理与应用同源隔离缺少闭合契约。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-010 | P1 | 缺口 | 消息、模型输出和诊断内容进入管理WebUI时的主动内容执行没有独立展示契约。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-011 | P1 | 缺口 | 高影响远程身份/信任根变更的新鲜认证与业务审批关系不明，可能以长期被盗会话完成敏感动作或导致每步重复确认。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-SEC-012 | P0 | 歧义 | 授权同时出现于两类状态域，恢复时可能产生两个可写权威。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-013 | P0 | 歧义 | 全局ReEnrolled容易被解释为已找回撤权历史或所有外部凭据已撤销。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-014 | P0 | 缺口 | 最小本地认证若直接使用旧备份管理员列表，仍可能恢复已撤销管理员权限。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-015 | P0 | 缺口 | 多数据库及对象池没有统一恢复清单，独立快照可能构成相互矛盾的权威切面。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-016 | P0 | 缺口 | 安全状态新鲜不能证明旧任务检查点之后没有外部副作用，灾难恢复可能重复PR/部署/发送。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-017 | P0 | 缺口 | 正常重试窗口与更长灾难恢复窗口未联合约束，恢复旧快照后旧键可能已被删除。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-018 | P0 | 歧义 | 确认消息不能丢失的措辞未限定故障模型，单副本落盘无法无条件抵抗整盘损毁和恢复旧快照。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-019 | P0 | 缺口 | 元数据恢复成功但对象池缺少结果/附件的关系修复和激活语义不完整。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-020 | P0 | 歧义 | 恢复中部分激活后失败/取消，单一恢复失败标签不能描述实际数据写入和未知效果。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-021 | P1 | 缺口 | 旧备份恢复可能重新发布已删除数据或共享，尤其缺失当前删除水位时不能声称完整保留历史删除。 | [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md) |
| FIX-SEC-022 | P1 | 缺口 | 审计、审批使用、效果记录与去重墓碑的回收缺少共同不变量，清理可能重新开放旧请求。 | [docs/04-security-identity.md](../docs/04-security-identity.md) |
| FIX-STA-001 | P1 | 歧义 | 单一枚举把生命周期、附件存在性、健康和有效访问模式混为互斥状态，无法清楚表示已挂载但只读/退化的卷。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-002 | P0 | 歧义 | 没有明确RWO的节点挂载限制和单个资源写入权威的区别，可能让同节点多进程或旧Writer绕过单写假设。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-003 | P0 | 缺口 | 缺少并行克隆与原目标恢复的身份分配规则，旧句柄或ACL可能误绑定恢复后的同名资源。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-004 | P0 | 歧义 | 持久化成功缺少进程崩溃、断电、单盘和整机损毁的适用故障边界，易被误解为唯一介质毁坏也能零损。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-005 | P1 | 缺口 | 内容校验与来源认证/安全新鲜度未在BackupManifest中分开，篡改者可同时替换内容和未认证校验表。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-006 | P1 | 歧义 | 主字段仍未落到可实施类型，score=0与未评分、vector缺失等可被不同后端解释。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-007 | P1 | 缺口 | missing/null、重复键、字符串数字和超2^53整数等语义不明，无法保证跨引擎精确查询。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-008 | P1 | 缺口 | 仍缺score/vector在原文修改并发下的明确失效和CAS写入规则，旧计算结果可能贴到新内容。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-009 | P1 | 歧义 | 固定版本内容和固定历史授权容易被混用，固定View需要的保留关系也未完全绑定。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-010 | P0 | 缺口 | 仅说明失效仍不足以处理FUSE writeback缓存与绕开内核的Catalog写入；dirty page可能覆盖表侧新版本。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-011 | P1 | 缺口 | 没有定义非UTF-8名称的可逆表示和分页水位，可能导致文件名损坏、目录移动时遗漏/重复或循环。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-012 | P0 | 缺口 | 静态引用清单不足以防GC扫描后并发建立引用或在途发布；仅TTL释放pin也不能证明Writer停写。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-013 | P0 | 歧义 | 原则未明确FUSE挂载检查者、allow_other与default_permissions关系以及已打开fd/mmap残留，容易把新请求ACL等同既有数据面撤权。 | [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md) |
| FIX-STA-014 | P1 | 歧义 | 安装操作、部署选择、进程状态、健康混为单一状态，崩溃和回退后无法准确表示已安装但未运行或运行未就绪。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-015 | P0 | 缺口 | 锁失效后的接管尚未明确旧迁移可能仍写入，仅重取锁可能让两个迁移者同时修改同一数据域。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-016 | P0 | 缺口 | 缺少启动阶段对实际权限及只读root挂载需求的验据边界，Runtime接受配置不总等于所有限制已生效。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-017 | P0 | 缺口 | 隔离要求只明确到脚本，未充分覆盖解析/解压自身的路径穿越、恶意链接和解压资源耗尽。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-018 | P1 | 歧义 | 未明确不同包生态信任入口区别，容易把自行计算哈希或并不存在的统一单包签名当认证。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-019 | P0 | 缺口 | 构建环境与生产凭据/网络出口未作具体隔离，受限脚本仍可能借宽外连窃取挂载秘密。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-020 | P0 | 缺口 | 只按路径代理不足以隔离不可信应用脚本、Cookie、Service Worker和管理会话；任意URL代理还可能产生SSRF。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-021 | P0 | 歧义 | 原则正确，但尚缺捕获范围与输入限制独立能力声明；窗口捕获成功可能被泛化为完整单应用安全。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-022 | P0 | 缺口 | 清空待发送输入不能处理已按下按键/按钮、IME组合、拖拽或ACK丢失后的重复文本提交。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-023 | P1 | 歧义 | 没有写清pipewire-serial是ScreenCast接口v6字段，可能把旧后端误判为协议违规或把serial当永久授权。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-024 | P1 | 缺口 | 未区分独立ScreenCast与组合RemoteDesktop会话的Start/Stop和持久化权威，可能错误消费两套token。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-025 | P1 | 缺口 | 未具体规定EIS连接与Notify输入互斥，适配器可能重复注入或违背后端协议。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-026 | P0 | 缺口 | 只要求轮换没有定义远端已消费但新令牌响应/持久化丢失的崩溃窗口，重试可能重复或错误恢复。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-STA-027 | P1 | 歧义 | 用户界面可分项但部分真实后端权限同时开放多类音频访问，可能虚构后端并未提供的分权。 | [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md) |
| FIX-ORC-001 | P1 | 歧义 | 将不同操作的字段放入同一栏，未区分创建请求尚无 task_id 与服务端返回已分配 ID，容易产生客户端伪造身份或无从构造合法请求的实现。 | [docs/08-tasks-events.md §8.2](../docs/08-tasks-events.md) |
| FIX-ORC-002 | P0 | 缺口 | 已有对账原则，但未区分权威确认不存在、索引已过期、滞后副本查不到与灾备不完整；普通 NotFound 可能被误用为再次提交的证据。 | [docs/08-tasks-events.md §8.2–8.3](../docs/08-tasks-events.md) |
| FIX-ORC-003 | P0 | 歧义 | origin_ref 的签发权威、租户/主体范围和摘要覆盖范围尚未形式化，可能跨安全域碰撞，或在复用旧键时偷偷改变目标、输入和预算。 | [docs/08-tasks-events.md §8.2](../docs/08-tasks-events.md) |
| FIX-ORC-004 | P0 | 缺口 | 持久 DispatchIntent 解决接受回执裂缝，但未完整定义取消与尚未确认提交并发时的串行化；删除本地 outbox 无法阻止已在网络中的 Submit。 | [docs/08-tasks-events.md §8.3；docs/11-workflows-projects.md §11.6](../docs/08-tasks-events.md) |
| FIX-ORC-005 | P1 | 歧义 | 已有 Task/Run 区分，但 Workflow 业务重试、Task 内部 Run 重试、网络重发与循环迭代的身份变更边界没有完整定义，可能导致重复派发或错误复用结果。 | [docs/08-tasks-events.md §8.4；docs/11-workflows-projects.md §11.3](../docs/08-tasks-events.md) |
| FIX-ORC-006 | P0 | 歧义 | OutcomeUnknown 与生命周期之间的正交关系没有机器级定义；Cancelled 或 Failed 后仍可能存在未知外部效果，Completed 又可能被误解为成功。 | [docs/08-tasks-events.md §8.4；docs/11-workflows-projects.md §11.4](../docs/08-tasks-events.md) |
| FIX-ORC-007 | P0 | 缺口 | 旧 Run 结果应被 fencing 拒绝，但原文未区分拒绝其修改当前状态与保存其外部成功证据；直接丢弃迟到回执会隐匿真实副作用。 | [docs/08-tasks-events.md §8.5](../docs/08-tasks-events.md) |
| FIX-ORC-008 | P0 | 缺口 | 输入/附件保留与业务记录可能属于不同服务；仅说建立保留关系不足以防止接受后内容被回收，或失败上传永久占用空间。 | [docs/08-tasks-events.md §8.2、8.6；docs/10-conversations.md §10.8](../docs/08-tasks-events.md) |
| FIX-ORC-009 | P1 | 缺口 | 已禁止依靠游标承诺 exactly-once，但未逐项约束 inbox 标记、业务状态与游标提交的先后；先标已处理再执行业务会丢工作，反向只记游标会重做。 | [docs/08-tasks-events.md §8.7](../docs/08-tasks-events.md) |
| FIX-ORC-010 | P1 | 缺口 | 已有预算要求，但没有声明并行预留、父子共享账本、重试不重置根预算和在途账单延迟；多个分支可同时读到余额后超发。 | [docs/08-tasks-events.md §8.8；docs/09-agents-models-protocols.md §9.4；docs/10-conversations.md §10.7](../docs/08-tasks-events.md) |
| FIX-ORC-011 | P1 | 错误 | 官方 MCP 2026-07-28 规范要求服务器实现 server/discover；可选的是客户端是否预先调用发现。原表达没有区分两者，会允许不合规服务器自称支持该基线。 | [docs/09-agents-models-protocols.md §9.7](../docs/09-agents-models-protocols.md) |
| FIX-ORC-012 | P1 | 缺口 | 未定义现代 MCP task completed 的业务含义；官方允许 completed 结果携带 isError:true，直接映射本地成功可能错误释放合并/部署等后继。 | [docs/09-agents-models-protocols.md §9.8](../docs/09-agents-models-protocols.md) |
| FIX-ORC-013 | P1 | 缺口 | 协议定位正确，但未说明 A2A Send Message 的发送幂等为服务器可选能力；仅携带 messageId 仍不足以安全重发有副作用请求。 | [docs/09-agents-models-protocols.md §9.8](../docs/09-agents-models-protocols.md) |
| FIX-ORC-014 | P2 | 歧义 | 模型版本可固定的强度依赖提供者；不公开权重修订的云服务无法由 PsiOS 生成可验证不可变版本，若统一宣称精确重放会超出可知事实。 | [docs/09-agents-models-protocols.md §9.2](../docs/09-agents-models-protocols.md) |
| FIX-ORC-015 | P1 | 歧义 | 原示意消息混合服务端身份、正文、任务关联，未给出独立发送请求；客户端幂等身份与服务端 message_id 的关系、摘要覆盖范围不清楚。 | [docs/10-conversations.md §10.3–10.4](../docs/10-conversations.md) |
| FIX-ORC-016 | P0 | 缺口 | 每次授权原则尚未定义移除成员与发送并发的线性化边界；先检查成员、排队后再提交可能在移除生效后写入。 | [docs/10-conversations.md §10.2](../docs/10-conversations.md) |
| FIX-ORC-017 | P1 | 缺口 | 正文删除与去重元数据 GC 后，旧发送键可能重新成为未知键；仅保留一个有限去重窗口无法长期阻止历史重放，同时无限保存正文又违反保留目标。 | [docs/10-conversations.md §10.4](../docs/10-conversations.md) |
| FIX-ORC-018 | P1 | 缺口 | 退出后重新加入仍是同一 Principal，但不是同一次授权关系；投递/处理去重若只绑定主体与消息，可能继承旧读取权限或吞掉新合法处理。 | [docs/10-conversations.md §10.2、10.6](../docs/10-conversations.md) |
| FIX-ORC-019 | P1 | 歧义 | Accepted、通知投递、已读、处理完成与关联 Task 成功之间缺少明确事实表，单一 ACK 字段容易被多个消费者解释为全部完成。 | [docs/10-conversations.md §10.6](../docs/10-conversations.md) |
| FIX-ORC-020 | P1 | 歧义 | 发送者撤销代次、成员修订和写者 fencing 代次未给出明确分离；且单次持久 ACK 的故障范围未区分进程崩溃与全盘丢失，恢复旧序号可能形成重复确认。 | [docs/10-conversations.md §10.3、10.5](../docs/10-conversations.md) |
| FIX-ORC-021 | P2 | 歧义 | 若原地把 DM 扩员，其既有一对一语义、历史可见边界和第三人权限可能同时改变；原文仅要求额外检查，没有明确对象身份迁移规则。 | [docs/10-conversations.md §10.1–10.2](../docs/10-conversations.md) |
| FIX-ORC-022 | P1 | 缺口 | 有界原则正确，但尚无结构化 loop 展开、稳定迭代身份、Skipped 与 Join 规则；AnySuccess 也不能消除仍在写入的败者效果。 | [docs/11-workflows-projects.md §11.2–11.4、11.6](../docs/11-workflows-projects.md) |
| FIX-ORC-023 | P0 | 缺口 | 审批请求、不可变判定、Workflow 进度和一次性使用账本没有完整区分；两个并行操作可能消费同一单次批准，或旧门禁状态绕过执行时撤权。 | [docs/11-workflows-projects.md §11.5](../docs/11-workflows-projects.md) |
| FIX-ORC-024 | P1 | 歧义 | 更高版本未区分提供者修订类型；ETag、Git SHA 和随机版本并不天然可排序，按字典序或到达时间选最新会覆盖正确事实。 | [docs/11-workflows-projects.md §11.9](../docs/11-workflows-projects.md) |
| FIX-ORC-025 | P0 | 缺口 | 绑定版本原则尚未完整覆盖 CI/Review 检查身份、尝试号与最终合并的 TOCTOU；先读当前 head 再无条件合并仍可能越过已审目标。 | [docs/11-workflows-projects.md §11.5、11.9–11.10](../docs/11-workflows-projects.md) |

## 逐项证据与处理

### FIX-CORE-001｜P1｜缺口

**原文依据：** Project 01_PsiOS_统一规范_v0.6_项目导入版.md 开头明确说“它不是 Space 统一 Page 的逐字导出”；其后内嵌完整 v0.5 旧标题和正文。Space v0.6 第35–37章另含跨模块接口/状态矩阵、WF-AT-01～12、实施队列与维护规则。

**问题：** 输入材料不是一份完整、可独立使用的同版本规范：仅加载名为v0.6的Project合集会缺少Space独有章节和12个具体WF验收定义，检索内嵌旧版标题又可能把历史语境当作当前独立规范。原导入版已披露差异，本项是完整交付所需补齐的材料范围，不声称原文隐瞒了差异。

**v1修正：** 将实际取得的Project各版、Space完整正文及风险增量共同纳入v1；当前正文按领域重写，历史全文放入只读归档；列明35–37章缺章/部分摘要的具体关系，逐项保留旧规范和验收映射。

**位置：** [migration/v0-to-v1.md](../migration/v0-to-v1.md)

**验证方向：** 对照两个v0.6文本与原v0.5字节内容，确认Project合集确实内嵌v0.5但不是Space全文；检查WF-AT-01～12每项都有原句、来源和v1去向，原始归档SHA-256与输入一致。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-002｜P1｜歧义

**原文依据：** Project导入指南§规范优先级把“本项目v0.6风险修订”与“已批准ADR和Schema”顺序列出；Project合集开头又指向Space明确新修订和已批准ADR；Project Instructions以本项目文件为基线。各入口同称v0.6，内容并不完全一致。

**问题：** 已有来源优先级说明，但缺少一套发布包内正文、Schema、ADR和不同阅读副本发生冲突时必须同步修订并阻断契约冻结的规则。同一版本的不同入口可能长期携带不同有效内容，使实现者按入口选择契约。

**v1修正：** v1规定同一发布包只有一套当前规范；docs语义与Schema结构必须同时满足，冲突登记为缺陷并阻断冻结；全文和HTML由同源生成；未来ADR变更必须随文档版本同步正文、Schema、迁移与兼容表。

**位置：** [docs/00-reading-and-authority.md](../docs/00-reading-and-authority.md)

**验证方向：** 故意让正文枚举与Schema不同、或单独修改全文副本；包检查应拒绝一致性声明并要求修订/重新生成，不能由某个入口自动覆盖另一份当前契约。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-003｜P1｜错误

**原文依据：** Space v0.6 §35.16：P0-04为Task/Run/Event/Workflow/Message/WorkItem权威状态机ADR，P0-05为Host/OCI/Volume/Backup闭环；Project实施路线§首批P0工作项：P0-04为SecurityState受限灾难恢复，P0-05为Workflow→Task outbox/去重PoC。

**问题：** 相同P0-04/P0-05在两份现行来源中指向不同工作，按裸编号导入会错误合并或覆盖任务。P0-03也只有部分范围重叠，不能取其中一份后宣称覆盖两份。

**v1修正：** 旧工作项身份使用(source_id,legacy_id)；明确列出同号差异及范围并集，新工作采用稳定WI编号，优先级和实施阶段不再编码为工作身份。

**位置：** [migration/id-conflicts.md](../migration/id-conflicts.md)

**验证方向：** 同时导入SRC-SPACE-V06::P0-04与SRC-IMPORT-ROADMAP::P0-04，应得到两个可独立追溯的来源工作定义；对P0-05执行同样检查，确认当前WI映射不遗漏任一交付物。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-004｜P2｜错误

**原文依据：** Space v0.6 §35.16末段：“同时可并行开展 ADR-22/22 的纯接口草案”；同章§35.13及风险台账§0已经规定ADR-21=Agent Chat、ADR-22=PsiFlow。

**问题：** ADR-22/22是重复编号笔误，无法准确表达Chat与Workflow两个并行接口草案，可能让Chat决策遗漏或继续传播旧编号错误。

**v1修正：** 明确该处应引用ADR-21与ADR-22；v1固定各编号主题并保留原句供溯源，没有改写历史归档或伪造ADR批准。

**位置：** [migration/id-conflicts.md](../migration/id-conflicts.md)

**验证方向：** 核对当前ADR登记的21/22主题和路线引用；当前规范不再包含将两个独立草案写成ADR-22/22的引用，历史原句仍可从归档定位。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-005｜P2｜错误

**原文依据：** Space v0.6 §35.15尾段称“不抄写或替代v0.5现有29个跨模块场景”；上传v0.5§30.1和Space继承正文实际逐项列出XAT-01～XAT-37。

**问题：** 29是v0.4阶段的过期计数，若据此计算覆盖率会漏计v0.5新增的XAT-30～37；v0.6又增加38～45，不能继续沿用29作为当前分母。

**v1修正：** 以实际定义ID去重计数，保留45个XAT与12个WF-AT；说明全部141个旧验收的分族计数和重复版本口径，覆盖率只表示文档去向。

**位置：** [migration/README.md](../migration/README.md)

**验证方向：** 从全部原始正文提取并展开编号范围：v0.5应包含37个XAT，合并v0.6为45个XAT，总旧测试141个；每个编号须有定义原句，不能靠只匹配范围端点计算。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-006｜P1｜歧义

**原文依据：** v0.5/Space v0.6 §29.1的ADR-22主题为“PsiFlow编排、项目对象和外部仓库集成”；Space §35.13新增ADR-23“项目管理及外部系统同步”，同时另以ADR-22描述PsiFlow领域/状态与DSL。

**问题：** Chat/Workflow编号已纠正，但旧ADR-22的宽范围与新增ADR-23仍有Project/Repo/CI责任重叠；没有拆分说明时，两个ADR可能分别决定同一外部字段权威或重复选择同步方案。

**v1修正：** 保留ADR-22负责Workflow/DSL/执行恢复，将Project和外部字段权威、修订、Webhook、冲突对账交给ADR-23；明确旧ADR-22相关内容转移并保留依赖，功能目标不删除。

**位置：** [docs/00-reading-and-authority.md](../docs/00-reading-and-authority.md)

**验证方向：** 分别检查一个Workflow状态变更和一个Forgejo PR/CI事实变更的决策归属；它们应只有对应领域权威，ADR-22与23存在依赖而不各自维护可覆盖对方的外部状态。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-007｜P1｜缺口

**原文依据：** Space v0.6 API-004给出“resourceVersion/ETag”示例，CHAT-004等使用snake_case字段；API-017/020要求Schema版本与同源SDK，但多处仍把具体字段名/类型留待正式Schema固定。

**问题：** 对象概念已提出，跨模块外部字段命名和若干共同类型却未形成唯一机器契约；客户端可能把camelCase/snake_case别名当两个字段，或对resourceVersion同时采用可排序数字与opaque值。这里是旧草案待冻结的合同缺口，不是声称旧稿已经批准了错误实现。

**v1修正：** 对外JSON统一UTF-8与snake_case，resource_version明确为只做相等比较的opaque string；资源信封给出字段/来源/约束，关键请求由版本化Schema约束，未知安全字段不得被宽松忽略。

**位置：** [docs/02-common-contracts.md](../docs/02-common-contracts.md)

**验证方向：** 向同一端点分别提交resource_version、resourceVersion以及两者同时存在的请求；仅正式合同接受，旧名仅能经明确版本适配。用不可排序版本串验证客户端不按数字或字典序决定新旧。AT-CORE-019。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-008｜P1｜缺口

**原文依据：** Space v0.6 CLU-005/012描述资源fencing代次与恢复世代，§35.9/35.11使用revision/epoch等计数，API-020预期多语言CLI/SDK；旧共同API未规定计数、序号和字节数的JSON无损传输类型。

**问题：** 通用计数若默认映射为JavaScript number，超过安全整数范围后会丢失低位，使不同值相等、大小比较错误或计量不准。此项针对全API的整数传输类型，不重复审批摘要算法问题，也不把Dataset任意精度JSON收窄为同一类型。

**v1修正：** 计数/序号/epoch/字节数使用规范十进制字符串，限定无正号/前导零，必要时语义校验u64上界；解析和比较使用整数语义，不经double中转，opaque revision和随机incarnation不套用该排序。

**位置：** [docs/02-common-contracts.md](../docs/02-common-contracts.md)

**验证方向：** 跨Rust/Python/JavaScript往返测试2^53-1、2^53、2^53+1和u64最大值；相邻值必须保持不同，越界/负值/非规范写法明确拒绝，随机incarnation仍只做身份匹配。AT-CORE-019。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-009｜P1｜缺口

**原文依据：** Space v0.6 API-012：“列表返回可用于后续订阅的修订游标；游标过期需显式要求重新列举”；没有进一步规定多页列表必须读取同一切面，以及列表过程中发生的变更与watch起点如何衔接。

**问题：** 只返回一个revision仍不足以构成完整初始同步：每页读取最新数据时，并发插入/删除可导致重复或遗漏；若最后一页后才取watch起点，中间变更可能永久缺失。本项是通用资源列表协议，区别于消费者inbox事务和TableFS名称编码。

**v1修正：** 列表返回固定快照身份、绑定该快照的分页游标及同权威watch起点；服务端保留起点之后事件或提供等效屏障，所有页完成后连续接续watch；游标失效明确重新同步，并处理删除对象。

**位置：** [docs/02-common-contracts.md](../docs/02-common-contracts.md)

**验证方向：** AT-CORE-006：在第一页到末页之间持续插入/修改/删除，令部分分页延迟并在转watch时断连；最终投影应与权威集合一致，无未覆盖变更。快照过期必须显式失败，不能以空页完成同步。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-010｜P1｜歧义

**原文依据：** Space v0.6 FND-06和CLU-007已要求合适单调钟、boot_id/持久代次及重启不延长期限；DEV-05要求睡眠/休眠恢复后重新检查会话与租用，但未明确哪些期限必须计入系统挂起时间及对应Linux时钟选择。

**问题：** “单调时钟”并不唯一决定挂起期间是否耗尽期限。若应持续到期的许可使用不计系统挂起时间的CLOCK_MONOTONIC，设备睡眠后可能保留本应消耗的授权/租约余量；原有墙钟回拨和重启规则不能单独消除这个差别。

**v1修正：** 按用途声明期限语义：需要计入挂起时可使用CLOCK_BOOTTIME；CLOCK_MONOTONIC不计系统挂起，不能笼统等同安全deadline。绝对有效期继续用可信实时时间，重启绑定boot_id并重新证明。

**位置：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)

**验证方向：** 签发短期离线许可/租约后挂起超过其剩余期限再唤醒；需计入挂起的Profile必须拒绝旧许可。再做仅计进程活动时间的独立用途对照，验证两种时钟语义未混用；重启和墙钟回拨仍不能延长权限。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-011｜P1｜歧义

**原文依据：** Space v0.6 UPD-01把失败转入Failed、RolledBack或RecoveryRequired；API-010另说明RecoveryRequired/Blocked表示条件或原因，Task、应用、卷、备份和更新保留领域生命周期并映射公共语义。

**问题：** 更新专用状态中仍把“需要恢复”与实际执行阶段/回退完成并列，缺少明确的机器映射，无法准确表示停在Staged还是TrialBoot、是否已回退以及同时需要人工恢复。此项只处理Update领域，不重复Task的OutcomeUnknown正交问题。

**v1修正：** 更新保持真实lifecycle；校验或容量失败记Failed、安全回退完成记RolledBack，无法安全选部署或数据不兼容记RecoveryRequired条件及原因，持久检查点保留失败所在阶段和实际部署。

**位置：** [docs/03-host-updates-network-devices.md](../docs/03-host-updates-network-devices.md)

**验证方向：** AT-CORE-009增补：分别在Staged与TrialBoot制造无法安全自动恢复的失败；查询应同时准确给出部署/生命周期和RecoveryRequired条件，不能仅剩一个状态名掩盖实际切换事实。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-012｜P0｜缺口

**原文依据：** Space v0.6 REL-02要求构建、测试、批准、签名、分发职责分离和最小权限凭据，REL-05列发布CI风险覆盖；未明确不可信PR源码/测试脚本与生产签名密钥、宿主root、用户数据的执行隔离边界。

**问题：** 流水线逻辑上分阶段并不保证来自PR的任意构建/测试代码无法读取同执行器中的发布Secret或宿主挂载。若沿用有高权限凭据的runner，即使最终签名另有审批，不可信代码仍可能事先窃取凭据或修改待签制品。

**v1修正：** 明确CI/PR代码为不可信执行输入，测试执行器隔离网络、卷和Secrets，不提供生产发布密钥、宿主root或用户数据；签名入口只接受绑定确切commit与artifact的准入证据，流水线/依赖变化重新审查。

**位置：** [docs/13-operations-release-governance.md](../docs/13-operations-release-governance.md)

**验证方向：** 提交含探测环境变量、runner文件系统、容器管理Socket和网络外发代码的测试PR；应无法取得生产密钥或宿主权限，也不能替换已绑定摘要的待签制品。验证正常审查/构建证据通过后独立签名路径仍可运行。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-013｜P1｜缺口

**原文依据：** xelab-package.zip中的28个XeLab模块readme以../../../reports/verification.md链接“验证与来源”，packaged/reports/xenonlab-old-review.md另链接同一缺失报告；外层ZIP目录不含对应reports/verification.md。

**问题：** 本次静态解析发现29处相对报告链接没有归档目标，部分历史验证说明无法仅靠此ZIP完整复核。现存XenonLab-Old报告不能自动替代缺失的上一轮XeLab报告。

**v1修正：** 保留原ZIP字节、记录全部缺链源/目标及证据限制；明确历史测试为作者报告而非本次验证。这里只补充可审查边界，没有声称找回缺失报告或重新执行其测试。

**位置：** [migration/xelab-reuse-boundary.md](../migration/xelab-reuse-boundary.md)

**验证方向：** 按各README/报告所在目录规范化解析相对路径，对照外层ZIP中央目录，应重复得到29处缺失链接；检查静态JSON保留源路径和解析目标，不把另一报告强行代入。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-014｜P1｜歧义

**原文依据：** xelab-package.zip packaged/metadata.json按文件列origins；静态清点得到7组仓库/commit，包含本地Forgejo回环地址、GitHub多个仓库/提交；66个内包无.git历史。旧规范GOV-002明确要求以后以当前Forgejo完整SHA审计。

**问题：** 该ZIP是多来源历史模块集合，缺少Git对象/历史和当前远端验证，不能把任一元数据SHA当作整包唯一HEAD。内包摘要与自带metadata相同，只能证明归档内部清单一致，不能证明当前仓库来源或PsiOS v1实现完成；原规范的当前HEAD要求本身是正确的。

**v1修正：** 区分实际ZIP摘要/目录观察与作者来源陈述，逐组保留7个来源并标HistoricalStatementOnly；整个归档为HistoricalSourceOnly/NotVerified，复用仍需当前仓库逐模块reuse/adapt/rewrite/exclude审查。回环地址不作为可移植项目配置。

**位置：** [migration/xelab-reuse-boundary.md](../migration/xelab-reuse-boundary.md)

**验证方向：** 静态统计28个XeLab与38个XenonLab-Old模块、66内包/1110条目，复算内包SHA并列出来源组；确认报告不声称当前HEAD已核验、不把psios-*crate名称或1638 passed历史数字提升为v1实现状态。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-CORE-015｜P1｜缺口

**原文依据：** Space v0.6 CLU-012已规定恢复旧控制面不能只把epoch加一，须资源端认可新incarnation；API-012要求过期游标重列举，但没有显式说明恢复旧快照后已建立watch、分页游标及控制器投影必须失效。

**问题：** 写入fencing正确也不保证观察缓存正确。恢复数据库可能把revision退到客户端已见版本之前，旧watch/成员/调度缓存若继续等待更高revision，可能长期遗漏删除或维持不再存在的资源投影。此项关注恢复后控制订阅，不重复旧Writer写权限的隔离缺陷。

**v1修正：** 控制订阅、游标和数据修订带恢复世代；旧快照恢复后让旧订阅失效并完整重新列表，处理已删除对象。etcd的revision bump/compaction仅用于相应缓存失效，不能替代PsiOS资源fencing或身份恢复。

**位置：** [docs/12-cluster-coordination.md](../docs/12-cluster-coordination.md)

**验证方向：** AT-CORE-006：先让客户端看到高revision及随后删除，再恢复到较旧控制面快照；旧watch/游标必须明确失效，新快照重建应清除旧投影，并且不能单凭revision bump认为旧Worker已被fence。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-001｜P0｜歧义

**原文依据：** v0.6 第十八章 SEC-003 使用 Approval；第三十三章 WFL-001 使用 ApprovalRequest；35.13 ADR-24 尚待映射

**问题：** 系统审批与 Workflow 请求缺少唯一权威和引用关系，可能产生双审批事实。

**v1修正：** 统一 ApprovalRequest、不可变 ApprovalDecision 和 ApprovalUse；旧 Approval 只作受控视图；Workflow 只保存引用。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-03/04

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-002｜P0｜缺口

**原文依据：** v0.6 SEC-004 支持一次操作授权，WFL-008要求审批版本绑定

**问题：** 未定义同一单次审批被两个并发 Operation 消费时的原子性。

**v1修正：** Approval Authority 原子CAS绑定operation_id；相同操作重试复用绑定，另一操作明确拒绝；不自动释放已绑定决定。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-03/04

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-003｜P0｜缺口

**原文依据：** v0.6 API-008要求执行前重新校验授权，SEC-008要求撤权

**问题：** 检查与真正副作用之间的竞争窗口未规定可保证的顺序，可能把检查完成误当作权限仍有效。

**v1修正：** 授权权威内准入与撤销排序；资源端检查许可、代次和效果前提；外部无条件写不能宣称原子前提保证。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-05/06

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-004｜P0｜缺口

**原文依据：** v0.6 API-008审批绑定不可变摘要，API-009保存规范化请求摘要

**问题：** 摘要的编码与解释规范未定义，重复JSON键、数值精度或跨语言序列化可造成批准内容与执行内容不一致。

**v1修正：** 固定JCS+SHA256、用途前缀和payload范围；拒绝重复键等非法输入；摘要向量跨语言校验，具体实现库待ADR验证。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-15

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-005｜P1｜歧义

**原文依据：** v0.6 SEC-001主体稳定身份、SEC-002 OIDC可选

**问题：** 外部认证账户与Principal的绑定键未规定，按邮箱/显示名合并可能串身份。

**v1修正：** 以认证签发域、外部稳定subject、绑定版本与状态映射主体；删除重建不复用授权。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-01/02

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-006｜P0｜缺口

**原文依据：** v0.6 SEC-003委托权限交集，CHAT-009要求预算上限

**问题：** 子代理范围缩减已有原则，但预算分配如果各自复制完整父额度仍可放大总消耗。

**v1修正：** 将子委托预算记入父预算的已分配额度，限制深度并阻止祖先授权撤销后的新敏感准入。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-02

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-007｜P0｜歧义

**原文依据：** v0.6 R-05 RevocationMode=GateOnly/StreamCut/SandboxTerminate/ResourceFence/ExternalNonRevocable

**问题：** 把可组合的强制机制和不可逆效果混成一个互斥枚举，无法描述多种能力同时存在。

**v1修正：** 改为revocation_capabilities[]能力集合，已发生不可逆效果记录在独立限制/残留效果字段。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-13

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-008｜P0｜缺口

**原文依据：** v0.6 R-05要求返回RevocationResult并可报告Partial/RecoveryRequired

**问题：** 没有规定控制面受理与逐目标强制完成的聚合条件，可能在尚有失联端时虚报完全撤权。

**v1修正：** 定义每目标Pending/Enforced/Unsupported/Unreachable/Failed及范围证据；聚合Complete要求所有必要目标确认，残留效果不被清空。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-05/13

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-009｜P1｜缺口

**原文依据：** v0.6 SEC-002远程认证、GUI-013仅图形流跨站防护

**问题：** 通用Shell管理会话的固定会话、注销、空闲/绝对期限、管理与应用同源隔离缺少闭合契约。

**v1修正：** 补通用Web管理会话保护、登录/提权轮换、服务端注销和长连接撤销；不把图形流规则误当全部管理安全。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-09/11/12

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-010｜P1｜缺口

**原文依据：** v0.6 GUI-013跨站请求及不受信来源控制，CHAT-004正文/附件

**问题：** 消息、模型输出和诊断内容进入管理WebUI时的主动内容执行没有独立展示契约。

**v1修正：** 所有展示内容按上下文转义/消毒；管理站点与应用来源隔离，CSP仅加固，Cookie/Origin/CSRF覆盖写请求和WebSocket。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-09/10

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-011｜P1｜缺口

**原文依据：** v0.6 SEC-002认证映射和SEC-009凭据生命周期

**问题：** 高影响远程身份/信任根变更的新鲜认证与业务审批关系不明，可能以长期被盗会话完成敏感动作或导致每步重复确认。

**v1修正：** 区分新鲜认证证据与审批，默认高影响身份变更要求策略规定的认证，已覆盖步骤复用有效证据；本地独立恢复保留。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-11/14

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-012｜P0｜歧义

**原文依据：** v0.6 FND-03：SecurityState包含撤销状态；CoreState表中同时写任务、授权、资源元数据

**问题：** 授权同时出现于两类状态域，恢复时可能产生两个可写权威。

**v1修正：** SecurityState作为授权/审批使用账本唯一逻辑权威，CoreState只留引用或可重建投影；物理共库不改变职责。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-04/12

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-013｜P0｜歧义

**原文依据：** v0.6 R-01定义VerifiedCurrent/Quarantined/ReEnrolled

**问题：** 全局ReEnrolled容易被解释为已找回撤权历史或所有外部凭据已撤销。

**v1修正：** 模式带可信域、覆盖资源和证据；ReEnrolled仅建立新信任，未联系外部端继续隔离并列残留凭据。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-01/03

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-014｜P0｜缺口

**原文依据：** v0.6 R-01允许最小本地认证和所有者重新登记，FND-05救援入口

**问题：** 最小本地认证若直接使用旧备份管理员列表，仍可能恢复已撤销管理员权限。

**v1修正：** 恢复认证独立于未核实旧授权；使用可信介质/受保护本机入口/事先恢复材料，不以旧远程管理员名单获得最高权限。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-02

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-015｜P0｜缺口

**原文依据：** v0.6 STO-006/007定义备份范围与日志，35.11将Workflow/Conversation/Project列为Core/ServiceState

**问题：** 多数据库及对象池没有统一恢复清单，独立快照可能构成相互矛盾的权威切面。

**v1修正：** 定义RecoverySetManifest、协调切面或非同步检查点加对账两种明确保证；核实Task/Workflow/Approval/outbox/对象引用后才激活。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-04/07/08

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-016｜P0｜缺口

**原文依据：** v0.6 R-01安全新鲜度恢复、AGT-003结果未知、UPD-04从检查点恢复

**问题：** 安全状态新鲜不能证明旧任务检查点之后没有外部副作用，灾难恢复可能重复PR/部署/发送。

**v1修正：** 恢复调度屏障独立于安全模式；使用operation/origin查询外部事实，不能核实则outcome_knowledge=Unknown并停止有害重放。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-05

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-017｜P0｜缺口

**原文依据：** v0.6 API-009幂等记录覆盖重试窗口；STO-009备份保留

**问题：** 正常重试窗口与更长灾难恢复窗口未联合约束，恢复旧快照后旧键可能已被删除。

**v1修正：** 联合设计备份/离线/去重窗口；用墓碑、权威对账、外部效果账本或旧incarnation永久拒绝，换新键不绕过未知效果。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-06

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-018｜P0｜歧义

**原文依据：** v0.6 R-03验收：不能双主确认或丢失已确认消息

**问题：** 确认消息不能丢失的措辞未限定故障模型，单副本落盘无法无条件抵抗整盘损毁和恢复旧快照。

**v1修正：** 声明Accepted持久性故障范围；旧尾部缺失公开HistoryGap，恢复代次/游标重同步并禁止重用旧确认身份空间序号。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-10

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-019｜P0｜缺口

**原文依据：** v0.6 EVT-002结果持久化、CHAT-013附件保留、STO-007恢复集

**问题：** 元数据恢复成功但对象池缺少结果/附件的关系修复和激活语义不完整。

**v1修正：** 清单覆盖发布引用所需对象版本；缺失对象准确报ResultUnavailable，孤儿对象隔离认领，不伪造空成功或跨域暴露。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-07/08

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-020｜P0｜歧义

**原文依据：** v0.6 AGT-004取消不同于业务回滚，UPD-04恢复到新位置；恢复过程无统一状态机

**问题：** 恢复中部分激活后失败/取消，单一恢复失败标签不能描述实际数据写入和未知效果。

**v1修正：** 定义恢复专用状态机和独立data_validation/security_mode/outcome_knowledge/activation_scope；部分激活保留准确清单。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-14

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-021｜P1｜缺口

**原文依据：** v0.6 DAT-004在线查询删除屏障，CHAT-013消息删除/保留，R-01只集中撤权恢复

**问题：** 旧备份恢复可能重新发布已删除数据或共享，尤其缺失当前删除水位时不能声称完整保留历史删除。

**v1修正：** 激活前应用可信删除屏障；删除历史未知时不恢复公开共享/跨群索引，按确认的新私有范围恢复并保留未核实限制。

**位置：** [docs/05-recovery-consistency.md](../docs/05-recovery-consistency.md)

**验证方向：** REC-V1-16

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-SEC-022｜P1｜缺口

**原文依据：** v0.6 SEC-010审计可靠性、API-009幂等墓碑和CHAT-013保留分别规定

**问题：** 审计、审批使用、效果记录与去重墓碑的回收缺少共同不变量，清理可能重新开放旧请求。

**v1修正：** 安全记录与缓存分域预算；未决效果/使用账本不可被缓存GC清除，正文回收后保留必要拒绝旧请求的最小状态。

**位置：** [docs/04-security-identity.md](../docs/04-security-identity.md)

**验证方向：** SEC-V1-14、REC-V1-06

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-001｜P1｜歧义

**原文依据：** Space v0.6 STO-004：Volume状态同时包含Attached、ReadOnly、Degraded、Deleting。

**问题：** 单一枚举把生命周期、附件存在性、健康和有效访问模式混为互斥状态，无法清楚表示已挂载但只读/退化的卷。

**v1修正：** Volume生命周期、VolumeAttachment生命周期、health和effective_access分离；保留旧Attached为附件派生显示。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-001/002：已挂载卷变只读、多个只读附件、失卷及恢复。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-002｜P0｜歧义

**原文依据：** Space v0.6 TFS-015：ReadWriteOnce等访问模式；同一卷单写者为首轮目标。

**问题：** 没有明确RWO的节点挂载限制和单个资源写入权威的区别，可能让同节点多进程或旧Writer绕过单写假设。

**v1修正：** 定义single_node_rw与single_writer_rw不同语义；单Writer必须在所有真实写通路fence。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-004/012：同节点两Writer及分区旧Writer复活。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-003｜P0｜缺口

**原文依据：** Space v0.6 STO-001、TFS-002：资源和inode身份稳定；STO-008：恢复到新目标。

**问题：** 缺少并行克隆与原目标恢复的身份分配规则，旧句柄或ACL可能误绑定恢复后的同名资源。

**v1修正：** 新增data_incarnation；原目标恢复更换数据世代并废旧附件，并行克隆使用新volume_id；inode联合身份加入卷数据世代。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-001/011：原卷/克隆并存及旧fd对新incarnation访问。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-004｜P0｜歧义

**原文依据：** Space v0.6 TFS-004/005：内容持久化后提交；R-03验收不得丢失已确认消息。

**问题：** 持久化成功缺少进程崩溃、断电、单盘和整机损毁的适用故障边界，易被误解为唯一介质毁坏也能零损。

**v1修正：** 区分Visible/Durable，Profile列ack_durability、故障域和RPO，超过能力的请求拒绝或显式降级。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-002/003：断电和介质损毁分别验证，对照声明范围。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-005｜P1｜缺口

**原文依据：** Space v0.6 STO-007：备份清单和内容校验值；STO-008：IntegrityChecked。

**问题：** 内容校验与来源认证/安全新鲜度未在BackupManifest中分开，篡改者可同时替换内容和未认证校验表。

**v1修正：** 清单由授权来源签名或等价受保护记录；分别记录完整性、真实性、新鲜度和恢复业务验证。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-003：内容和清单一起被替换、有效签名旧备份、错密钥。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-006｜P1｜歧义

**原文依据：** Space v0.6 DAT-001及R-14：字段具体类型、NULL、缺省在ADR-13冻结。

**问题：** 主字段仍未落到可实施类型，score=0与未评分、vector缺失等可被不同后端解释。

**v1修正：** v1明确uid/string、tag/去重数组、score/有限number或null、可选vector及Profile，extra_data保留任意JSON根值。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-005：未评分、零分、空分类和非object extra_data往返。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-007｜P1｜缺口

**原文依据：** Space v0.6 DAT-002：JSON精确查询；DAT-001：JSON路径语义待冻结。

**问题：** missing/null、重复键、字符串数字和超2^53整数等语义不明，无法保证跨引擎精确查询。

**v1修正：** 定义受限AST、无隐式转换、重复键拒绝、精确十进制比较或公开入口数值边界、表达式预算。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-005：大整数、1与1.0、1与字符串1、missing与null。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-008｜P1｜缺口

**原文依据：** Space v0.6 DAT-003：索引可追溯版本；DAT-001：评分器仅更新授权范围。

**问题：** 仍缺score/vector在原文修改并发下的明确失效和CAS写入规则，旧计算结果可能贴到新内容。

**v1修正：** content变更新版本；旧派生数据失效或保留旧版本；评分/Embedding更新绑定输入版本和Profile并条件写。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-006：计算中更新正文、换模型，旧结果不得覆盖新版本。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-009｜P1｜歧义

**原文依据：** Space v0.6 DAT-003：View固定或动态最新版本；DAT-004：当前授权代次。

**问题：** 固定版本内容和固定历史授权容易被混用，固定View需要的保留关系也未完全绑定。

**v1修正：** 固定View保留内容快照，但访问每次检查当前权限和删除屏障；审计保留用独立策略。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-007：固定View与撤权/GC并发，保留内容而拒绝无权访问。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-010｜P0｜缺口

**原文依据：** Space v0.6 TFS-007：文件与表查询缓存失效；TFS-005：共享提交序列。

**问题：** 仅说明失效仍不足以处理FUSE writeback缓存与绕开内核的Catalog写入；dirty page可能覆盖表侧新版本。

**v1修正：** 首轮禁止未经验证的writeback双入口并发，明确统一写入口或有证据的排空/失效协议；mmap能力单独验证。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-008：dirty page未回写时Catalog改内容/截断，重启后无旧写覆盖。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-011｜P1｜缺口

**原文依据：** Space v0.6 TFS-002/008/010：目录名称、查询分页、同名冲突。

**问题：** 没有定义非UTF-8名称的可逆表示和分页水位，可能导致文件名损坏、目录移动时遗漏/重复或循环。

**v1修正：** 名称保留字节，不隐式归一化；Catalog有可逆编码；游标绑定查询/快照/授权，过期重同步。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-010/011：非UTF-8、组合Unicode、移动/重名和游标过期导出往返。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-012｜P0｜缺口

**原文依据：** Space v0.6 TFS-013：GC保护活跃inode、句柄、快照、备份、导出。

**问题：** 静态引用清单不足以防GC扫描后并发建立引用或在途发布；仅TTL释放pin也不能证明Writer停写。

**v1修正：** 把在途发布/构建/恢复pin纳入安全根，删除前原子验证新引用，恢复pin与世代并防扫描/发布竞争。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-009：GC扫描无引用后创建快照/发布内容，不能误删。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-013｜P0｜歧义

**原文依据：** Space v0.6 TFS-012：FUSE、UID/GID、ACL共用授权；R-05：fd/挂载撤权。

**问题：** 原则未明确FUSE挂载检查者、allow_other与default_permissions关系以及已打开fd/mmap残留，容易把新请求ACL等同既有数据面撤权。

**v1修正：** 挂载Profile明确内核/daemon授权；撤权按真实能力集合和逐资源结果，旧句柄保留内容但不能伪称全撤销。

**位置：** [docs/06-storage-data-tablefs.md](../docs/06-storage-data-tablefs.md)

**验证方向：** AT-STA-007/008：allow_other多用户访问、chmod后既有fd、mmap撤权。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-014｜P1｜歧义

**原文依据：** Space v0.6 APP-005：Installed、Running/Degraded、Updating、Failed混合生命周期。

**问题：** 安装操作、部署选择、进程状态、健康混为单一状态，崩溃和回退后无法准确表示已安装但未运行或运行未就绪。

**v1修正：** 拆Operation、Deployment、desired_state、Runtime observed、health、readiness并给旧状态迁移规则。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-014/015：进程活但不就绪、安装完成后启动失败、升级回退。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-015｜P0｜缺口

**原文依据：** Space v0.6 APP-007：升级/恢复/卸载互斥，使用可恢复操作锁。

**问题：** 锁失效后的接管尚未明确旧迁移可能仍写入，仅重取锁可能让两个迁移者同时修改同一数据域。

**v1修正：** 操作锁含范围/epoch/恢复规则；接管前真实fencing或证实停写，不可证时RecoveryRequired。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-014：旧迁移停顿超时后恢复与新迁移竞争。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-016｜P0｜缺口

**原文依据：** Space v0.6 APP-002/003：申请、批准、实际权限分离；缺必需限制拒绝。

**问题：** 缺少启动阶段对实际权限及只读root挂载需求的验据边界，Runtime接受配置不总等于所有限制已生效。

**v1修正：** 实际权限必须不超授权且必需限制有证据；run/tmp等明确受限可写挂载，不能失败后开放整个宿主。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-013：只读根无法创建run、缺seccomp/资源控制、UID映射不符。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-017｜P0｜缺口

**原文依据：** Space v0.6 PKG-005：安装脚本为不可信代码；PKG-002：读取元数据。

**问题：** 隔离要求只明确到脚本，未充分覆盖解析/解压自身的路径穿越、恶意链接和解压资源耗尽。

**v1修正：** 格式探测起即受限进程，限制字节/文件/深度/时间，拒绝逃逸/设备/危险属性，失败清理不追恶意链接。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-017：tar bomb、../、absolute path、symlink/hardlink逃逸、setuid条目。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-018｜P1｜歧义

**原文依据：** Space v0.6 PKG-004：签名/哈希证明来源或完整性。

**问题：** 未明确不同包生态信任入口区别，容易把自行计算哈希或并不存在的统一单包签名当认证。

**v1修正：** Adapter逐生态验证仓库元数据/签名链/摘要；用户无认证来源包另设策略并展示信任等级。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-016/018：替换仓库索引、缺签名用户包、仅本地SHA不得标可信发布。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-019｜P0｜缺口

**原文依据：** Space v0.6 PKG-003/005：隔离用户空间及脚本最小权限。

**问题：** 构建环境与生产凭据/网络出口未作具体隔离，受限脚本仍可能借宽外连窃取挂载秘密。

**v1修正：** 构建默认无生产秘密和宿主会话；网络只允许受控依赖源，解析后来源锁定，离线缺依赖不自动联网。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-017/018：安装脚本访问生产Secret、重定向源和离线联网。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-020｜P0｜缺口

**原文依据：** Space v0.6 GUI-014：NativeWeb反向代理、iframe、SSO和域名路径边界。

**问题：** 只按路径代理不足以隔离不可信应用脚本、Cookie、Service Worker和管理会话；任意URL代理还可能产生SSRF。

**v1修正：** 管理与不可信应用分origin，host-only会话、限制postMessage与代理目的，验证重定向/DNS/IPv6，不移除安全头强嵌入。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-024：应用脚本窃管理Cookie、SW跨路径、伪postMessage、代理到元数据端点。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-021｜P0｜歧义

**原文依据：** Space v0.6 GUI-005、CU-003：不能裁剪冒充隔离或整屏输入冒充单窗口。

**问题：** 原则正确，但尚缺捕获范围与输入限制独立能力声明；窗口捕获成功可能被泛化为完整单应用安全。

**v1修正：** 分别发现观察和输入范围；无法限制键盘/焦点时拒SingleApplication或改应用专属会话。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-019：指定窗口画面但快捷键能操作宿主或其他窗口。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-022｜P0｜缺口

**原文依据：** Space v0.6 CU-004、GUI-007：撤销输入并清空队列。

**问题：** 清空待发送输入不能处理已按下按键/按钮、IME组合、拖拽或ACK丢失后的重复文本提交。

**v1修正：** 跟踪按下集合、输入序号与租约代次，安全清理原会话状态，不能确定目标时终止会话且禁重放。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-020/026：Ctrl按下断线、接管时拖拽、IME未提交、粘贴ACK丢失。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-023｜P1｜歧义

**原文依据：** Space v0.6 R-15/36.1：优先pipewire-serial，不能无限复用旧Node ID。

**问题：** 没有写清pipewire-serial是ScreenCast接口v6字段，可能把旧后端误判为协议违规或把serial当永久授权。

**v1修正：** 能力矩阵绑定接口版本；老后端Node ID限定活动连接/代次；serial也绑定会话和PipeWire代次。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-021/022：v4/v5/v6、Node ID复用、PipeWire重启与热插拔。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-024｜P1｜缺口

**原文依据：** Space v0.6 CU-006/007及R-15：使用ScreenCast/RemoteDesktop并管理持久令牌。

**问题：** 未区分独立ScreenCast与组合RemoteDesktop会话的Start/Stop和持久化权威，可能错误消费两套token。

**v1修正：** 组合会话只由RemoteDesktop管理生命周期及persist/restore，ScreenCast用于选择捕获源。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-022：组合会话恢复、单独Portal授权、拒绝双token配置。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-025｜P1｜缺口

**原文依据：** Space v0.6 CU-006：登记可用输入方式。

**问题：** 未具体规定EIS连接与Notify输入互斥，适配器可能重复注入或违背后端协议。

**v1修正：** 按能力固定一个输入通路，建立EIS后不得混用Notify。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-022：EIS建连后Notify请求明确拒绝，不重复输入。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-026｜P0｜缺口

**原文依据：** Space v0.6 R-15：恢复token单次使用及会话代次绑定。

**问题：** 只要求轮换没有定义远端已消费但新令牌响应/持久化丢失的崩溃窗口，重试可能重复或错误恢复。

**v1修正：** 本地Available/Reserved/Consumed与UnknownConsumed状态；未知消费禁止盲重用，等待重新授权/可靠核实。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-023：Portal消费后杀进程或丢Start响应，旧token不得自动恢复可用。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-STA-027｜P1｜歧义

**原文依据：** Space v0.6 GUI-006：音频播放/麦克风分开授权；APP-004：Flatpak静态权限映射。

**问题：** 用户界面可分项但部分真实后端权限同时开放多类音频访问，可能虚构后端并未提供的分权。

**v1修正：** 按actual capability显示最窄可强制范围，能力不足拒绝或授权真实更宽范围，不据UI复选框声称隔离。

**位置：** [docs/07-applications-packages-gui.md](../docs/07-applications-packages-gui.md)

**验证方向：** AT-STA-025：Flatpak音频Socket带麦克风访问而用户只同意播放。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-001｜P1｜歧义

**原文依据：** space-unified-v0.6.md §35.9 接口表：Workflow→Task Engine 的 Submit/Cancel/GetResult 共享字段列含 task_id

**问题：** 将不同操作的字段放入同一栏，未区分创建请求尚无 task_id 与服务端返回已分配 ID，容易产生客户端伪造身份或无从构造合法请求的实现。

**v1修正：** 拆分 Submit 与 Accepted 回执；创建请求不含 task_id/run_id，Broker 持久接受后分配并返回；既有身份导入使用单独迁移接口。

**位置：** [docs/08-tasks-events.md §8.2](../docs/08-tasks-events.md)

**验证方向：** 分别验证合法 Submit、携带 task_id 的非法创建请求及 Accepted 回执；重复请求必须返回原 task_id。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-002｜P0｜缺口

**原文依据：** space-risk-v0.6.md R-02：恢复时先按 origin_ref 查询，超过幂等窗口仍通过墓碑或对账拒绝旧请求

**问题：** 已有对账原则，但未区分权威确认不存在、索引已过期、滞后副本查不到与灾备不完整；普通 NotFound 可能被误用为再次提交的证据。

**v1修正：** 定义 Found、AbsentWithinWindow、Expired、Indeterminate 四种查询结果；只有有效窗口内当前权威的一致读可证明未接受；过期或不确定阻断自动重提。

**位置：** [docs/08-tasks-events.md §8.2–8.3](../docs/08-tasks-events.md)

**验证方向：** 删除正文、过期去重索引、读取滞后副本及恢复旧快照后查询同键，均不得变成可安全新建工作的普通 NotFound。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-003｜P0｜歧义

**原文依据：** space-risk-v0.6.md R-02：(origin_ref,idempotency_key) 持久去重，参数摘要不同则 Conflict

**问题：** origin_ref 的签发权威、租户/主体范围和摘要覆盖范围尚未形式化，可能跨安全域碰撞，或在复用旧键时偷偷改变目标、输入和预算。

**v1修正：** origin_ref 绑定 authority_id、origin_type、origin_id 及步骤/尝试；去重同时绑定安全域和认证主体；TaskSubmit 全对象除 request_digest 外按共同 JCS 域分离规则计算摘要，服务端复算。

**位置：** [docs/08-tasks-events.md §8.2](../docs/08-tasks-events.md)

**验证方向：** 两个域提交同字符串键不会串任务；同域同键修改 input_ref、目标类型、预算引用或 origin_ref 不得误返回旧成功。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-004｜P0｜缺口

**原文依据：** space-risk-v0.6.md R-02；space-unified-v0.6.md AGT-004、WFL-009 的取消与恢复规则

**问题：** 持久 DispatchIntent 解决接受回执裂缝，但未完整定义取消与尚未确认提交并发时的串行化；删除本地 outbox 无法阻止已在网络中的 Submit。

**v1修正：** 引入 Broker 按来源键的 CancelOrigin 取消/拒绝墓碑，由同一权威与提交判定串行化；未确认时保持 CancelPending 并对账在途 Task。

**位置：** [docs/08-tasks-events.md §8.3；docs/11-workflows-projects.md §11.6](../docs/08-tasks-events.md)

**验证方向：** 交错测试 Submit 先提交、CancelOrigin 先提交及两者回执均丢失；结果必须是原 Task 收到取消或旧提交被拒绝，不能漏出未跟踪任务。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-005｜P1｜歧义

**原文依据：** space-unified-v0.6.md AGT-001；space-risk-v0.6.md R-02 的 step_key、attempt_generation；R-11 动态任务

**问题：** 已有 Task/Run 区分，但 Workflow 业务重试、Task 内部 Run 重试、网络重发与循环迭代的身份变更边界没有完整定义，可能导致重复派发或错误复用结果。

**v1修正：** step_key 固定步骤实例及迭代/Map/子图路径；attempt_generation 只在确认安全的新业务尝试增加；ACK 丢失和引擎重启保留原值；Run 与 Operation 各有独立身份。

**位置：** [docs/08-tasks-events.md §8.4；docs/11-workflows-projects.md §11.3](../docs/08-tasks-events.md)

**验证方向：** 分别注入网络重传、Broker Run 重试、Workflow 步骤重试和第二次 Loop 迭代，验证只有对应层的身份或代次发生变化。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-006｜P0｜歧义

**原文依据：** space-unified-v0.6.md §35.9：Task 状态列表后另有 OutcomeUnknown，Workflow 含 Completed

**问题：** OutcomeUnknown 与生命周期之间的正交关系没有机器级定义；Cancelled 或 Failed 后仍可能存在未知外部效果，Completed 又可能被误解为成功。

**v1修正：** 使用 lifecycle 与 outcome_knowledge 两个字段；Failed/Cancelled + Unknown 合法且阻断依赖；Succeeded 必须 Known；Workflow Completed 仅在确认成功时迁移为 Succeeded。

**位置：** [docs/08-tasks-events.md §8.4；docs/11-workflows-projects.md §11.4](../docs/08-tasks-events.md)

**验证方向：** 写请求发出后断网再取消，查询必须同时返回已停止的生命周期与 Unknown；Succeeded+Unknown 必须被拒绝；对账仅追加证据而非篡改执行历史。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-007｜P0｜缺口

**原文依据：** space-unified-v0.6.md AGT-004：跨节点重试遵循租约与 fencing，避免两个执行者写同一资源

**问题：** 旧 Run 结果应被 fencing 拒绝，但原文未区分拒绝其修改当前状态与保存其外部成功证据；直接丢弃迟到回执会隐匿真实副作用。

**v1修正：** 结果提交校验 Task 修订、Run、领取/恢复代次；过期执行者不得覆盖当前 Task，却仍可提交可验证的独立对账证据；资源释放也必须归属具体 Run。

**位置：** [docs/08-tasks-events.md §8.5](../docs/08-tasks-events.md)

**验证方向：** 新 Run 领取后旧 Run 返回远端写成功回执：新状态不被覆盖，外部效果被记录且进入恢复判断，不能误释放新 Run 资源。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-008｜P0｜缺口

**原文依据：** space-unified-v0.6.md AGT-001 的输入保留关系、EVT-002 的接受持久化、CHAT-013 的附件保留

**问题：** 输入/附件保留与业务记录可能属于不同服务；仅说建立保留关系不足以防止接受后内容被回收，或失败上传永久占用空间。

**v1修正：** 本地原子提交范围明确；跨服务 pin 使用可核验保留凭据、幂等确认和对账；未提交孤立 pin 有界回收，活动任务/消息引用必须续持或保护；未完成内容不标为可执行/已送达。

**位置：** [docs/08-tasks-events.md §8.2、8.6；docs/10-conversations.md §10.8](../docs/08-tasks-events.md)

**验证方向：** 在 pin 创建、Task/Message 提交和 pin 确认各边界崩溃；已接受引用不悬空，未接受孤立内容有界回收，重复确认不多次计费。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-009｜P1｜缺口

**原文依据：** space-unified-v0.6.md EVT-004 与 §35.9 写入边界：至少一次事件、消费者去重、持久游标

**问题：** 已禁止依靠游标承诺 exactly-once，但未逐项约束 inbox 标记、业务状态与游标提交的先后；先标已处理再执行业务会丢工作，反向只记游标会重做。

**v1修正：** 消费者的 inbox 意图、业务状态与消费进度必须同事务或等效恢复；外部效果另走持久 Operation/幂等/结果核实；重复和毒消息保留可审计恢复记录。

**位置：** [docs/08-tasks-events.md §8.7](../docs/08-tasks-events.md)

**验证方向：** 在 inbox 登记、业务更新、外部调用和消费提交边界逐次崩溃，重投后无丢工作、无重复不可幂等效果。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-010｜P1｜缺口

**原文依据：** space-unified-v0.6.md AGT-010、CHAT-009、WFL-005 的预算与递归限制

**问题：** 已有预算要求，但没有声明并行预留、父子共享账本、重试不重置根预算和在途账单延迟；多个分支可同时读到余额后超发。

**v1修正：** 统一预算树的准入预留、运行计量和结算，子 Agent、重试、压缩及桥接回声共享根预算；只释放未消费预留，限制在途最大成本并区分估计与账单。

**位置：** [docs/08-tasks-events.md §8.8；docs/09-agents-models-protocols.md §9.4；docs/10-conversations.md §10.7](../docs/08-tasks-events.md)

**验证方向：** 多个并行分支争用最后额度、A→B→A 递归和计费延迟下核对账本；不得新建根绕过限制，基础救援资源不受侵占。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-011｜P1｜错误

**原文依据：** space-risk-v0.6.md R-06：将 server/discover 写作可选

**问题：** 官方 MCP 2026-07-28 规范要求服务器实现 server/discover；可选的是客户端是否预先调用发现。原表达没有区分两者，会允许不合规服务器自称支持该基线。

**v1修正：** 保留经官方核验确实存在的 2026-07-28 基线，明确服务器必需实现、客户端可直接请求；现代逐请求上下文与 legacy 初始化握手分开注册和测试。

**位置：** [docs/09-agents-models-protocols.md §9.7](../docs/09-agents-models-protocols.md)

**验证方向：** 现代服务器缺少 server/discover 时兼容验证失败；客户端未预调用发现但逐请求元信息正确时可工作；不把旧 Session 当现代授权。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-012｜P1｜缺口

**原文依据：** space-risk-v0.6.md R-06：Tasks 扩展与 PsiOS Task 显式映射；space-unified-v0.6.md AGT-008

**问题：** 未定义现代 MCP task completed 的业务含义；官方允许 completed 结果携带 isError:true，直接映射本地成功可能错误释放合并/部署等后继。

**v1修正：** 映射同时校验协议状态与工具业务结果；completed+isError 按业务失败或恢复条件处理；远端 TTL 到期只表示结果可能已清理，不证明未执行。

**位置：** [docs/09-agents-models-protocols.md §9.8](../docs/09-agents-models-protocols.md)

**验证方向：** 构造 completed+isError:true、任务 TTL 过期及取消后迟到结果；不能误标 Succeeded 或重放未知写操作。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-013｜P1｜缺口

**原文依据：** space-unified-v0.6.md AGT-008、CHAT-014：A2A 适合 Agent 间请求/任务语义

**问题：** 协议定位正确，但未说明 A2A Send Message 的发送幂等为服务器可选能力；仅携带 messageId 仍不足以安全重发有副作用请求。

**v1修正：** 适配器逐实现登记发送幂等范围、期限和结果查询能力；ACK 丢失优先对账，未经证明不自动重发；外部 Task ID 按端点/租户/适配器域隔离。

**位置：** [docs/09-agents-models-protocols.md §9.8](../docs/09-agents-models-protocols.md)

**验证方向：** 接入不实现 Send Message 幂等的测试服务，故意丢 ACK；不得仅因 messageId 相同就重复发送业务动作。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-014｜P2｜歧义

**原文依据：** space-risk-v0.6.md R-07：Run 绑定模型提供者/模型版本与状态快照

**问题：** 模型版本可固定的强度依赖提供者；不公开权重修订的云服务无法由 PsiOS 生成可验证不可变版本，若统一宣称精确重放会超出可知事实。

**v1修正：** 记录提供者可见标识、时间与版本信息，使用 model_revision_assurance=ProviderReported 标注证据强度；不承诺固定随机种子可使云服务确定重放。

**位置：** [docs/09-agents-models-protocols.md §9.2](../docs/09-agents-models-protocols.md)

**验证方向：** 使用仅返回可变模型别名的提供者恢复 Run，必须显示版本保证不足并按兼容政策处理，不能伪造权重摘要。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-015｜P1｜歧义

**原文依据：** space-unified-v0.6.md CHAT-004、§34.5 示意消息；space-risk-v0.6.md 规范一致性修正中的 idempotency_key

**问题：** 原示意消息混合服务端身份、正文、任务关联，未给出独立发送请求；客户端幂等身份与服务端 message_id 的关系、摘要覆盖范围不清楚。

**v1修正：** 拆分 SendMessage 与 Accepted；引入稳定 client_send_id；message_id、seq、writer_epoch、身份和确认时间均由服务端权威产生；content_digest 覆盖完整发送请求除自身。

**位置：** [docs/10-conversations.md §10.3–10.4](../docs/10-conversations.md)

**验证方向：** 同键同内容重发返回同一 message_id/seq；同键改 mentions、attachment_refs 或 reply_to 必须冲突；客户端伪造 sender/seq 不改变权威字段。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-016｜P0｜缺口

**原文依据：** space-unified-v0.6.md CHAT-002 的每次访问授权；space-risk-v0.6.md R-03 的 ACL 决策与消息提交

**问题：** 每次授权原则尚未定义移除成员与发送并发的线性化边界；先检查成员、排队后再提交可能在移除生效后写入。

**v1修正：** 成员变更与消息提交进入同一权威日志或等效线性化协议；在提交点校验 Active 与授权代次，并记录 membership_revision；投递、读取、搜索和附件再次授权。

**位置：** [docs/10-conversations.md §10.2](../docs/10-conversations.md)

**验证方向：** 并发执行发送、成员移除与 writer 切换；每条结果符合权威序列，移除之后不能通过旧队列/缓存继续发言或读取。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-017｜P1｜缺口

**原文依据：** space-unified-v0.6.md CHAT-005、CHAT-013；space-risk-v0.6.md R-03 的幂等键

**问题：** 正文删除与去重元数据 GC 后，旧发送键可能重新成为未知键；仅保留一个有限去重窗口无法长期阻止历史重放，同时无限保存正文又违反保留目标。

**v1修正：** 服务器签发/登记有界发送命名空间，窗口内保留映射，过期后拒绝命名空间所有旧键；正文和防重放边界独立，不能信任客户端自报发送时间。

**位置：** [docs/10-conversations.md §10.4](../docs/10-conversations.md)

**验证方向：** 删除正文、清理去重记录、重放过期 client_send_id 和伪造客户端时间；均不得产生新消息，旧备份不能恢复旧窗口写权。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-018｜P1｜缺口

**原文依据：** space-unified-v0.6.md CHAT-006 的未处理确认、CHAT-002 的成员身份

**问题：** 退出后重新加入仍是同一 Principal，但不是同一次授权关系；投递/处理去重若只绑定主体与消息，可能继承旧读取权限或吞掉新合法处理。

**v1修正：** Membership 每次加入创建新实例；Delivery 去重绑定消息、主体、成员实例、目标订阅/处理器及版本；重新加入不复活旧未处理请求或旧读游标。

**位置：** [docs/10-conversations.md §10.2、10.6](../docs/10-conversations.md)

**验证方向：** 成员退出、移除、重新加入并改变历史范围后重放旧投递；旧 inbox 失效，新订阅不会被旧 receipt 误判已处理。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-019｜P1｜歧义

**原文依据：** space-unified-v0.6.md CHAT-005、CHAT-006：至少一次通知、已处理确认、不得伪装已送达

**问题：** Accepted、通知投递、已读、处理完成与关联 Task 成功之间缺少明确事实表，单一 ACK 字段容易被多个消费者解释为全部完成。

**v1修正：** 定义 Accepted、Delivery Queued、Delivered、ReadCursor、Processed 五种事实；处理结果绑定处理器版本并返回正式 Task/Operation 引用；高序号通知不证明之前全部已读。

**位置：** [docs/10-conversations.md §10.6](../docs/10-conversations.md)

**验证方向：** 只收到通知但未获取正文、只已读但未处理、处理创建 Task 尚未完成三种情况下分别核对 UI/API，不能越级显示成功。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-020｜P1｜歧义

**原文依据：** space-risk-v0.6.md 规范一致性修正使用 sender_epoch，R-03 使用 writer_epoch/next_seq 与 Accepted(seq)

**问题：** 发送者撤销代次、成员修订和写者 fencing 代次未给出明确分离；且单次持久 ACK 的故障范围未区分进程崩溃与全盘丢失，恢复旧序号可能形成重复确认。

**v1修正：** 分离 writer_epoch、membership_revision 和 SecurityState；序号只按整数语义单会话比较且用十进制字符串编码；durability_profile 明示故障范围，旧备份需恢复屏障、RPO 和新鲜度核验。

**位置：** [docs/10-conversations.md §10.3、10.5](../docs/10-conversations.md)

**验证方向：** 令序号超过 2^53，注入旧 writer、成员移除及全盘旧快照恢复；无精度碰撞、无代次互换、无把缺失历史宣称已完整恢复。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-021｜P2｜歧义

**原文依据：** space-unified-v0.6.md CHAT-003：将私聊扩为群聊需要额外策略检查；会话表中 DirectMessage 为一对一

**问题：** 若原地把 DM 扩员，其既有一对一语义、历史可见边界和第三人权限可能同时改变；原文仅要求额外检查，没有明确对象身份迁移规则。

**v1修正：** DirectMessage 固定两个参与主体；第三人加入创建新 Room，历史复制或引用逐项授权；原 DM 不自动扩员或合并，同名主体不能用于身份去重。

**位置：** [docs/10-conversations.md §10.1–10.2](../docs/10-conversations.md)

**验证方向：** 向 DM 邀请第三主体只产生新 Room，默认不暴露原私聊；重新邀请同显示名不同 Principal 不串会话。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-022｜P1｜缺口

**原文依据：** space-unified-v0.6.md WFL-005；space-risk-v0.6.md R-11：DAG、有界循环和动态子任务

**问题：** 有界原则正确，但尚无结构化 loop 展开、稳定迭代身份、Skipped 与 Join 规则；AnySuccess 也不能消除仍在写入的败者效果。

**v1修正：** 基图无环，循环用 BoundedLoop 有限展开；Map 固定输入清单；定义 AllSuccess/AllSettled/AnySuccess，Skipped 只适用于未选分支；败者 Unknown 阻断单一效果后继，补偿保留残留事实。

**位置：** [docs/11-workflows-projects.md §11.2–11.4、11.6](../docs/11-workflows-projects.md)

**验证方向：** 循环中途重启、未选分支 Join、重复终态、AnySuccess 败者仍写入和补偿失败时均得到确定且不隐匿效果的结果。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-023｜P0｜缺口

**原文依据：** space-unified-v0.6.md WFL-008、§35.9 的 Approval/Operation；space-risk-v0.6.md ADR-24 主题

**问题：** 审批请求、不可变判定、Workflow 进度和一次性使用账本没有完整区分；两个并行操作可能消费同一单次批准，或旧门禁状态绕过执行时撤权。

**v1修正：** ApprovalRequest 等待、ApprovalDecision 判定、旧 Approval 兼容视图使用单一权威；单次决策 CAS 绑定 operation_id；同 Operation 重试不重复消费，绑定不返池；ExecutionPermit 在执行前复验策略和效果前提。

**位置：** [docs/11-workflows-projects.md §11.5](../docs/11-workflows-projects.md)

**验证方向：** 两个 Operation 并发抢同一单次决策只能一个成功；同 Operation 重试不耗第二次；批准后撤权或改 head 时不能用旧门禁记录执行。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-024｜P1｜歧义

**原文依据：** space-unified-v0.6.md §35.9：不覆盖更高版本事实；R-10：ExternalLink revision 与冲突对账

**问题：** 更高版本未区分提供者修订类型；ETag、Git SHA 和随机版本并不天然可排序，按字典序或到达时间选最新会覆盖正确事实。

**v1修正：** ExternalLink 声明 MonotonicSequence/OpaqueETag/CommitGraph/None；opaque 仅相等和条件写，Git 关系由权威图校验；Webhook 为通知，回源读取和周期对账建立事实。

**位置：** [docs/11-workflows-projects.md §11.9](../docs/11-workflows-projects.md)

**验证方向：** 以随机 ETag、非时间序 SHA 和乱序 Webhook 驱动同步；不得使用字符串大小判断新旧，无法判断时回源或阻塞冲突。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。

### FIX-ORC-025｜P0｜缺口

**原文依据：** space-unified-v0.6.md WFL-008、WFL-011、WF-AT-06：审批和提交版本绑定；R-10 外部事实

**问题：** 绑定版本原则尚未完整覆盖 CI/Review 检查身份、尝试号与最终合并的 TOCTOU；先读当前 head 再无条件合并仍可能越过已审目标。

**v1修正：** CI、Review、Artifact 与 Approval 绑定具体仓库/head/检查或运行身份；合并/写回使用远端实际支持的条件约束；缺少条件写不能声称原子，按风险转只读补丁或专用接管。

**位置：** [docs/11-workflows-projects.md §11.5、11.9–11.10](../docs/11-workflows-projects.md)

**验证方向：** 在 CI/Review 成功之后、合并调用之前改变 head；旧证据不能授权新提交；远端无条件写能力时不得错误宣称无竞争保障。

**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。
