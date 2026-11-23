# 在 QEMU 中安装并启动单节点 PsiOS

此配置使用固定的 Ubuntu Minimal 24.04 amd64，SeaBIOS 从独立的 QCOW2 系统盘引导。每次安装生成自己的 NoCloud 种子，里面包含本机提供的管理员 SSH **公钥**和已构建的 Core 包。首次开机在客户机内生成 Linux machine-id、SSH 主机密钥、PsiOS 节点和所有者凭据。安装 Core 的过程可在客户机无法出网时完成。适用范围是 QEMU 单节点开发与验收。Helium、Neon、Argon 的发行就绪验收，UEFI、Secure Boot、物理磁盘安装与引导回退另行跟踪。

## 创建实例

构建机使用 Python 3.11+、`dpkg-deb`、`qemu-img`、`genisoimage` 和 `ssh-keygen`。运行 VM 还需要 `qemu-system-x86_64` 和 SeaBIOS。先从成功的 CI 运行取得 Ubuntu amd64 包并校验清单，或在 Ubuntu 24.04 上按[宿主包指南](host-package.md)构建。包须来自管理员信任的来源，它的安装脚本将以客户机 root 身份执行。

```bash
curl -fL "$(python3 -c 'import json; print(json.load(open("images/qemu/base.json"))["url"])')" -o ubuntu-minimal.img
python3 images/qemu/create.py --base ubuntu-minimal.img \
  --package ./psios-core_0.1.0~dev1_amd64.deb --authorized-key ~/.ssh/id_ed25519.pub --output ./node-a
```

创建器核对基底文件大小和 SHA-256，检查包名与架构，要求输出目录尚不存在。生成的 `manifest.json` 记录原始基底、软件包、未启动的磁盘与种子摘要。基底摘要取自 Ubuntu 签名清单（签名者完整指纹在 `base.json` 中）。该基底用于本配置的实测，系统基底正式选型见 ADR-01。[固定基底](../images/qemu/base.json) · [Ubuntu 签名清单](https://cloud-images.ubuntu.com/minimal/releases/noble/release-20261105/SHA256SUMS.gpg) · [基底决策记录](v1/adr/ADR-001.md)

## 启动与管理

```bash
cd node-a
qemu-system-x86_64 -accel tcg -cpu max -m 1536 -smp 2 \
  -drive file=system.qcow2,format=qcow2,if=virtio \
  -drive file=seed.iso,format=raw,media=cdrom,readonly=on \
  -nic user,model=virtio-net-pci,restrict=on,hostfwd=tcp:127.0.0.1:2222-:22 -nographic
```

有可用 KVM 时可用 `-accel kvm -cpu host`。另一终端使用上述公钥对应的私钥登录。客户机仅将 SSH 转发至宿主回环地址。管理员用 sudo 管理 Linux，Core 所有者调用使用专用服务 UID。

```bash
ssh -p 2222 psios-admin@127.0.0.1
sudo cloud-init status --wait
sudo systemctl status psiosd
sudo -u psios psiosctl --socket /run/psios/api.sock node
sudo systemctl poweroff
```

再次启动使用同一块系统盘和种子。需要第二个节点时重新运行创建器，指定新目录和独立的宿主转发端口。已经启动的磁盘包含真实身份和工作负载状态，只应作为该节点的私有数据保存。

## 首次开机中断

通过 SSH 进入 Linux 后，查看 `journalctl -b -u psios-provision -u psios-init -u psiosd`。引导服务在调用初始化前将一次性意图写入 `/var/lib/psios-provision/initialization-issued` 并同步到磁盘。若凭据与数据库已成功生成，重新启动 `psios-provision.service` 会沿用原状态。若中断恰好发生在意图记录之后、Core 初始化之前，管理员应先核对 state 和 witness；仅在确认本次新安装的两个目录均为空时显式启动 `psios-init.service`，再重试引导服务。部分初始化或不匹配的见证需保留现场进行核验。安装完成标记在一次成功的本机 System API 调用之后写入。服务停用、离线核验和包升级沿用宿主安装流程。

验收用两份独立磁盘实际启动 Linux，通过 SSH 和本机 System API 执行任务，正常关机后重新启动，检查 boot ID 更换、节点与 machine-id 延续、原 Task 可读，并确认另一次安装具有不同身份。[QEMU 验收](../tests/qemu_boot.py)
