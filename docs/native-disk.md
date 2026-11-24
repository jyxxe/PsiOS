# PsiOS 独立 UEFI 开发盘

此安装候选把同一份静态 Core、Linux 内核、早期用户空间和 GNU GRUB 组装成每实例一份的 GPT raw 磁盘。amd64 QEMU 的 OVMF 直接从 EFI 系统分区加载，根用户空间在 RAM 中运行，第二分区的 ext4 持久保存 /etc、Core 状态、见证和卷。第三分区单独为控制台账户保存主目录。启动与管理均可断网完成。此组合的内核、模块及 BusyBox 来自固定摘要的 Alpine virt 输入。首批正式平台与许可证分发清单仍由 ADR-01 跟踪。[基底候选要求](v1/adr/ADR-001.md)

## 生成新盘

使用 Linux、Python 3.11+、GNU cpio、isoinfo、GNU GRUB 的 x86_64-efi 模块、mtools、mkfs.fat、sfdisk 及 e2fsprogs。构建可以普通用户运行，目标必须是新文件。先对完整 Core 归档核验主线构建证明，再将已核验归档的摘要传给构建器。内核 ISO 的位置与摘要见输入清单，可在联网机器取得后传入离线环境。[归档核验](core-local.md#portable-amd64-core) · [内核输入](../images/rescue/base.json)

在私有目录解包已核验的完整归档，进入解包目录并执行下列命令。安装器及启动脚本在归档的 `native/` 目录，内核输入清单在同级 `rescue/` 目录。[随包说明](../images/native/README.txt)

OpenSSL 在终端内询问新密码。在每台实例上使用自己的控制台凭据，构建输出的状态 UUID 绑定本次部署。

```sh
umask 077
sha256sum --strict --check SHA256SUMS
openssl passwd -6 > operator.hash
python3 native/create.py node.raw --kernel-iso alpine-virt.iso \
  --core-archive psios-core-linux-amd64.tar.gz --core-sha256 "$VERIFIED_CORE_SHA256" \
  --console-hash operator.hash > node-build.json
cp /usr/share/OVMF/OVMF_VARS_4M.fd node-vars.fd
qemu-system-x86_64 -nodefaults -no-user-config -machine q35 -m 768 -smp 2 \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,unit=1,file=node-vars.fd \
  -drive if=virtio,format=raw,file=node.raw -boot order=c,strict=on \
  -display none -serial stdio -monitor none -nic none
```

串口账户 `psios` 和服务使用 UID 1000。登录后可运行 `psiosctl whoami`、`psiosctl node`，或用 `printf '{}' | psiosctl submit system.status --key first-status` 提交任务。需要传统系统管理时，可在生成新盘时传入 `--root-console-hash root.hash` 设置独立的本机 root 密码。默认锁定 root。启用后可用 root 在串口执行 `poweroff`。关机钩子先允许 Core 结算，随后关闭其余会话与进程，刷盘并卸载状态和主目录。运行时生成的所有者令牌位于服务账户的私有见证目录。[关机验收](../tests/native_disk.py)

每次开机在 `/tmp` 创建独立的 tmpfs，最多使用 32 MiB 数据空间和 4096 个 inode。普通账户可以创建临时文件，目录的 sticky bit 保护其他账户的条目，挂载启用 `nodev` 与 `nosuid`。达到任一上限时，内核拒绝继续分配。重启后临时文件消失。可将需要保留的用户文件放在持久主目录。

## 可选有线接入

新盘可用 `--dhcp-mac 52:54:00:51:6f:20` 启用单个 virtio 网口的 IPv4 DHCP。为虚机配置同一个 MAC。下面的 QEMU 参数可用于只验证租约的隔离网络。

```sh
-netdev user,id=lan,restrict=on \
-device virtio-net-pci,netdev=lan,mac=52:54:00:51:6f:20,romfile=
```

该配置独占匹配网口的 IPv4 地址、DHCP 默认路由和 `/etc/resolv.conf`。DHCP 提供地址后，默认路由和最多三个 DNS 服务器按租约配置，释放租约时清理。普通账户可用 `busybox ip -4 addr show`、`busybox ip -4 route show` 和 `cat /etc/resolv.conf` 检查。最新租约或故障信息在 `/run/psios-network/status`。配置的网口缺失时，串口账户与 Core 可照常使用。静态网络配置、无线、IPv6 管理和远程配置事务仍待开发。[隔离网络中的真实续租和释放验收](../tests/native_dhcp.py)

## 核对安装与运行的制品

构建成功后，将 `node-build.json` 保存在节点外。其中 `initial_image_sha256` 可在首次启动前与 `sha256sum node.raw` 比对。启动会改变数据分区，离线核对引导程序可单独读取 EFI 系统分区。

```sh
mcopy -i node.raw@@1048576 ::/EFI/BOOT/BOOTX64.EFI boot.efi
sha256sum boot.efi
```

将结果与回执的 `efi_application_sha256` 比对。启动后普通账户可运行 `sha256sum /psios-deployment.json` 与回执的 `deployment_manifest_sha256` 比对。该清单列出 Core 源码修订及归档摘要、内核 ISO 及其内核与初始内存盘的输入摘要、两个文件系统 UUID 和 PsiOS 加入启动环境的文件摘要。回执另含组装后的完整 initramfs 摘要。PID 1 在挂载可写分区前核对清单和这些文件，发现不一致时给出 `deployment-payload` 停机阶段。外部回执供操作者比对，固件信任链与远程启动证明仍待验证。

## 持久性与救援

每次启动在挂载前核对状态和主目录的文件系统 UUID 及分区布局，要求匹配的两个分区在同一块 virtio 磁盘上。额外的 virtio 数据盘可随系统一起连接。发现部署 UUID 重复或匹配分区分属两块磁盘时，启动在挂载前停止。[双盘验收](../tests/native_storage.py)

首次启动核对部署记录，在私有暂存目录创建并核验 Core 状态与见证，再逐个发布两个目录。在准备期间断电，可重新准备未发布的权威。发布开始后断电，启动会核验并继续安装原节点的状态与见证。初始化完成标记持久落盘后才开启本机管理。已有节点丢失状态时，启动检查会报错并保留原见证供救援。启动检查失败时，串口会指明失败阶段，系统关闭已经挂载的持久分区后停机。保留原盘，使用同版本救援归档检查 `--partition 2 --state /psios --uid 1000`。强制断电产生的待重放日志可用有容量限制的临时覆盖层检查。[首次启动断电验收](../tests/native_enrollment.py) · [离线救援](offline-rescue.md)

本版生成的 323 MiB 新盘给主目录分配独立的 64 MiB ext4。普通账户可以在 `/home/psios` 保存文件并执行脚本，root 的 `/root` 独立设为私有，两者共享这 64 MiB。主目录写满时仍可向 Core 提交任务。第三分区丢失或 UUID 不符时，启动在挂载前停止。早期两分区开发盘的主目录在 RAM，已有磁盘继续使用它们原有的引导程序。

此开发盘使用 virtio 存储、标准 4 MiB amd64 OVMF 和本机串口。本机账户及密码摘要保存在状态分区的 /etc 配置中。root 可运行 passwd psios 更换密码，重启后生效。磁盘数据为明文。状态与见证位于同一物理故障域。新节点应重新生成磁盘。物理安装、Secure Boot、加密、自动系统更新与回退，以及三个发行版的 EditionReady 验收尚待实现或验证。[虚机故障验收](../tests/native_disk.py)

## 停机升级到另一份原生磁盘

适用于已初始化且停机的 323 MiB 三分区开发盘，使用标准 `/var/lib/psios` 和 `/var/lib/psios.witness`。保留当前可用的 EFI 变量文件和节点外可信构建回执。使用目标版本的已核验 Core 归档及安装器。构建机还需 `qemu-img`、`qemu-system-x86_64` 和约 1 GiB 可用工作空间。

```sh
umask 077
python3 native/create.py next.raw --upgrade-from node.raw --previous-receipt node-build.json \
  --kernel-iso alpine-virt.iso --core-archive psios-core-linux-amd64.tar.gz \
  --core-sha256 "$VERIFIED_CORE_SHA256" > next-build.json
```

安装器用 QEMU 的一致性读锁复制源盘，检查原 GPT 布局、旧 EFI 摘要、文件系统 UUID 和部署标记。待重放的 ext4 内部日志在固定大小的私有分区副本上重放，随后执行只读文件系统检查。新版本 Core 由独立救援内核在只读候选盘上核验权威及见证。通过后替换候选盘的 EFI 分区，保留原 GPT、状态、卷、凭据、站点配置和主目录。回执记录源盘摘要、目标制品摘要、升级前的数据库格式及已重放日志的分区。活动的 QEMU 写入、身份不匹配或需要进一步修复时停止发布。创建完成后可核对 `initial_image_sha256`。[QEMU 文件锁](https://www.qemu.org/docs/master/system/qemu-block-drivers.html#disk-image-file-locking) · [ext4 日志重放](https://man7.org/linux/man-pages/man8/e2fsck.8.html) · [救援核验](offline-rescue.md)

保留原盘，使用 EFI 变量文件的副本启动 `next.raw`。启动检查会按新 Core 的格式要求迁移候选盘。登录后比对 `/psios-deployment.json` 摘要、节点 ID 和已有任务回执，再恢复工作流。原盘与候选盘共享节点身份和密钥，只启动其中一个。验收失败且尚未产生需保留的新写入时，可停掉候选盘并用原盘及对应的 EFI 变量文件重新启动。候选盘开始接受业务写入后，回退前需要另行处理候选盘内的新状态与外部效果。[双版本启动与保留数据测试](../tests/native_upgrade.py)
