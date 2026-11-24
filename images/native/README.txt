PsiOS 独立 UEFI 开发盘安装工具（amd64）

在 Linux 上预备 Python 3.11+、GNU cpio、isoinfo、GNU GRUB 的
x86_64-efi 模块、mtools、mkfs.fat、sfdisk、e2fsprogs 和 openssl。
准备经主线构建证明核验的完整 psios-core-linux-amd64.tar.gz，并从
rescue/base.json 所列来源取得固定内核 ISO。下载结束后可以断网。

在私有目录解包完整归档。核验 SHA256SUMS 后执行以下命令。
ARCHIVE 和 KERNEL_ISO 分别是已核验完整归档与内核 ISO 的绝对路径，
VERIFIED_CORE_SHA256 是上述完整归档核验后记录的 SHA-256。

    umask 077
    sha256sum --strict --check SHA256SUMS
    openssl passwd -6 > operator.hash
    python3 native/create.py node.raw --kernel-iso "$KERNEL_ISO" \
      --core-archive "$ARCHIVE" --core-sha256 "$VERIFIED_CORE_SHA256" \
      --console-hash operator.hash > node-build.json

将 node-build.json 保存在节点外。首次启动前可核对 initial_image_sha256，
启动后可核对 /psios-deployment.json 的 SHA-256 和记录的 Core 修订。
新盘可用 --state-mib 与 --home-mib 分别设置状态分区和主目录容量。
默认及最小容量为 192 MiB 和 64 MiB，整盘上限为 8192 MiB。
构建回执和来宾部署清单记录 geometry_mib。

构建器以当前用户创建全新的 GPT raw 文件。其内核和早期用户空间来自
核验过的 ISO。每个节点分别生成新盘并设置自己的凭据。需要本机 root
管理时可另用 openssl passwd -6 创建私有摘要文件，通过
--root-console-hash 单独设置；默认锁定 root。

为每个节点复制标准 4 MiB amd64 OVMF 变量文件。使用独占的系统盘和
变量文件启动，串口登录用户名为 psios。示例使用 QEMU 的标准 OVMF 路径。

    cp /usr/share/OVMF/OVMF_VARS_4M.fd node-vars.fd
    qemu-system-x86_64 -nodefaults -no-user-config -machine q35 -m 768 -smp 2 \
      -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
      -drive if=pflash,format=raw,unit=1,file=node-vars.fd \
      -drive if=virtio,format=raw,file=node.raw -boot order=c,strict=on \
      -display none -serial stdio -monitor none -nic none

登录后可执行 psiosctl whoami。外部只读核验使用同一归档的
rescue/inspect.py，选择 --partition 2 --state /psios --uid 1000。
救援参数见 rescue/README.txt。本开发盘的验收范围是 amd64 UEFI 虚机。
每次开机从空的 /tmp 开始，供普通账户写入，容量为 32 MiB 和 4096 个
inode。持久用户文件可放在主目录。
启动检查失败时，串口报告失败阶段，系统关闭已挂载的持久分区后停机。
可附加 virtio 数据盘。启动阶段按部署 UUID 选择同一系统盘上的状态与
主目录分区。部署身份重复或分区分属不同磁盘时会拒绝启动。
首次启动会在私有暂存区创建并检查状态与见证，然后发布两个目录。
初始化期间断电后可再次启动，以恢复该次初始化。

生成新盘时可用 --dhcp-mac 指定一个 virtio 网口并启用 IPv4 DHCP。
为虚机提供同一 MAC 的网口。该配置管理此网口的 IPv4 地址、DHCP
租约路由和运行时 DNS。支持选项 121 的直连和经网关路由。普通账户可使用 busybox ip -4 addr show、
busybox ip -4 route show 和 cat /etc/resolv.conf 查看。

升级已初始化的三分区开发盘时，先停止虚机，保留原 raw 磁盘、EFI
变量文件和可信的构建回执。使用目标版本的归档和安装器，另需 qemu-img
及 qemu-system-x86_64。准备一份新的候选盘。

    python3 native/create.py next.raw --upgrade-from node.raw \
      --previous-receipt node-build.json --kernel-iso "$KERNEL_ISO" \
      --core-archive "$ARCHIVE" --core-sha256 "$VERIFIED_CORE_SHA256" > next-build.json

构建器核验原启动文件、文件系统和 Core 权威，待重放的 ext4 内部
日志在限额副本中处理。新盘沿用原节点身份、
配置和账户。保留原盘及其 EFI 变量，使用变量文件的副本启动候选盘。
升级尺寸取自已核实的原盘布局。版本 1 回执使用 323 MiB 布局。
核对候选盘的部署清单和节点身份后再恢复工作。原盘与候选盘只启动
其中之一。验收失败可关闭候选盘并启动原盘。已有业务写入时，先处置
候选盘内的新状态与外部效果。
