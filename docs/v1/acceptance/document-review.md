# 本次文档交付复核记录

日期：2026-11-22；对象：PsiOS文档v1.0.0。此记录描述实际完成的文档审查，不替代未来产品验收。

## 审阅范围与修复结果

领域重写后进行了独立交叉审阅，检查00～16章、Schema生成源与生成文件、正反示例、ADR编号、证据模板和旧要求承接。在89项旧文审查记录之外，统稿阶段发现并消除了以下9项新稿一致性问题；这些不重复计入“旧文89项”。

| 编号 | 统稿时发现的差异 | 交付前处理 |
|---|---|---|
| INT-001 | Task结果枚举正文与Schema不一致 | 统一Pending、NoneExpected、Available、Evicted、Unavailable及历史迁移含义 |
| INT-002 | 等待/阻塞原因在正文必需但Schema不可携带 | Task、Run、WorkflowRun加入conditions，Unknown需要具体原因 |
| INT-003 | Succeeded仍可带Pending结果 | 排除该组合；保留历史成功后的结果回收/不可取情况，增加反例 |
| INT-004 | 包兼容正文为证据集合，Schema为单一等级 | 改compatibility_claims逐项记录Recognized/Imported/RuntimeVerified/GUIVerified |
| INT-005 | 验收模板字段与正文信息项不对应 | 补requirement_ids、统一执行result，增加明确嵌套路径映射 |
| INT-006 | 摘要排除“服务端字段”会与完整对象规则冲突 | 只排除该合同摘要自身；对象内ID/时间仍参与，传输字段本来不在对象中 |
| INT-007 | effects与effect_refs名称不一致 | 统一effect_refs，指向独立EffectRecord |
| INT-008 | “循环无上限”反例实际只测试步骤期限 | 原例改名，另构造真实BoundedLoop缺少max_iterations的反例 |
| INT-009 | ApplicationManifest不足以承载完整运行要求 | 新增必填release_spec_ref及严格ApplicationReleaseSpec，启动前验证完整引用 |

上述处置已落实在当前分章、生成器、Schema和样例。计划中的验证结果仍不会因此被标为产品Passed。

## 原始要求的人工语义抽查

独立抽查19项：AGT-001/007/011、CHAT-003/008/015、CLU-001、DAT-001/002/004、EVT-006、GUI-001、PKG-001/005、TFS-001/009、WFL-005/010/014。

抽查确认保留原生与第三方接入、local_only、主动DM/邀请及受控群摘要、本机自治、通用JSON与大逻辑记录、fence后释放租约、三类GUI入口、DEB/RPM/Alpine隔离导入、可选TableFS和Dataset映射、有界循环、外部权威与最小Workflow。DM加入第三人转成新Room并检查历史共享，是对象语义澄清。

迁移复核还把基础网络状态、时区/DNS/证书/随机数/IPC、权限页使用者与数据去向、观测预算及失败隔离的原有要求明确补回对应新章节。全量245个旧编号有去向；这一统计与19项人工抽查不同，不将“有链接”称作全量行为实现证明。

## 实际静态校验

结果以包根[validation-report.json](../validation-report.json)为准，包括严格JSON、33份Schema、本地引用、35正例/35反例、5组夹具摘要、规范/测试/ADR/任务索引、原文切片、归档校验、内部链接和HTML锚点。ZIP封装前另核对MANIFEST.sha256，封装后检查CRC和路径唯一性。

离线HTML已检查内部链接、重复ID、外部运行资源依赖和内联JavaScript语法。当前环境禁止原生浏览器启动所需的socket，浏览器截图与真实交互渲染检查未完成；不将其记录为通过。完整Markdown及各分章是独立可读的交付内容。

## 没有执行的工程验证

未运行 PsiOS 产品代码、真实设备断电、数据库事务并发、资源端fencing、桌面输入/Portal、灾难恢复、协议互通、性能或正式发布流水线。技术决策仍Proposed，实现仍NotVerified，产品场景仍NotRun。文档工具通过不改变这些事实。
