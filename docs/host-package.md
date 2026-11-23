# 在现有 Linux 主机安装 PsiOS Core

`psios-core` 包含本机权威服务、CLI、默认配置和两个 systemd 单元。构建针对当前主机架构。CI 在 Ubuntu 24.04 的 amd64 虚机上执行打包与服务生命周期验收。当前交付范围是已有 Linux 上的单节点 Core。三个发行版的启动介质、磁盘安装器、签名更新通道和系统部署回退仍待实现。

通过验收的 Actions 运行保存 `psios-core-ubuntu-24.04-amd64` 开发制品 14 天。归档含实际测试的 deb、dpkg buildinfo、源码提交、Rust 版本、Cargo.lock 和 SHA-256 清单。从对应的成功运行下载并解压后，在包目录执行 `sha256sum -c SHA256SUMS`。主线开发包的来源核验见下文。正式发行通道及其独立信任策略仍待建立。

## 核验主线开发包的来源

在 `main` 的构建记录中选择可信的完整提交 SHA，下载同一次运行的包制品，以及 `psios-core-build-attestation` 中的 `provenance.jsonl`。使用支持 `gh attestation verify --source-digest` 的 GitHub CLI，从可信源码 checkout 执行下例。第二个参数取自所选主线提交。

```bash
tools/verify-host-package ./host-build "$EXPECTED_MAIN_SHA" ./provenance.jsonl
```

核验器检查 GitHub 托管 runner 的构建身份证明，限定仓库、工作流、主线引用和提交。deb 和 SHA256SUMS 均为签名对象，buildinfo、源码版本、工具链和依赖锁文件由该清单覆盖。省略 bundle 参数会从 GitHub 取得证明；使用本地 bundle 仍需可用的受信验证根。主线仅在 Linux、包安装升级和虚机启动全部通过后签发。PR 构建可用于测试。正式发行密钥、更新准入和版本回退由后续发布流程管理。[构建证明机制](https://docs.github.com/en/actions/concepts/security/artifact-attestations)

## 构建与首次安装

在 Debian 系的构建机安装 `build-essential dpkg-dev debhelper`，按仓库根目录的 `rust-toolchain.toml` 安装 Rust。源依赖取自 `Cargo.lock`。使用完整 Git 历史，在仓库根目录按下例构建。构建器从指定提交导出源文件，在临时目录调用 dpkg 和 rustup。首次构建需要联网取得锁定的 crate 或事先填充 Cargo 缓存。

```bash
tools/build-host-package ./host-build
(cd host-build && sha256sum -c SHA256SUMS)
sudo apt install ./host-build/psios-core_*.deb
sudo systemctl start psios-init.service
sudo systemctl enable --now psiosd.service
sudo -u psios /usr/bin/psiosctl node
```

开发包版本在 changelog 版本后追加可达提交数和完整 Git SHA。提交数沿同一分支的祖先链严格递增，dpkg/apt 可分辨先后的构建；不同分支的顺序不代表发布先后。可用 `tools/build-host-package ./older-build REVISION` 构建已有 Debian 打包规则的指定提交。输出目录须尚不存在。未提交的工作树更改不参与构建。运行 `dpkg-query -W psios-core` 可查出已安装的源码版本。

首次安装只创建软件文件和专用的 `psios` 系统账户。管理员显式运行初始化单元，在私有目录中生成节点身份和所有者凭据。它调用与源码部署相同的排他初始化。二次初始化或存在残留文件时会失败。在已有手工部署上，先停旧服务，核对 `systemctl cat psiosd`、账户、配置、state 和 witness 的位置，再用原有状态启动新服务。两种部署都需要同一服务 UID 访问状态与所有者 API。

默认运行目录是 `/run/psios`，状态为 `/var/lib/psios`，见证和凭据在 `/var/lib/psios.witness`。配置由 dpkg 作为 conffile 管理。已安装的 `psiosctl` 与 `psiosd` 默认读取 `/etc/psios/core.json`，CLI 使用配置中的 socket 和见证目录。指定其他站点时可给任一命令传入 `--config FILE`；`--state DIR` 选择开发用的目录布局。已有系统配置读取失败时命令报错。显式提供 socket 的老客户端仍可独立运行，单独的 Agent 凭据用 `--token-file` 指定。更改持久目录时，还须在两个单元中同步配置可写路径、属主和挂载依赖。宿主管理员可按需向客户端开放 socket 的组访问，并单独登记和分发客户端凭据。包使用基础工具集，OCI 引擎访问和受管 systemd 单元由宿主管理员另行配置。

## 升级、卸载与恢复

升级前安排停机并保存相互匹配的状态和见证，遵循[状态格式升级说明](core-runtime.md)。dpkg 在替换文件后对运行中的服务执行 try-restart，已停止的服务保持停止。用 `systemctl status psiosd`、`journalctl -u psiosd -b` 和上述 CLI 命令核实节点与调度状态。

`apt remove psios-core` 停止服务，保留配置和数据。`apt purge psios-core` 还删除包管理的配置，但保留服务账户、数据库、卷、备份和见证。重新安装后，核对配置并启动原节点。状态或见证不完整时，保留现场，停服后用 `sudo -u psios /usr/bin/psiosd --config /etc/psios/core.json verify` 在私有副本上核验数据库和见证，报告原状态格式与迁移需求。后续显式使用 `check` 或启动服务才会完成受支持的迁移。降级二进制前需核对其可读格式。所有者凭据恢复见[单机 Core 指南](core-local.md)。

包生命周期验收会在一次性系统中真实安装、升级、停止、清除和重新安装包，并检查节点、任务回执及 Linux 权限。CI 从 PR #3 合并提交重新编译早期包，安装后产生实际任务和配置，再用当前提交的更高版本升级。跨格式升级另用 PR #1 的真实二进制验证。[包验收脚本](../tests/debian-install.sh) · [状态升级测试](../tests/upgrade.py)
