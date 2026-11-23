# PsiOS — 实施路线与验收清单（项目导入版）

状态：规划中，NotVerified / NotRun。此文件是实施任务说明，不代表已创建 Forgejo Issues 或代码提交。

## 项目定位

PsiOS 是 Linux 上的 Agent-Native OS，共用 PsiOS Core，首批发行形态：Helium（精简）、Neon（服务器）、Argon（KDE Plasma 桌面）。Agent 原生/第三方兼容、集群接入为三版核心目标；Dataset/Knowledge 可选，TableFS 是可选 StorageDriver。一般应用与工具经 OCI/Flatpak/隔离环境运行；不能要求宿主内核和驱动容器化。PsiOS Shell 统一 WebUI，不能替代 KDE Plasma。

## M0–M6

| 阶段 | 交付主题 | 退出判据 |
|---|---|---|
| M0 | 最新 Forgejo HEAD 审计，主架构/资源 API/安全状态/任务契约和 ADR-01～25 | 清楚记录模块单一权威、Schema、失败状态、代码复用边界和契约测试；R-01～R-05 危险场景可表达/测试 |
| M1 | 最小可信宿主、镜像、引导、离线账户、CLI 和独立救援 | 断模型/网络/容器引擎仍能启动本机诊断、恢复，已记录可重建镜像摘要 |
| M2 | Identity/Policy/Broker、OCI Application、Storage Volume/Backup、持久 Task/Event | 从独立备份恢复有状态应用；撤权数据面/网络错误配置回滚、outbox 与可重复结果验证 |
| M3 | 原生 Agent、第三方 C1/C2、MCP、Skills、单机 PsiFlow/Agent Chat/DM | 同类工具任务、预算、权限、取消、ACK丢失/崩溃恢复、双 Agent 会话/DM 验收 |
| M4 | 三版、单控制器与 Worker、跨节点协作、KDE/GUI Bridge、包适配、Forgejo/CI | 分区无双写、共享租约被 fencing、人类接管、GUI/多发行版包受限 Profile 实测 |
| M5 | 可选数据服务、Knowledge 视图、TableFS 单机对照、存储增强 | 索引与原文单一来源，POSIX 子集实测、完整备份/导出、性能对比 |
| M6 | 发布质量、正式兼容矩阵、SBOM、签名、升级回退及故障演练 | 通过确切镜像/硬件/Profile 的测试，发布声明只包含 Passed 能力 |

## 首批 P0 工作项

1. `P0-01` 核验 PsiOS 最新提交，审查核心模块接口、依赖及许可证。
2. `P0-02` 建立 docs/architecture、docs/adr、api/schema、tests/contracts，CI 保留带 SHA 的证据。
3. `P0-03` 统一 Principal/Grant/Delegation/Approval/Operation、Task/Run、WorkflowRun、Conversation/Message Schema。
4. `P0-04` 实现并验证 SecurityState 受限灾难恢复，确认无独立可信撤权记录时默认隔离旧权限。
5. `P0-05` 制作 Workflow→Task DispatchIntent + outbox + Task 接受去重/回执丢失对账 PoC。
6. `P0-06` 设计 Conversation WriterEpoch、存储端 fencing 和分区不重复确认。
7. `P0-07` 测试真实用户目的/允许副作用约束，以及 RevocationMode 数据面撤权。
8. `P0-08` M1 引导/恢复与 M2 OCI + Volume + Backup 基础链路。

## P1/P2 工作项

- 第三方 Agent 与 MCP 2026/旧版适配；定版 Workflow DSL 和审批/预算语义。
- Room/DM/Thread/Delivery/Subscription 和 Project/Forgejo/CI 外部事件对账。
- KDE Portal 与无头 GUI Bridge、输入租约、会话恢复与浏览器 Viewer 生命周期。
- Linux DEB/RPM/Alpine APK 隔离导入、ABI/安装脚本限制、依赖安全重建。
- GPU/设备、HostExtension、Kernel ABI、宿主更新与救援。
- 三版资源预算、CI Profile；TableFS 与传统 FS + Index 基准；Dataset/Knowledge 与 JSON 精确查询。

## 实际验证记录模板

```
requirement_id:
adr_id:
work_item:
owner:
repo_ref:
branch:
commit_sha:
image_digest:
profile_edition:
test_environment:
fault_injection_point:
expected_behavior:
observed_behavior:
logs_artifacts:
checksum:
reviewer:
result: NotRun | Passed | Failed | Blocked | NotApplicable
limitations:
```

## 规范入口

- 合并规范 v0.6：https://chatgpt.com/space/page_e72f183122b08191ac04e6990c6c35b1
- 风险台账：https://chatgpt.com/space/page_c1cd937805a0819191a370c09f64f5ae
- 已保存实施看板：https://chatgpt.com/space/page_b3a106d044988191abfda348ef0c9ca9
