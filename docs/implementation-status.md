# 实现与验证进度

截至 2026-11-23，PsiOS 单节点 Core 提供独立的 Linux amd64 静态二进制，以及供 Ubuntu 24.04 宿主使用的可选 .deb 包。Alpine 用户空间用于验证 Core 独立运行。Ubuntu Minimal QEMU 开发环境用于验证 systemd 和虚拟机引导。独立的 UEFI 开发盘可直接从 GPT 启动，断网使用本机账户与 CLI，并把节点状态保存在 ext4。CLI 和本机 Agent 都通过同一权威执行任务。PsiOS 原生镜像基底仍由 ADR-01 跟踪，Helium、Neon、Argon 的 EditionReady 验收尚待完成。[独立运行](core-local.md#portable-amd64-core) · [基底决策](v1/adr/ADR-001.md) · [启动验收](../tests/qemu_boot.py) · [独立开发盘](native-disk.md)

| 领域 | 已实现并验证的范围 | 部分支持、待开发或待取得的证据 |
| --- | --- | --- |
| 安装、升级与发布 | Ubuntu 24.04 amd64 上用 apt 从实际旧包升级，验证启停、清除、重装、配置和节点数据保留。QEMU 的 SeaBIOS 和 OVMF 配置从固定 Ubuntu Minimal 基底独立启动。覆盖正常重启、UEFI 虚机被强制终止后重启，以及节点、EFI 变量与 Task 回执的保留。 | 开发包按源码版本排序；CI 暂存制品 14 天，独立二进制归档与可选 .deb 分别由对应工作流签发主线源码构建证明。正式签名渠道、物理安装、整机部署升级和引导回退待开发。Secure Boot 和真实硬件验收仍需对应环境。[包验收](../tests/debian-install.sh) · [安装说明](host-vm.md) |
| 宿主与网络 | 查询真实 procfs、sysfs 和本机网络命名空间。接口 Task 有独立的节点授权。真实 systemd 单元可在配置的允许范围内查询、启停及核实未知结果。 | 已覆盖观察网卡、地址及对端；网络配置事务、路由、DNS、驱动与固件生命周期待开发。[宿主管理](core-runtime.md) · [网络测试](../tests/test_network.py) · [服务测试](../tests/services.py) |
| Core、身份与恢复 | 本机 Unix socket 核验主体与 UID。资源版本、审批、授权预算、Task、Operation 和事件共享 SQLite 权威。调度器支持并发、排空新领取、取消及幂等重试。停服核验可在私有副本检查 WAL、外键、见证和所有者凭据。宿主无法启动时，独立救援内核可从 raw ext4 或 GPT 分区的只读镜像检查同一权威，支持在限额临时副本上重放断电后的 ext4 日志。严格只读检查还可构建并导出经过 SeaBIOS、OVMF 启动验证的救援光盘。 | 权威在单机；数据库与见证同时回滚或整块介质丢失时仍需独立信任锚和恢复流程。历史资源保留与磁盘长期回收尚不完整。[系统测试](../tests/system.py) · [外部救援与限制](offline-rescue.md) · [故障恢复](../tests/test_offline.py) · [旧状态升级](../tests/upgrade.py) |
| 应用与存储 | Docker OCI 按本机镜像摘要安装、限额运行、停止、切换或选回 Release。支持单卷文件访问、内容核验、发布前复核源文件与目录的离线备份、按清单原地核验和恢复至新卷。已核实启动检查点的运行中容器可在回执丢失后结算原 Task。 | 无检查点、容器已退出等场景仍保持效果未知。跨机备份、在线多卷一致性、Flatpak、第三方商店与可选 Dataset、Knowledge、TableFS 待开发。[OCI 真机引擎测试](../tests/oci.py) · [丢回执测试](../tests/oci_recovery.py) · [存储范围](core-local.md) |
| Agent 与协作 | 本机提供独立 Agent 和 Provider 身份、版本化 Skill、绑定 Linux 启动身份与挂起计时的模型租约、持久多轮 Session、受权工具派发与取消。定向信号支持幂等发送、等待、重投、条件确认和一次性触发会话。 | 由客户端驱动模型轮次；持续自主触发、云模型适配与计费、完整 Service Bus、Agent Chat、PsiFlow、Project 和 Computer Use 待开发。[运行协议](core-runtime.md) · [多主体测试](../tests/test_runtime.py) |
| 用户交互与集群 | 本机 CLI、Rust SDK 和无需模型的传统系统管理入口可用。两台独立虚机已验证会生成不同的身份。 | Shell/WebUI、Argon KDE 与 GUI Bridge、节点加入和退出、跨节点路由、租约共识及故障接管待开发。[本机入门](core-local.md) · [集群规范](v1/docs/12-cluster-coordination.md) |

## 如何复核

每次合入后可在相应提交的 Actions 中查看各项作业。Linux 作业运行契约、进程故障、真实 Docker 和 systemd 测试；host-package 实际安装并升级；guest-boot 使用该次打出的包启动虚机。独立工作流用 Alpine 构建，在断网的非特权用户空间执行系统测试，随后为主线归档签发证明并在禁网环境核验。主线的 package-provenance 在 .deb 对应的检查通过后签发证明，verify-provenance 独立核验并测试被更改的包将被拒绝。单项通过仅覆盖脚本中的平台与故障条件。v1 原始包的校验报告记录文档一致性，保留在其发布目录内。[宿主 CI](../.github/workflows/core.yml) · [独立 Core CI](../.github/workflows/portable.yml) · [主线运行记录](https://github.com/jyxxe/PsiOS/actions?query=branch%3Amain) · [验收规范](v1/docs/15-acceptance-and-evidence.md)
