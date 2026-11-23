# 在现有 Linux 主机安装 PsiOS Core

`psios-core` 包含本机权威服务、CLI、默认配置和两个 systemd 单元。构建针对当前主机架构。CI 在 Ubuntu 24.04 的 amd64 虚机上执行打包与服务生命周期验收。当前交付范围是已有 Linux 上的单节点 Core。三个发行版的启动介质、磁盘安装器、签名更新通道和系统部署回退仍待实现。

## 构建与首次安装

在 Debian 系的构建机安装 `build-essential dpkg-dev debhelper`，按仓库根目录的 `rust-toolchain.toml` 安装 Rust。源依赖取自 `Cargo.lock`。使用外部 rustup 时，dpkg 的依赖库没有这份 Rust 工具链的登记，可按下例构建本机二进制包。首次构建需要联网取得锁定的 crate 或事先填充 Cargo 缓存。

```bash
dpkg-buildpackage -b -us -uc -d
sudo apt install ../psios-core_*.deb
sudo systemctl start psios-init.service
sudo systemctl enable --now psiosd.service
sudo -u psios /usr/bin/psiosctl --socket /run/psios/api.sock node
```

首次安装只创建软件文件和专用的 `psios` 系统账户。管理员显式运行初始化单元，在私有目录中生成节点身份和所有者凭据。它调用与源码部署相同的排他初始化。二次初始化或存在残留文件时会失败。在已有手工部署上，先停旧服务，核对 `systemctl cat psiosd`、账户、配置、state 和 witness 的位置，再用原有状态启动新服务。两种部署都需要同一服务 UID 访问状态与所有者 API。

默认运行目录是 `/run/psios`，状态为 `/var/lib/psios`，见证和凭据在 `/var/lib/psios.witness`。配置由 dpkg 作为 conffile 管理。更改持久目录时，还须在两个单元中同步配置可写路径、属主和挂载依赖。宿主管理员可按需向客户端开放 socket 的组访问，并单独登记和分发客户端凭据。包使用基础工具集，OCI 引擎访问和受管 systemd 单元由宿主管理员另行配置。

## 升级、卸载与恢复

升级前安排停机并保存相互匹配的状态和见证，遵循[状态格式升级说明](core-runtime.md)。dpkg 在替换文件后对运行中的服务执行 try-restart，已停止的服务保持停止。用 `systemctl status psiosd`、`journalctl -u psiosd -b` 和上述 CLI 命令核实节点与调度状态。

`apt remove psios-core` 停止服务，保留配置和数据。`apt purge psios-core` 还删除包管理的配置，但保留服务账户、数据库、卷、备份和见证。重新安装后，核对配置并启动原节点。状态或见证不完整时，保留现场，停服后用 `sudo -u psios /usr/bin/psiosd --config /etc/psios/core.json check` 查看诊断。此命令会执行受支持的状态格式迁移；降级二进制前需核对其可读格式。所有者凭据恢复见[单机 Core 指南](core-local.md)。

包生命周期验收会在一次性系统中真实安装、升级、停止、清除和重新安装包，并检查节点、任务回执及 Linux 权限。包版本升级用相同载荷的较早测试修订触发 dpkg，跨格式升级另用 PR #1 的真实二进制验证。[包验收脚本](../tests/debian-install.sh) · [状态升级测试](../tests/upgrade.py)
