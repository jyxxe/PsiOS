# PsiOS 独立 UEFI 开发盘

此安装候选把同一份静态 Core、Linux 内核及早期用户空间组装成每实例一份的 GPT raw 磁盘。amd64 QEMU 的 OVMF 从 EFI 系统分区加载标准 GRUB 或已签名的统一内核镜像。根用户空间在 RAM 中运行，第二分区的 ext4 持久保存 /etc、Core 状态、见证和卷。第三分区单独为控制台账户保存主目录。启动与管理均可断网完成。此组合的内核、模块及 BusyBox 来自固定摘要的 Alpine virt 输入。首批正式平台与许可证分发清单仍由 ADR-01 跟踪。[基底候选要求](v1/adr/ADR-001.md)

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

生成新盘时，用 `--state-mib` 和 `--home-mib` 分别指定两个 ext4 分区的容量。默认值和最小值分别为 192 MiB 和 64 MiB。完整 raw 镜像最多 8192 MiB，含 EFI 分区和 GPT 空间。安装回执和来宾的 `/psios-deployment.json` 均记录 `geometry_mib`。

串口账户 `psios` 和服务使用 UID 1000。登录后可运行 `psiosctl whoami`、`psiosctl node`，或用 `printf '{}' | psiosctl submit system.status --key first-status` 提交任务。需要传统系统管理时，可在生成新盘时传入 `--root-console-hash root.hash` 设置独立的本机 root 密码。默认锁定 root。启用后可用 root 在串口执行 `poweroff`。关机钩子先允许 Core 结算，随后关闭其余会话与进程，刷盘并卸载状态和主目录。运行时生成的所有者令牌位于服务账户的私有见证目录。[关机验收](../tests/native_disk.py)

每次开机在 `/tmp` 创建独立的 tmpfs，最多使用 32 MiB 数据空间和 4096 个 inode。普通账户可以创建临时文件，目录的 sticky bit 保护其他账户的条目，挂载启用 `nodev` 与 `nosuid`。达到任一上限时，内核拒绝继续分配。重启后临时文件消失。可将需要保留的用户文件放在持久主目录。

## Core 资源限额

生成新盘时，可同时传入以下三个参数，让内核强制限制守护进程及其子进程。

```sh
--core-cpu-millicores 500 --core-memory-mib 128 --core-pids-max 48
```

此配置给 `psiosd` 分配一个专用 cgroup v2。500 millicores 对应每 100 毫秒最多 50 毫秒的 CPU 时间。内核执行内存硬上限、零 swap 和线程数上限。PID 1、负责重启服务的进程及串口会话位于该 cgroup 之外。只有 root 可以修改控制值。启用此配置后，启动会先检查控制器与写入后的配置，缺少控制器时在挂载持久分区前停机。Core 遭遇 OOM 或退出后，监督进程清理上一实例残留的进程，复核内核限额并重新启动服务。[Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)

