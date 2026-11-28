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

制作新盘时可用 `--hostname psi-edge-17` 设置本机名。生产引导在启动服务前从 root 所有的 `/etc/hostname` 读取一个 DNS 标签，缺省名称为 `psios`。root 可更新该文件，并在重启后应用。停机升级沿用源盘配置。无效文件会在 `host-name` 阶段进入救援流程。

第一次生产启动生成标准的 `/etc/machine-id`，由系统分区中的 root 只读文件提供。每台从同一未启动盘部署的节点在各自启动时生成 ID。之后重启沿用原值，停机升级保留它。早期镜像首次运行新引导时会补建文件。已存在的 ID 必须是非全零的 32 个小写十六进制字符。启动遇到格式、类型或所有者不符时，记录 `machine-identity` 阶段并进入原有救援流程。

正常部署挂载并核验状态卷以后，如早期启动检查失败，会在 `/var/lib/.psios-boot-failure` 保存最近一次失败的启动 ID 和阶段。文件为 root 专用，修复后成功启动仍保留记录。停机时，持有可信磁盘副本的操作者也可只读查看状态分区根目录下的 `/.psios-boot-failure`。状态卷核验前的故障和隔离试启动故障由串口报告。

生成新盘时可用 `--timezone Asia/Shanghai` 选择本地显示时区，默认 UTC。普通账户可用 `psios-timezone show` 查询，或用 `psios-timezone list` 查看随盘提供的 IANA 名称。在本机 root 控制台执行 `psios-timezone set Europe/Berlin` 会原子更新 `/etc/localtime`，重启和停机升级保留该设置。系统 UTC 时钟由原有内核与固件管理。需要 UTC 输出时可用 `date -u`。

当前镜像从固定 Linux 输入取用公有领域的 IANA tzdata 2026f。启动清单记录数据包摘要，启动时校验随盘的时区文件及管理命令。停机升级以新部署内置数据解释已保留的 IANA 名称。[断网与停机升级验收](../tests/native_timezone.py)

生产模式在 `/dev/pts` 挂载独立的 Linux devpts 实例。普通账户可通过 `/dev/ptmx` 为本机终端程序分配伪终端，每个从设备仅向创建者开放读写（0600）。同一挂载最多同时分配 64 对。耗尽时已有终端仍可用，关闭一对后可继续分配。重新开机后分配从空实例开始。[真实终端验收](../tests/native_pty.c)

生产模式为 POSIX 共享内存在 `/dev/shm` 单独挂载 tmpfs。程序可以用 `shm_open` 打开命名对象，通过 `mmap(MAP_SHARED)` 交换数据。`shm_unlink` 移除名称后，已取得的映射仍可用。此挂载上限为 16 MiB 和 1024 个 inode，与 `/tmp` 各自计量。根目录为 1777，启用 `nodev,nosuid,noexec`。需要私有数据的程序应创建 0600 对象。对象在重启后消失。[跨进程与容量验收](../tests/native_shm.c)

生产模式在 `/run` 单独挂载 8 MiB、1024 个 inode 的 tmpfs，存放 Core 的本机套接字和临时网络状态。顶层目录由 root 管理，Core 的目录保持 0700。挂载启用 `nodev,nosuid,noexec`，与应用共享内存和普通临时文件分别计量。开机重新建立套接字，持久的状态、见证和主目录仍从各自文件系统读取。

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

生产引导在持久配置缺少 `/etc/hosts` 时创建 IPv4 和 IPv6 的 `localhost` 回环记录，断网时同样可解析。管理员维护的现有内容会沿用。文件必须为 root 所有的普通文件，权限为 0444 或 0644，且只有一个硬链接。无效文件在 `host-resolution` 阶段触发救援。[原生联网与回环验收](../tests/native_combined_core.py)

新盘可用 `--dhcp-mac 52:54:00:51:6f:20` 启用单个 virtio 网口的 IPv4 DHCP。为虚机配置同一个 MAC。下面的 QEMU 参数可用于只验证租约的隔离网络。

```sh
-netdev user,id=lan,restrict=on \
-device virtio-net-pci,netdev=lan,mac=52:54:00:51:6f:20,romfile=
```

用于 Intel e1000e 网口时，另加 `--dhcp-driver e1000e`。构建机需要 `unsquashfs`，安装器从已核验的固定内核介质提取并核对模块，将它和其他引导内容一起纳入启动校验。该 IPv4 DHCP 配置仍按单一 MAC 绑定。格式 8 的构建回执固定驱动，停机升级对试启动及正式候选沿用原配置。QEMU 用 `-device e1000e,netdev=lan,mac=52:54:00:51:6f:20,romfile=`，已验收登记证书的 OVMF Secure Boot 联网。实际网卡与固件待验收。[有线 DHCP 验收](../tests/native_dhcp.py)

