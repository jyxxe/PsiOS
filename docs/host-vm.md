# 在 Ubuntu Minimal 测试虚拟机中启动 PsiOS Core

此配置使用固定的 Ubuntu Minimal 24.04 amd64，支持 SeaBIOS 和标准 OVMF UEFI 两种固件，从独立的 QCOW2 系统盘引导。每次安装生成自己的 NoCloud 种子，里面包含本机提供的管理员 SSH **公钥**和已构建的 Core 包。首次开机在客户机内生成 Linux machine-id、SSH 主机密钥、PsiOS 节点和所有者凭据。安装 Core 的过程可在客户机无法出网时完成。适用范围是 QEMU 单节点开发与验收。Helium、Neon、Argon 的发行就绪验收，Secure Boot、物理磁盘安装与引导回退另行跟踪。

## 创建实例

构建机使用 Python 3.11+、`dpkg-deb`、`qemu-img`、`genisoimage` 和 `ssh-keygen`。运行 VM 还需要 `qemu-system-x86_64`，BIOS 模式使用 SeaBIOS，UEFI 模式使用发行版的 `ovmf` 包。先从成功的 CI 运行取得 Ubuntu amd64 包并校验清单，或在 Ubuntu 24.04 上按[宿主包指南](host-package.md)构建。包须来自管理员信任的来源，它的安装脚本将以客户机 root 身份执行。

```bash
curl -fL "$(python3 -c 'import json; print(json.load(open("images/qemu/base.json"))["url"])')" -o ubuntu-minimal.img
python3 images/qemu/create.py --base ubuntu-minimal.img \
  --package ./host-build/psios-core_*.deb --authorized-key ~/.ssh/id_ed25519.pub --output ./node-a
```

创建器核对基底文件大小和 SHA-256，检查包名与架构，要求输出目录尚不存在。生成的 `manifest.json` 记录原始基底、软件包、未启动的磁盘与种子摘要。基底摘要取自 Ubuntu 签名清单（签名者完整指纹在 `base.json` 中）。该基底用于本配置的实测，系统基底正式选型见 ADR-01。[固定基底](../images/qemu/base.json) · [Ubuntu 签名清单](https://cloud-images.ubuntu.com/minimal/releases/noble/release-20261105/SHA256SUMS.gpg) · [基底决策记录](v1/adr/ADR-001.md)

使用 UEFI 时，在创建命令增加 `--firmware uefi`。默认从 `/usr/share/OVMF` 复制发行版的标准 `OVMF_CODE_4M.fd` 与空白 `OVMF_VARS_4M.fd`。也可用 `--ovmf-dir DIR` 指定可信的固件目录。每台实例保有自己的只读固件和可写变量存储，清单记录初始摘要。变量会随客户机运行而变化，保存或搬迁实例时应连同该文件一起保存。当前 UEFI 配置未启用 Secure Boot。

## 启动与管理

从仓库根目录启动。启动器读取实例清单选择与创建时相同的固件。它复核 OVMF 代码摘要，使用该实例的 EFI 变量，在 KVM 可用时使用 KVM，否则使用 TCG。开发配置把 SSH 转发到宿主回环地址，并限制客户机向外发起网络连接。

```bash
python3 images/qemu/run.py ./node-a --ssh-port 2222
```

`--accel tcg` 可显式使用软件模拟，`--serial-log FILE` 可将串口日志写到指定文件。另一终端使用上述公钥对应的私钥登录。管理员用 sudo 管理 Linux，Core 所有者调用使用专用服务 UID。

```bash
ssh -p 2222 psios-admin@127.0.0.1
sudo cloud-init status --wait
sudo systemctl status psiosd
sudo -u psios psiosctl node
sudo systemctl poweroff
```

再次启动使用同一块系统盘和种子。需要第二个节点时重新运行创建器，指定新目录和独立的宿主转发端口。已经启动的磁盘包含真实身份和工作负载状态，只应作为该节点的私有数据保存。

## 首次开机中断

通过 SSH 进入 Linux 后，查看 `journalctl -b -u psios-provision -u psios-init -u psiosd`。引导服务在调用初始化前将一次性意图写入 `/var/lib/psios-provision/initialization-issued` 并同步到磁盘。若凭据与数据库已成功生成，重新启动 `psios-provision.service` 会沿用原状态。若中断恰好发生在意图记录之后、Core 初始化之前，管理员应先核对 state 和 witness；仅在确认本次新安装的两个目录均为空时显式启动 `psios-init.service`，再重试引导服务。部分初始化或不匹配的见证需保留现场进行核验。安装完成标记在一次成功的本机 System API 调用之后写入。服务停用、离线核验和包升级沿用宿主安装流程。

验收分别从 BIOS 和 UEFI 启动两份独立实例，核实客户机实际固件模式、节点身份和 SSH 管理。BIOS 路径正常关机重启；UEFI 路径写入非易失 EFI 变量、取得已成功的 Task 回执后强制终止 QEMU。再次启动核对变量、原节点与 machine-id、变化后的 boot ID，以及使用原提交键重取的 Task 和结果。此处故障覆盖运行中虚机突然丢失，宿主机物理断电与整机部署回退仍待验收。[QEMU 验收](../tests/qemu_boot.py)
