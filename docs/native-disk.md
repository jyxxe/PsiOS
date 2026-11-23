# PsiOS 独立 UEFI 开发盘

此安装候选把同一份静态 Core、Linux 内核、早期用户空间和 GNU GRUB 组装成每实例一份的 GPT raw 磁盘。amd64 QEMU 的 OVMF 直接从 EFI 系统分区加载，根用户空间在 RAM 中运行，第二分区的 ext4 持久保存 /etc、Core 状态、见证和卷。第三分区单独为控制台账户保存主目录。启动与管理均可断网完成。此组合的内核、模块及 BusyBox 来自固定摘要的 Alpine virt 输入。首批正式平台与许可证分发清单仍由 ADR-01 跟踪。[基底候选要求](v1/adr/ADR-001.md)

## 生成新盘

使用 Linux、Python 3.11+、GNU cpio、isoinfo、GNU GRUB 的 x86_64-efi 模块、mtools、mkfs.fat、sfdisk 及 e2fsprogs。构建可以普通用户运行，目标必须是新文件。先对完整 Core 归档核验主线构建证明，再将已核验归档的摘要传给构建器。内核 ISO 的位置与摘要见输入清单，可在联网机器取得后传入离线环境。[归档核验](core-local.md#portable-amd64-core) · [内核输入](../images/rescue/base.json)

在私有目录执行下列命令。OpenSSL 在终端内询问新密码。在每台实例上使用自己的控制台凭据，构建输出的状态 UUID 绑定本次部署。

```sh
umask 077
openssl passwd -6 > operator.hash
python3 images/native/create.py node.raw --kernel-iso alpine-virt.iso \
  --core-archive psios-core-linux-amd64.tar.gz --core-sha256 "$VERIFIED_CORE_SHA256" \
  --console-hash operator.hash
cp /usr/share/OVMF/OVMF_VARS_4M.fd node-vars.fd
qemu-system-x86_64 -nodefaults -no-user-config -machine q35 -m 768 -smp 2 \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,unit=1,file=node-vars.fd \
  -drive if=virtio,format=raw,file=node.raw -boot order=c,strict=on \
  -display none -serial stdio -monitor none -nic none
```

串口账户 `psios` 和服务使用 UID 1000。登录后可运行 `psiosctl whoami`、`psiosctl node`，或用 `printf '{}' | psiosctl submit system.status --key first-status` 提交任务。需要传统系统管理时，可在生成新盘时传入 `--root-console-hash root.hash` 设置独立的本机 root 密码。默认锁定 root。启用后可用 root 登录串口执行 `poweroff` 正常关机。运行时生成的所有者令牌位于服务账户的私有见证目录。

## 持久性与救援

首次启动在挂载前分别核对状态和主目录的文件系统 UUID，核对部署记录，在磁盘上持久消费一次性初始化标记，然后创建节点。每次启动在启动服务前复核 Core 权威及见证。初始化中断或状态盘不匹配时，系统停机并在串口指明失败阶段。保留原盘，使用同版本救援归档检查 `--partition 2 --state /psios --uid 1000`。强制断电产生的待重放日志可用有容量限制的临时覆盖层检查。[离线救援](offline-rescue.md)

本版生成的 323 MiB 新盘给主目录分配独立的 64 MiB ext4。普通账户可以在 `/home/psios` 保存文件并执行脚本；root 的 `/root` 独立设为私有，两者共享这 64 MiB。主目录写满时仍可向 Core 提交任务。第三分区丢失或 UUID 不符时，启动在挂载前停止。早期两分区开发盘的主目录在 RAM，已有磁盘继续使用它们原有的引导程序；此构建器只创建新盘。

该组合限单个 virtio 系统盘、标准 4 MiB amd64 OVMF 和本机串口。本机账户及密码摘要保存在状态分区的 /etc 配置中。root 可运行 passwd psios 更换密码，重启后生效。磁盘数据为明文。状态与见证位于同一物理故障域。新实例应重新生成磁盘，复制已经初始化的镜像也会复制节点身份和密钥。物理安装、Secure Boot、加密、系统 A/B 更新及回退，以及三个发行版的 EditionReady 验收尚待实现或验证。[虚机故障验收](../tests/native_disk.py)