该配置管理匹配网口的 IPv4 地址、租约路由和 `/etc/resolv.conf`。收到选项 121 时，最多安装 32 个不同前缀，支持直连、经网关和默认路由。同一租约中的选项 3 仅在缺少选项 121 时提供默认网关。续租与释放按已记录的前缀、网关、接口、协议和 metric 逐条替换或删除租约路由。应用租约失败时清理已配置部分并报告失败。

最多三个 DNS 服务器按租约配置。普通账户可用 `busybox ip -4 addr show`、`busybox ip -4 route show` 和 `cat /etc/resolv.conf` 检查。最新租约或故障信息在 `/run/psios-network/status`。配置的网口缺失时，串口账户与 Core 可照常使用。[RFC 3442](https://datatracker.ietf.org/doc/html/rfc3442) · [租约续租和释放验收](../tests/native_dhcp.py) · [无类别路由验收](../tests/native_dhcp_routes.py)

对于无 DHCP 的局域网，构建新盘时可使用以下参数绑定一个有线网口。默认选择 virtio，添加 `--static-driver e1000e` 使用 Intel e1000e，需要构建工具 `unsquashfs`。可省略网关和 DNS。IPv4 网关必须是同子网的另一个主机地址。DNS 参数最多提供三次。

```sh
--static-mac 52:54:00:51:6f:20 --static-address 192.0.2.20/24 \
--static-gateway 192.0.2.2 --static-dns 192.0.2.53
```

静态 IPv6 可以独立启用，也可以与同一 MAC 的静态 IPv4 同时启用。IPv6 默认网关可用链路本地地址或同子网的另一个主机地址，由系统绑定到该网口。两种地址族合计最多三个 DNS 服务器，IPv4 服务器排在 IPv6 服务器之前。

```sh
--static-mac 52:54:00:51:6f:20 --static-ipv6-address 2001:db8:70::20/64 \
--static-ipv6-gateway fe80::2 --static-ipv6-dns 2001:db8:70::53
```

启动时读入 root 私有的 `/etc/psios/static-ipv4` 或 `/etc/psios/static-ipv6`，按 MAC 和指定驱动匹配唯一网口。静态 IPv6 网口关闭路由器通告、自动配置和重定向，等待 Linux 重复地址检测通过后才发布 IPv6 默认路由。静态路由使用 metric 30320。地址或路由配置失败时清理本次添加的地址和路由，保留已有条目与解析器配置。两种地址族都配置成功后一次性更新双栈 DNS。可查看 `/run/psios-network/status` 和 `busybox ip -6 route show`。多网口、无线、DHCPv6 和远程确认回退事务仍待开发。[静态 IPv4 验收](../tests/native_static_ipv4.py) · [静态 IPv6 和双栈验收](../tests/native_static_ipv6.py) · [e1000e 双栈验收](../tests/native_static_e1000e.py)

virtio 静态 IPv4 对应格式 4 回执，静态 IPv6 和双栈对应格式 5。配有 Core 资源限额的 virtio 盘在格式 6 中记录同一个 `network_profile`。e1000e 的格式 8 回执同时记录驱动和静态模式。停机升级会检查保存的配置内容和权限，并核对 e1000e 的启动方式。双栈还会核对两个配置的 MAC 和合计 DNS 数量。配置损坏时保留源盘并拒绝发布候选盘。

新盘可用 `--slaac-mac 52:54:00:51:6f:20` 为指定 Virtio 网口启用 IPv6 路由器发现。Linux 接收路由器通告，按通告形成 IPv6 地址、执行重复地址检测并管理地址与默认路由的有效期。此配置关闭 IPv6 重定向。可用 `--slaac-dns` 指定至多三个不带区标识的 IPv6 DNS 地址。`/run/psios-network/status` 显示发现功能是否启动。地址与路由可用 `busybox ip -6 addr` 和 `busybox ip -6 route` 核对。

SLAAC 使用格式 10 的构建回执。工厂镜像核验和隔离试启动更新都检查 `/etc/psios/slaac` 及服务定义。正常更新保留 MAC 与显式 DNS。该模式使用 IPv6 单栈。DHCPv6、其他网卡驱动和物理硬件的 SLAAC 验证仍待开展。[真实 IPv6 发现验收](../tests/native_slaac.py)

连接允许信任路由器 DNS 通告的网络时，可在相同 MAC 配置上选择 `--slaac-rdnss`。它与 `--slaac-dns` 互斥。镜像从已固定的内核介质提取 dhcpcd 10.5.2，在所选 Virtio 网口运行 IPv6 路由器发现。解析器只接收有有效期的 RDNSS 地址，去重并发布前三个，为链路本地地址记录所选网口的作用域。到期、链路断开和重启都会重新核对正在使用的解析器。停机升级通过格式 11 回执核对配置、服务和专用账户。获得当前 Node 授权的 Agent 可以用 `node.resolver` 核对实时 DNS 服务器、接口作用域和启动身份。隔离来宾已验证到期后默认路由仍有效，升级后可以读取原任务制品并取得新启动的观察。许可副本随镜像提供。[dhcpcd 许可](../images/native/dhcpcd.LICENSE) · [IPv6 TAP 验收](../tests/native_rdnss.py)

## 可选 SSH 控制台

制作新盘时可在已有的 DHCP、静态网络或 SLAAC 配置上加入 `--ssh-authorized-key operator.pub`。文件使用标准 OpenSSH Ed25519 公钥格式。控制台在 TCP 22 监听，只允许 `psios` 用户用该公钥登录。root、密码认证和转发均关闭。SLAAC 的格式 10 回执可同时记录 SSH 配置。串口账户仍使用各自的本机配置。

首次正式引导生成并刷盘保存本部署的 Ed25519 主机密钥。可通过可信串口运行 `ssh-keygen -lf /run/psios-ssh/host-key.pub` 核对首次连接显示的指纹。复制尚未启动的新盘时，每份副本在自己的首次正式引导生成主机密钥。重启和停机升级保留原主机密钥与授权公钥。隔离试启动不运行 SSH。查看 SSH 启动错误可使用本地串口。[真实 SSH 验收](../tests/native_ssh_console.py)

## 核对安装与运行的制品

构建成功后，将 `node-build.json` 保存在节点外。其中 `initial_image_sha256` 可在首次启动前与 `sha256sum node.raw` 比对。启动会改变数据分区，离线核对引导程序可单独读取 EFI 系统分区。

```sh
mcopy -i node.raw@@1048576 ::/EFI/BOOT/BOOTX64.EFI boot.efi
sha256sum boot.efi
```

构建器保留固定内核的原始 initramfs，将 Core 程序、配置和启动脚本作为独立的 gzip 压缩 cpio 成员追加。文件名、权限和部署清单中的单文件摘要仍按解压后的内容核验。停机升级可从早期未压缩的开发盘生成压缩的候选盘，沿用原 EFI 分区布局。

构建器固定追加成员的修改时间。使用相同 Core、内核介质、文件系统 UUID、部署配置和工具版本，在不同目录及初始 umask 下可比对部署清单、完整 initramfs 和未签名 EFI 的 SHA-256。为每次新安装生成的磁盘身份随该次回执记录。[重复构建验收](../tests/native_boot_reproducibility.py)

将结果与回执的 `efi_application_sha256` 比对。启动后普通账户可运行 `sha256sum /psios-deployment.json` 与回执的 `deployment_manifest_sha256` 比对。该清单列出 Core 源码修订及归档摘要、内核 ISO 及其内核与初始内存盘的输入摘要、两个文件系统 UUID 和 PsiOS 加入启动环境的文件摘要。回执另含组装后的完整 initramfs 摘要。PID 1 在挂载可写分区前核对清单和这些文件，发现不一致时给出 `deployment-payload` 停机阶段。外部回执供操作者比对。

## 安装到空白块设备

在 Linux amd64 上用本机 root 运行归档中的 `native/install.py`。准备尚未启动的新盘镜像、构建回执以及来自可信构建的回执 SHA-256。目标可为空白 Virtio 整盘、本地 PCIe NVMe 命名空间或固定 SATA AHCI 整盘，逻辑扇区为 512 字节。Virtio 使用唯一序列号。NVMe 计划还固定命名空间 ID 和 WWID。SATA AHCI 要求 ATA 序列号和注册的 NAA WWID，并检查同类已枚举磁盘的身份唯一性。三种设备的计划均固定内核设备代次与拓扑，并在写入前复核。安装记录目录须位于另一个受保护的本机持久文件系统，支持 ext4、XFS 和 Btrfs。由 root 所有，权限为 `0700`。安装器会检查并同步上级目录。[隔离来宾安装与重启验收](../tests/native_install_virtio.py)

```sh
sudo install -d -m 0700 "$INSTALL_STATE"
sudo python3 native/install.py plan --source-image node.raw --receipt node-build.json \
  --receipt-sha256 "$VERIFIED_RECEIPT_SHA256" --target "$TARGET_DISK" --state-dir "$INSTALL_STATE"
sudo python3 native/install.py apply --plan-digest "$APPROVED_PLAN_SHA256" --state-dir "$INSTALL_STATE"
sudo python3 native/install.py inspect --state-dir "$INSTALL_STATE"
```

安装器在操作记录目录所在的文件系统暂存工厂盘。复制时按完整逻辑内容核对固定的 SHA-256，全零的 1 MiB 区段可在临时普通文件中保留为空洞。记录目录还需要容纳分区检查用的暂存文件。[安装源校验](../images/native/install_source.py) · [安装验收](../tests/native_install.py)

默认按工厂镜像的精确容量安装。为较大的空白目标规划时可加 `--allow-larger-target`，上限为 2 TiB，要求 `sfdisk` 支持 `--relocate gpt-bak-std`。安装器保留分区大小、内容与身份，把备份 GPT 移到目标盘末尾，并将余量留为未分配空间。主表和备份表各自通过校验和核验后才可批准安装。批准内容包含 `deployment_image`，记录实际容量、备份表偏移和安装后整盘 SHA-256。空白检查与回读覆盖目标盘的全部字节。当前停机升级接受构建回执对应的 raw 盘布局，较大安装目标的升级入口仍待开发。[较大 NVMe 来宾验收](../tests/native_install_virtio.py)

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

同一原生 GPT 镜像可在 OVMF 的 virtio-blk、PCIe NVMe 和 SATA AHCI 磁盘上引导，验收使用 512 字节逻辑扇区。启动核对初始枚举中这些设备的文件系统 UUID 及分区布局。发现重复身份或状态与主目录分属两个盘时，在挂载前停机。实机固件和控制器仍待验收。[混合磁盘验收](../tests/native_storage.py)

首次启动核对部署记录，在私有暂存目录创建并核验 Core 状态与见证，再逐个发布两个目录。在准备期间断电，可重新准备未发布的权威。发布开始后断电，启动会核验并继续安装原节点的状态与见证。初始化完成标记持久落盘后才开启本机管理。已有节点丢失状态时，启动检查会报错并保留原见证供救援。启动检查失败时，串口会指明失败阶段，系统关闭已经挂载的持久分区后停机。保留原盘，使用同版本救援归档检查 `--partition 2 --state /psios --uid 1000`。强制断电产生的待重放日志可用有容量限制的临时覆盖层检查。[首次启动断电验收](../tests/native_enrollment.py) · [离线救援](offline-rescue.md)

本版生成的 323 MiB 新盘给主目录分配独立的 64 MiB ext4。普通账户可以在 `/home/psios` 保存文件并执行脚本，root 的 `/root` 独立设为私有，两者共享这 64 MiB。主目录写满时仍可向 Core 提交任务。第三分区丢失或 UUID 不符时，启动在挂载前停止。早期两分区开发盘的主目录在 RAM，已有磁盘继续使用它们原有的引导程序。

此开发盘使用 4 MiB amd64 OVMF 和本机串口。本机账户及密码摘要保存在状态分区的 /etc 配置中。root 可运行 passwd psios 更换密码，重启后生效。磁盘数据为明文。状态与见证位于同一物理故障域。新节点应重新生成磁盘。物理安装、加密、自动系统更新与回退，以及三个发行版的 EditionReady 验收尚待实现或验证。[虚机故障验收](../tests/native_disk.py)

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

`native/stage_update.py` 适用于 Linux amd64 上的 OVMF 原生盘。先停止原来的来宾并卸载原盘。准备核验过的原盘构建回执、原 EFI 变量和目标 Core 与内核输入。输出路径应是操作者私有目录中尚未存在的名称。

```sh
python3 native/stage_update.py next-release \
  --upgrade-from node.raw --previous-receipt node-build.json \
  --previous-receipt-sha256 "$VERIFIED_PREVIOUS_RECEIPT_SHA256" \
  --kernel-iso "$KERNEL_ISO" --core-archive "$TARGET_CORE_ARCHIVE" --core-sha256 "$TARGET_CORE_SHA256" \
  --ovmf-code "$OVMF_CODE" --ovmf-code-sha256 "$VERIFIED_OVMF_CODE_SHA256" \
  --ovmf-vars node-vars.fd --ovmf-vars-sha256 "$VERIFIED_NODE_VARS_SHA256"
```

控制器采集一次原盘，在私有副本上处理必要的 ext4 日志，再基于同一份快照生成两份盘。隔离试启动在禁用来宾网络的 QEMU、私有 EFI 变量和限额写时复制层上运行。固件优先选择本次试启动盘，即使原 EFI 变量记录了不同的 PCI 路径。控制器核对 `Ready`、部署摘要和正常关机，再用目标 Core 离线核对同一节点的迁移结果与见证。发布前重新检查原盘与 EFI 变量。保持原盘停机，直至控制器退出。

数据库及其 WAL 和回滚日志合计使用 `--max-copy-mib` 控制核验复制量，范围 1 至 256 MiB。`create.py --upgrade-from` 默认 64 MiB。`stage_update.py` 默认 128 MiB，并在源核验与试启动后核验中执行同一上限。救援来宾为离线迁移预留额外临时空间。构建与分阶段回执均记录实际复制量和采用的上限，超限时原盘留在原位且候选未发布。[大库升级验收](../tests/native_staged_update.py)

签名原盘追加 `--secure-boot-key`、`--secure-boot-cert`、`--efi-stub` 和 `--efi-stub-sha256`，使用原登记证书及相应 Secure Boot 固件。两份 EFI 均使用同一证书签名。试启动在挂载可写状态前核对固件的 Secure Boot 执行状态，通过后才继续迁移。回执为试启动保存固件执行证据，为正式候选保存单独的签名与摘要。

成功后，`next-release` 同时出现 `candidate.raw`、`candidate-build.json` 和 `staged-update.json`。回执记录原始与规范化源摘要、试启动证明及正式候选摘要，状态为 `Staged`，回退方式为 `ManualRecoveryOnly`。正式启动、激活和回退由操作者单独执行。自动引导接管仍待开发。[分阶段更新验收](../tests/native_staged_update.py)

### 核验受管候选盘的首次正式引导

对 OVMF 部署，可用 `native/activate_update.py` 核验正式候选盘的首次启动。以普通 Linux amd64 操作者运行，为节点准备一个由该操作者所有的 `0700` 持久目录。来宾须已配置本机 root 控制台。命令从操作者持有的私有文件读取此次登录凭据。

```sh
install -d -m 0700 "$ACTIVATION_STATE"
python3 native/activate_update.py plan --state-dir "$ACTIVATION_STATE" \
  --stage-dir next-release --stage-receipt-sha256 "$VERIFIED_STAGE_SHA256" \
  --source-disk node.raw --ovmf-code "$OVMF_CODE" --ovmf-vars node-vars.fd
python3 native/activate_update.py activate --state-dir "$ACTIVATION_STATE" \
  --plan-digest "$APPROVED_ACTIVATION_SHA256" --root-console-password-file "$CONSOLE_PASSWORD_FILE"
python3 native/activate_update.py inspect --state-dir "$ACTIVATION_STATE"
```

计划固定分阶段回执、原盘、候选盘和固件，在状态目录准备私有引导材料。执行复核原盘身份与摘要，持久记录 `BootIssued`，再启动只连接候选盘副本的无网络来宾。通过控制台登录后，核对本次启动 ID、部署摘要及 Core 节点身份。来宾正常退出并刷盘后，控制器从加锁的停机磁盘再次核对 GPT 布局和分区标识，以及状态和主目录分区的 ext4 UUID、清洁标志和启用时的超级块校验和。确认无需日志重放且无待清理的孤立 inode 后，才写入列有受管磁盘和 EFI 变量的 `BootVerified` 回执。完整文件系统检查仍由离线救援执行。

准备材料期间中断后，可在同一目录重跑原来的 `plan`。若尚无准备记录，目录应为空。已有记录时，`inspect` 显示 `Preparing`，重试会重新核对原盘、分阶段回执、固件和已发布的副本，补齐缺失的材料。所有文件持久化后才发布 `BootPending`。计划摘要和批准对象在重试中保持一致。已进入 `BootIssued` 的启动继续遵循下述恢复流程。

签名候选须使用包含挂载前强制检查的分阶段构建。计划核对候选和试启动的证书、EFI stub 摘要与固件强制执行记录。正式候选在可写挂载前要求 Secure Boot。控制器通过来宾核对正式启动的固件状态，并把此次证据收入 `BootVerified` 回执。[签名首次引导验收](../tests/native_signed_activation.py)

控制器按小段向虚拟串口写入认证数据和证明命令，给来宾时间读取串口缓冲区。串口超时会报告登录、Core 核验或关机等阶段。超时消息记录对应的固定阶段名称。

有启动记录而缺少最终证明时，`inspect` 报告 `RecoveryRequired`，原计划停止派发。执行期间，控制器在操作者拥有的本地源映像上持有 Linux OFD 排他锁。支持遵循文件锁的 QEMU 进程，原盘在整个操作内保持停机。首次引导回执及其副本可供下一节使用。[首次引导验收](../tests/native_activation.py)

## 选择并启动已验证的受管部署

在同一节点使用一个独立的私有持久目录。用 `BootVerified` 的 `operation.json` 文件摘要生成选择计划，首次选择的预期代次为 0。

```sh
install -d -m 0700 "$selection_dir"
receipt_sha256=$(sha256sum "$activation_dir/operation.json" | cut -d ' ' -f1)
python3 -B native/select_update.py plan --state-dir "$selection_dir" \
  --activation-dir "$activation_dir" --activation-receipt-sha256 "$receipt_sha256" \
  --expected-generation 0
python3 -B native/select_update.py adopt --state-dir "$selection_dir" \
  --plan-digest "$plan_digest"
python3 -B native/select_update.py boot-selected --state-dir "$selection_dir" \
  --selection-digest "$selection_digest" --root-console-password-file "$console_password_file"
python3 -B native/select_update.py inspect --state-dir "$selection_dir"
```

从计划回执取得 `plan_digest`。提交会重新校验已验证候选及其来源，从加锁的停机盘核验状态和主目录分区，再将私有磁盘、固件代码与 EFI 变量作为同一代全部持久发布。原始分阶段候选、首次引导回执及映像继续保留。采用下一代时，先基于已选代次的当前停机磁盘、EFI 变量及该代构建回执生成新的分阶段候选并验证首次引导。选择命令复核新候选确实从当前受管状态派生，旧代次保留供运维核查。

每次启动从当前 `Selected` 回执取得 `selection_digest`。命令只附加该代的私有磁盘，把 `BootIssued` 持久化后通过受管 QEMU 启动，认证来宾 Core 的节点、部署摘要和新的 Linux 启动 ID，签名部署还需证明 Secure Boot 开启。来宾正常关机、刷盘并通过同样的停机分区核验后，回执给出新的选择摘要。后续启动必须显式批准新摘要。若缺少干净关机证据或启动结果不明，`inspect` 返回 `RecoveryRequired`，本目录停止再次启动或接管。

如果 `inspect` 提供 `boot_proof_sha256`，操作者可以核对并批准已有启动证据。

```sh
python3 -B native/select_update.py reconcile-boot --state-dir "$selection_dir" \
  --selection-digest "$issued_selection_digest" --boot-proof-sha256 "$boot_proof_sha256"
```

对账复核原启动意图、证明文件和同代磁盘及 EFI 摘要，并再次读取停机盘上的分区状态。核验通过只补交选择指针。证明缺失、过时、盘面不匹配或分区未干净关闭时仍保留待核实状态。

采用复制开始前登记一份精确计划对应的暂存目录。复制中断且新代目录尚未发布时，可用 `discard-staging --state-dir "$selection_dir" --plan-digest "$plan_digest"` 清理该计划的暂存文件。指向不明文件或目录的记录交由操作者核查。已发布的代次继续使用原批准的 `adopt` 恢复指针。

当前交付针对单节点受管 OVMF 的有限时引导和干净关机。操作者负责保持本节点的其他副本停机，外部 QEMU 须启用文件锁。物理固件切换、持续运行及自动回退属于后续开发范围。[普通验收](../tests/native_activation.py) · [Secure Boot 验收](../tests/native_signed_activation.py)

### 启动正式候选盘

从保留的停机原盘独立构建正式候选，按本节开头的命令省略 `--trial-boot`。可比对两份回执中的原盘摘要、目标 Core 源码版本及迁移演练结果。使用新的 EFI 变量副本启动 `next.raw`，启动检查会再次执行真实迁移。登录后比对 `/psios-deployment.json` 摘要、节点 ID 和已有任务回执，再恢复工作流。原盘与候选盘共享节点身份和密钥，只启动其中一个。候选盘开始接受业务写入后，回退前需要另行处理候选盘内的新状态与外部效果。[双版本启动与保留数据测试](../tests/native_upgrade.py)