构建回执格式 6 与启动清单记录 `core_limits`。停机升级沿用可信回执中的限额及原节点身份。来宾可以经独立授权运行 `node.pressure`，核对同一 cgroup 的限额、节流、OOM 和 PID 事件。[资源压力接口](core-runtime.md#linux-资源压力与守护进程-cgroup) · [内核限额与升级验收](../tests/native_core_limits.py)

运行中的 Core 遇到内核拒绝分配连接线程或文件描述符时，关闭受影响的连接，原工作线程继续处理已准入的任务。容量释放后，同一守护进程恢复接收。启动阶段先创建全部必要线程，再开放任务派发。线程容量不足会返回可诊断错误，串口仍可用于维护。[文件描述符耗尽验收](../tests/test_server_capacity.py)

## 可选有线接入

新盘可用 `--dhcp-mac 52:54:00:51:6f:20` 启用单个 virtio 网口的 IPv4 DHCP。为虚机配置同一个 MAC。下面的 QEMU 参数可用于只验证租约的隔离网络。

```sh
-netdev user,id=lan,restrict=on \
-device virtio-net-pci,netdev=lan,mac=52:54:00:51:6f:20,romfile=
```

该配置管理匹配网口的 IPv4 地址、租约路由和 `/etc/resolv.conf`。收到选项 121 时，最多安装 32 个不同前缀，支持直连、经网关和默认路由。同一租约中的选项 3 仅在缺少选项 121 时提供默认网关。续租与释放按已记录的前缀、网关、接口、协议和 metric 逐条替换或删除租约路由。应用租约失败时清理已配置部分并报告失败。

最多三个 DNS 服务器按租约配置。普通账户可用 `busybox ip -4 addr show`、`busybox ip -4 route show` 和 `cat /etc/resolv.conf` 检查。最新租约或故障信息在 `/run/psios-network/status`。配置的网口缺失时，串口账户与 Core 可照常使用。[RFC 3442](https://datatracker.ietf.org/doc/html/rfc3442) · [租约续租和释放验收](../tests/native_dhcp.py) · [无类别路由验收](../tests/native_dhcp_routes.py)

对于无 DHCP 的局域网，构建新盘时可使用以下参数绑定一个 virtio 网口。可省略网关和 DNS。网关必须是同子网的另一个主机地址。DNS 参数最多提供三次。

```sh
--static-mac 52:54:00:51:6f:20 --static-address 192.0.2.20/24 \
--static-gateway 192.0.2.2 --static-dns 192.0.2.53
```

静态 IPv6 可以独立启用，也可以与同一 MAC 的静态 IPv4 同时启用。IPv6 默认网关可用链路本地地址或同子网的另一个主机地址，由系统绑定到该网口。两种地址族合计最多三个 DNS 服务器，IPv4 服务器排在 IPv6 服务器之前。

```sh
--static-mac 52:54:00:51:6f:20 --static-ipv6-address 2001:db8:70::20/64 \
--static-ipv6-gateway fe80::2 --static-ipv6-dns 2001:db8:70::53
```

启动时读入 root 私有的 `/etc/psios/static-ipv4` 或 `/etc/psios/static-ipv6`，按 MAC 匹配唯一的 virtio 网口。静态 IPv6 网口关闭路由器通告、自动配置和重定向，等待 Linux 重复地址检测通过后才发布 IPv6 默认路由。静态路由使用 metric 30320。地址或路由配置失败时清理本次添加的地址和路由，保留已有条目与解析器配置。两种地址族都配置成功后一次性更新双栈 DNS。可查看 `/run/psios-network/status` 和 `busybox ip -6 route show`。多网口、无线、DHCPv6、SLAAC 和远程确认回退事务仍待开发。[静态 IPv4 验收](../tests/native_static_ipv4.py) · [静态 IPv6 和双栈验收](../tests/native_static_ipv6.py)

静态 IPv4 的网络配置对应格式 4 回执，静态 IPv6 和双栈对应格式 5。配有 Core 资源限额的盘在格式 6 中记录同一个 `network_profile`。停机升级会检查保存的配置内容和权限。双栈还会核对两个配置的 MAC 和合计 DNS 数量。配置损坏时保留源盘并拒绝发布候选盘。

## 核对安装与运行的制品

构建成功后，将 `node-build.json` 保存在节点外。其中 `initial_image_sha256` 可在首次启动前与 `sha256sum node.raw` 比对。启动会改变数据分区，离线核对引导程序可单独读取 EFI 系统分区。

```sh
mcopy -i node.raw@@1048576 ::/EFI/BOOT/BOOTX64.EFI boot.efi
sha256sum boot.efi
```

将结果与回执的 `efi_application_sha256` 比对。启动后普通账户可运行 `sha256sum /psios-deployment.json` 与回执的 `deployment_manifest_sha256` 比对。该清单列出 Core 源码修订及归档摘要、内核 ISO 及其内核与初始内存盘的输入摘要、两个文件系统 UUID 和 PsiOS 加入启动环境的文件摘要。回执另含组装后的完整 initramfs 摘要。PID 1 在挂载可写分区前核对清单和这些文件，发现不一致时给出 `deployment-payload` 停机阶段。外部回执供操作者比对。

## 安装到空白块设备

在 Linux amd64 上用本机 root 运行归档中的 `native/install.py`。准备尚未启动的新盘镜像、构建回执以及来自可信构建的回执 SHA-256。目标应为容量与镜像完全相同的空白 Virtio 整盘，具备唯一序列号和 512 字节逻辑扇区。安装记录目录须位于另一个受保护的本机持久文件系统，支持 ext4、XFS 和 Btrfs。由 root 所有，权限为 `0700`。安装器会检查并同步上级目录。

```sh
sudo install -d -m 0700 "$INSTALL_STATE"
sudo python3 native/install.py plan --source-image node.raw --receipt node-build.json \
  --receipt-sha256 "$VERIFIED_RECEIPT_SHA256" --target "$TARGET_DISK" --state-dir "$INSTALL_STATE"
sudo python3 native/install.py apply --plan-digest "$APPROVED_PLAN_SHA256" --state-dir "$INSTALL_STATE"
sudo python3 native/install.py inspect --state-dir "$INSTALL_STATE"
```

操作者从 Plan 核对目标序列号、设备代次、分区和来源摘要，再将该份计划的 `plan_digest` 用于批准。执行会排他打开同一整盘，重新核对身份和空白字节，在首次写入前刷盘保存 `WriteIssued`。各分区内容刷盘后才写入两份 GPT。设备完成刷盘并全盘回读匹配后，回执进入 `Verified`。成功回执可重复读取。执行中断后，同次宿主启动内可用 `inspect` 对原目标核实完全写入的结果。部分写入或目标身份变化保持 `RecoveryRequired`，原计划停止派发。开发验收可通过 `--allow-loop` 选用专用 loop 测试盘。[Linux 块设备排他打开](https://man7.org/linux/man-pages/man2/open.2.html)

## 使用已登记证书构建 Secure Boot 盘

构建机需要 `ukify`、`sbsign`、`sbverify` 和 OpenSSL。提供来自可信工具包的 `linuxx64.efi.stub` 及摘要，并使用已登记到目标 UEFI 信任数据库的签名证书和构建者私有 PEM 密钥。将以下参数附加到新盘或升级命令。

```sh
--secure-boot-key signing.key --secure-boot-cert signing.pem \
--efi-stub /usr/lib/systemd/boot/efi/linuxx64.efi.stub --efi-stub-sha256 "$VERIFIED_STUB_SHA256"
```

构建器将内核、initramfs、内置命令行和部署清单组装进同一个 UKI，并在发布前核验签名。`secure_boot` 回执字段记录证书 DER 和 EFI stub 的 SHA-256，可用 `sbverify --cert signing.pem boot.efi` 检查 EFI 制品。升级已签名的开发盘须提供相同证书，并同时核对旧 EFI 摘要和签名。升级保留原盘和节点身份。[systemd-stub 255](https://www.freedesktop.org/software/systemd/man/255/systemd-stub.html) · [Secure Boot 和升级验收](../tests/native_secure_boot.py)

当前验收环境为使用自建登记证书的 amd64 OVMF。需要在相同固件信任配置下核验时，可使用签名的独立只读救援光盘。真实硬件固件、证书轮替和 TPM 解锁仍需对应的产品验收。[签名救援](offline-rescue.md#在-secure-boot-下核验)

## 持久性与救援

每次启动在挂载前核对状态和主目录的文件系统 UUID 及分区布局，要求匹配的两个分区在同一块 virtio 磁盘上。额外的 virtio 数据盘可随系统一起连接。发现部署 UUID 重复或匹配分区分属两块磁盘时，启动在挂载前停止。[双盘验收](../tests/native_storage.py)

首次启动核对部署记录，在私有暂存目录创建并核验 Core 状态与见证，再逐个发布两个目录。在准备期间断电，可重新准备未发布的权威。发布开始后断电，启动会核验并继续安装原节点的状态与见证。初始化完成标记持久落盘后才开启本机管理。已有节点丢失状态时，启动检查会报错并保留原见证供救援。启动检查失败时，串口会指明失败阶段，系统关闭已经挂载的持久分区后停机。保留原盘，使用同版本救援归档检查 `--partition 2 --state /psios --uid 1000`。强制断电产生的待重放日志可用有容量限制的临时覆盖层检查。[首次启动断电验收](../tests/native_enrollment.py) · [离线救援](offline-rescue.md)

本版生成的 323 MiB 新盘给主目录分配独立的 64 MiB ext4。普通账户可以在 `/home/psios` 保存文件并执行脚本，root 的 `/root` 独立设为私有，两者共享这 64 MiB。主目录写满时仍可向 Core 提交任务。第三分区丢失或 UUID 不符时，启动在挂载前停止。早期两分区开发盘的主目录在 RAM，已有磁盘继续使用它们原有的引导程序。

此开发盘使用 virtio 存储、4 MiB amd64 OVMF 和本机串口。本机账户及密码摘要保存在状态分区的 /etc 配置中。root 可运行 passwd psios 更换密码，重启后生效。磁盘数据为明文。状态与见证位于同一物理故障域。新节点应重新生成磁盘。物理安装、加密、自动系统更新与回退，以及三个发行版的 EditionReady 验收尚待实现或验证。[虚机故障验收](../tests/native_disk.py)

## 停机升级到另一份原生磁盘

适用于已初始化且停机的 323 MiB 三分区开发盘，使用标准 `/var/lib/psios` 和 `/var/lib/psios.witness`。保留当前可用的 EFI 变量文件和节点外可信构建回执。使用目标版本的已核验 Core 归档及安装器。构建机还需 `qemu-img`、`qemu-system-x86_64` 和约 1 GiB 可用工作空间。

```sh
umask 077
python3 native/create.py next.raw --upgrade-from node.raw --previous-receipt node-build.json \
  --kernel-iso alpine-virt.iso --core-archive psios-core-linux-amd64.tar.gz \
  --core-sha256 "$VERIFIED_CORE_SHA256" > next-build.json
```

安装器用 QEMU 的一致性读锁复制源盘，检查原 GPT 布局、旧 EFI 摘要、文件系统 UUID 和部署标记。待重放的 ext4 内部日志在固定大小的私有分区副本上重放，随后执行只读文件系统检查。新版本 Core 由独立救援内核在只读候选盘上核验权威及见证。通过后替换候选盘的 EFI 分区，保留原 GPT、状态、卷、凭据、站点配置和主目录。回执记录源盘摘要、目标制品摘要、升级前的数据库格式及已重放日志的分区。活动的 QEMU 写入、身份不匹配或需要进一步修复时停止发布。创建完成后可核对 `initial_image_sha256`。[QEMU 文件锁](https://www.qemu.org/docs/master/system/qemu-block-drivers.html#disk-image-file-locking) · [ext4 日志重放](https://man7.org/linux/man-pages/man8/e2fsck.8.html) · [救援核验](offline-rescue.md)

发布前，救援环境用目标版本的 Core 和保留的配置，在容量受限的内存文件系统中演练实际数据库迁移及见证结算。副本通过完整性、外键、制品、凭据、同一节点身份及目标格式检查后，`upgrade.verified_authority.migration_rehearsal` 记录来源格式、目标格式、原配置字节的 SHA-256 和核验结果。迁移演练失败会阻止候选盘发布。演练只打开私有副本，不启动守护进程、调度任务或运行外部效果。[停机升级与失败验收](../tests/native_upgrade.py)

回执版本 2 的 `geometry_mib` 用来逐项核对原 GPT 与镜像总长。升级盘沿用核对后的容量和分区位置。回执版本 1 按原始 323 MiB 布局检查。可在来宾用 `df -h /var/lib /home/psios` 查看两个文件系统的可用空间。[独立容量和断电升级验收](../tests/native_capacity.py)

### 隔离试启动

如需核对目标 EFI 制品实际引导和迁移，可从同一停机原盘构建独立的试启动镜像。构建和试启动期间保持原盘停机，并卸载通过 loop 等方式挂载的原盘分区。将试启动镜像作为只读后备文件，在一次性 qcow2 覆盖层和原 EFI 变量文件的私有副本中运行。为这次 VM 启动禁用网卡。

```sh
umask 077
python3 native/create.py trial.raw --trial-boot --upgrade-from node.raw \
  --previous-receipt node-build.json --kernel-iso alpine-virt.iso \
  --core-archive psios-core-linux-amd64.tar.gz --core-sha256 "$VERIFIED_CORE_SHA256" \
  > trial-build.json
qemu-img create -f qcow2 -F raw -b "$(realpath trial.raw)" trial.qcow2
cp node-vars.fd trial-vars.fd
qemu-system-x86_64 -nodefaults -no-user-config -machine q35 -m 768 -smp 1 \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,unit=1,file=trial-vars.fd \
  -drive if=virtio,format=qcow2,file=trial.qcow2 -boot order=c,strict=on \
  -display none -serial stdio -monitor none -nic none
```

试启动回执使用格式 7，`boot_mode` 为 `IsolatedTrial`，`trial_disk_uuid` 记录试启动盘的新 GPT 磁盘 UUID。引导模式和磁盘 UUID 随 initramfs 发布。系统在挂载可写状态前核对 GPT UUID，试启动 EFI 即使被复制到原盘也会拒绝打开原盘状态。使用 Secure Boot 时，沿用原证书和登记配置，将相同签名参数传给构建器，并使用相应固件代码文件。试启动核对持久 Core 配置，在覆盖层由目标版本的 `psiosd check` 执行真实迁移。检查超时为 120 秒，另有 5 秒终止宽限。持久服务配置、Core API、调度和来宾网络保持停用，迁移后卸载文件系统并自动关机。

串口输出一条以 `PSIOS-TRIAL/1 ` 开头的 JSON 状态。验收时要求 `status` 为 `Ready`，其中 `deployment_manifest_sha256` 须与 `trial-build.json` 相符，并确认虚机已正常退出。失败状态包含启动阶段。中断或缺少成功状态时可丢弃覆盖层及试启动 EFI 变量，原盘和原变量文件仍供独立引导。[真实 UEFI 试启动验收](../tests/native_upgrade.py)

### 试启动后发布停机更新候选

`native/stage_update.py` 适用于 Linux amd64 上的普通 OVMF 原生盘。先停止原来的来宾并卸载原盘。准备核验过的原盘构建回执、原 EFI 变量和目标 Core 与内核输入。输出路径应是操作者私有目录中尚未存在的名称。

```sh
python3 native/stage_update.py next-release \
  --upgrade-from node.raw --previous-receipt node-build.json \
  --previous-receipt-sha256 "$VERIFIED_PREVIOUS_RECEIPT_SHA256" \
  --kernel-iso "$KERNEL_ISO" --core-archive "$TARGET_CORE_ARCHIVE" --core-sha256 "$TARGET_CORE_SHA256" \
  --ovmf-code "$OVMF_CODE" --ovmf-code-sha256 "$VERIFIED_OVMF_CODE_SHA256" \
  --ovmf-vars node-vars.fd --ovmf-vars-sha256 "$VERIFIED_NODE_VARS_SHA256"
```

控制器采集一次原盘，在私有副本上处理必要的 ext4 日志，再基于同一份快照生成两份盘。隔离试启动在禁用来宾网络的 QEMU、私有 EFI 变量和限额写时复制层上运行。控制器核对 `Ready`、部署摘要和正常关机，再用目标 Core 离线核对同一节点的迁移结果与见证。发布前重新检查原盘与 EFI 变量。保持原盘停机，直至控制器退出。

成功后，`next-release` 同时出现 `candidate.raw`、`candidate-build.json` 和 `staged-update.json`。回执记录原始与规范化源摘要、试启动证明及正式候选摘要，状态为 `Staged`，回退方式为 `ManualRecoveryOnly`。正式启动、激活和回退由操作者单独执行。签名盘的自动分阶段发布与自动引导接管仍待开发。[分阶段更新验收](../tests/native_staged_update.py)

### 启动正式候选盘

从保留的停机原盘独立构建正式候选，按本节开头的命令省略 `--trial-boot`。可比对两份回执中的原盘摘要、目标 Core 源码版本及迁移演练结果。使用新的 EFI 变量副本启动 `next.raw`，启动检查会再次执行真实迁移。登录后比对 `/psios-deployment.json` 摘要、节点 ID 和已有任务回执，再恢复工作流。原盘与候选盘共享节点身份和密钥，只启动其中一个。候选盘开始接受业务写入后，回退前需要另行处理候选盘内的新状态与外部效果。[双版本启动与保留数据测试](../tests/native_upgrade.py)
