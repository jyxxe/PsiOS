# 从外部核验无法启动的节点

`images/rescue/inspect.py` 在独立的 amd64 QEMU 虚拟机中运行 `psiosd verify`。它适用于已停止的节点或一致性快照，输入为普通文件形式的 raw ext4 文件系统，或含有 ext4 分区的 raw 磁盘。操作员需要宿主机上读取镜像的权限、可信的 Core 和原服务账户的数字 UID。即使被检系统的内核、服务管理器和用户程序无法启动，仍可执行核验。

## 准备可信的工具

Linux 宿主需要 Python 3.11+、GNU cpio、`isoinfo` 和 `qemu-system-x86_64`。临时日志重放还需要 `qemu-img`。新版独立 Core 归档包含救援启动器、初始化程序、介质清单及离线用法。救援内核与初始内存文件系统来自固定的 Alpine virt 3.23.8 测试介质，读取前先核验整个 ISO，提取后再校验这两个文件。介质摘要及上游发布签名者指纹记在仓库中。[固定输入](../images/rescue/base.json) · [上游发布目录](https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/)

先取得所选主线提交的独立 Core 归档和构建证明。下面的 `EXPECTED_MAIN_SHA` 取自可信主线记录。`trusted_root.jsonl` 是事先通过可信连接取得的信任根。核验后的归档提供救援程序及其预期摘要。[取得和核验独立 Core](core-local.md#portable-amd64-core)

```bash
tools/verify-portable-core psios-core-linux-amd64.tar.gz "$EXPECTED_MAIN_SHA" provenance.jsonl trusted_root.jsonl
mkdir rescue-core
tar -xzf psios-core-linux-amd64.tar.gz -C rescue-core
(cd rescue-core && sha256sum --strict --check SHA256SUMS)
core_sha=$(awk '$2=="psiosd" {print $1}' rescue-core/SHA256SUMS)
```

所有输入备妥后，检查机可以断网运行，无需 Git、源码检出或 Cargo。只有二进制的早期归档仍可按对应可信源码中的说明运行启动器。

## 保存权威检查点并修复制品

停止 `psiosd` 后，以服务账户执行 `checkpoint`。命令在私有暂存目录中重放 SQLite 崩溃日志，核验数据库、见证、所有者凭据和分块制品，然后刷盘并原子发布目录。清单记录节点、恢复代次、数据库格式、提交头以及实际封存的数据库和日志文件。将回执中的 `manifest_digest` 存入与检查点独立的可信记录。在受保护的位置保管检查点，其中包含 Task 结果、审计和数据库中的密文。卷数据、见证文件和密钥材料需独立保管。

```bash
mkdir -m 700 checkpoints
psiosd --config /etc/psios/core.json checkpoint --output checkpoints/authority
```

脱机核验只需要检查点、独立记录中的摘要和可信 `psiosd`。使用原 Task 的制品 `ResourceRef` 可取回复核过摘要的原始字节。两条命令都在临时副本中读取 SQLite，`--scratch-dir` 可指定暂存位置，`--max-copy-mib` 限制复制量，提取还受 `--max-bytes` 限制。

```bash
psiosd verify-checkpoint --directory checkpoints/authority \
  --manifest-digest "$MANIFEST_DIGEST"
umask 077
psiosd checkpoint-artifact --directory checkpoints/authority \
  --manifest-digest "$MANIFEST_DIGEST" --reference-file artifact-ref.json > artifact.json
psiosctl --config /etc/psios/core.json artifact-repair artifact.json < artifact-ref.json
```

`artifact-repair` 由当前权威验证本机所有者身份、制品引用和分块摘要，然后修复缺失或损坏的内容。这样可以恢复原 Task 结果并保留检查点之后的授权、任务和操作历史。已记录为 `Unavailable` 的 Task 在修复后需要重新核验，再恢复受影响的会话或工作流。[崩溃检查点与在线修复验收](../tests/test_authority_checkpoint.py) · [任务结果核验](core-local.md)

## 停服后回收 SQLite 空闲页

任务结果回收可释放 SQLite 中供后续写入复用的页。停服后以原服务账户运行以下命令，可以将空闲页归还给文件系统。

```bash
umask 077
psiosd --config /etc/psios/core.json plan-state-compaction > state-compaction.json
psiosd --config /etc/psios/core.json compact-state --plan-file state-compaction.json
```

预检在私有副本上核对数据库、见证与所有者凭据，并记录源库身份、文件摘要和页用量。`free_pages` 用于估算可回收量。两个命令都可用 `--scratch-dir` 指定核验副本目录，用 `--max-copy-mib` 限制需要复制的状态量。

执行独占节点的状态锁，并重新核对原计划。SQLite 在本机原库完成压缩和 WAL 截断，再独立核对节点身份、提交头、数据库及相关制品。已有 Task、事件、授权与回执沿用原记录。容量检查保留权威存储预留，并为临时数据库及 WAL 计入额外空间。提交头变动、凭据轮换待处理或空间储备不足时，执行阶段会报告相应原因。最终回执报告压缩前后的文件大小。[SQLite VACUUM](https://www.sqlite.org/lang_vacuum.html) · [离线压缩验收](../tests/test_state_compaction.py)

## 离线检查旧版任务输入的引用

保存 1 至 16 个旧版 `Input` 或 `TaskInputManifest` 的完整引用 JSON 数组。停止守护进程后运行以下命令。

```sh
psiosd --config /etc/psios/core.json inspect-retained-inputs --references-file input-refs.json
```

命令固定并核对节点身份、提交头、连续性见证和原输入摘要，然后扫描私有副本的 SQLite 应用数据表。输出包含每项正文占用字节数及每张表中含该输入 ID 的行数。扫描包含历史资源及仍保存的接受记录，排除每项输入自己的那条版本记录。报告只计数并列出表名，原数据库和见证保持原样。

字面匹配可能包含正文中顺带提到输入 ID 的记录。SQLite 自带的元数据、编码或压缩后的引用、外部文件、其他节点和备份副本需要分别核查。复制或扫描超限会拒绝输出清单，可用 `--max-copy-mib` 和 `--max-scan-mib` 调整有界工作量。

## 验证和恢复独立的卷备份

原权威在线时，可授予操作者目标 Backup 上的 `backup.export`，再用当前 Backup 的 `ResourceRef` 导出经过核验的完整副本。导出命令接受 JSON 引用文件或标准输入，目标必须是私有父目录下的新目录。将导出回执中的 `manifest_digest` 保存到与副本独立的可信记录。手工转存既有副本时，也可以用 `psiosctl get BACKUP_ID` 读取同一摘要。[权限与分页](core-runtime.md#导出备份) · [在线导出与脱机恢复验收](../tests/test_backup_export.py)

```bash
mkdir -m 700 exports
psiosctl backup-export --output exports/backup-id backup-ref.json
```

传输可能中断时，可从第一次导出起加上 `--resume`。客户端在私有父目录下保存暂存数据，恢复时使用同一个输出名、同一固定版本的 Backup 引用和有效授权。接续传输从已有字节开始，完整校验前缀和新增内容，再原子发布目录。对已经发布的目标再次执行时，命令重新取得授权清单并核验现有副本。

恢复时的 `MANIFEST_DIGEST` 来自独立记录。使用独立归档中的 `psiosd`，在 Linux 上直接检查副本。

```bash
rescue-core/psiosd verify-backup --directory /media/backup/backup-id \
  --manifest-digest "$MANIFEST_DIGEST"
mkdir -m 700 recovery
rescue-core/psiosd verify-backup --directory /media/backup/backup-id \
  --manifest-digest "$MANIFEST_DIGEST" --restore-to recovery/data
```

工具核验清单摘要、成员集合、路径结构、各文件长度和完整内容。配置、数据库和见证可以留在离线的原节点。恢复前检查目标的空闲数据块与 inode，将数据写入私有暂存目录，通过核验并刷盘后以拒绝覆盖的原子改名发布。恢复保持文件和目录模式以及清单中记录的修改时间，所有权取自恢复账户。恢复结果是普通目录。[脱离权威的恢复验收](../tests/test_detached_backup.py)

使用数据库格式 112 对应的 Core 核验新备份，才能在恢复时应用清单所记载的根目录 `rwx` 权限位。旧备份缺少此字段时根目录采用 `0700`。旧版独立工具读取新备份时也会使用 `0700`，因此需要保留新版本工具以准确恢复权限。导入后恢复为托管卷时，卷数据根须允许服务账户读取和搜索。[权限恢复与兼容验收](../tests/test_backup_root_mode.py)

导出与脱机核验默认限制 256 MiB 负载。使用 `psiosctl backup-export --max-bytes BYTES` 和 `psiosd verify-backup --max-bytes BYTES` 可提高该次操作的上限。服务端按原清单流式核对已存备份，输出前检查容量与文件摘要。[大备份验收](../tests/test_backup_limits.py)

### 导入新节点并恢复到新卷

在已初始化的接收节点停止 `psiosd`，以该节点服务账户运行 `import-backup`。来源为 PsiOS 单卷备份的 `backup-export` 目录，摘要使用独立可信记录。导入核验原清单和所有数据块，将副本写入接收节点的私有备份存储，随后发布新节点可查询的 `Backup` 引用。原备份 ID、源卷引用和数据代次保留为来源信息。本地授予后续任务与导出权限。

服务配置中的 `backup_max_bytes` 适用于新建备份和导入，默认 268435456 字节，范围 1 字节至 1 TiB。`StoragePool.Get` 可查询当前配置。恢复已登记备份时使用原清单中的长度和摘要。

```bash
psiosd --config /etc/psios/core.json import-backup \
  --directory /media/backup/backup-id --manifest-digest "$MANIFEST_DIGEST"
```

启动服务后，使用回执里的 `reference` 提交 `backup.restore`。新卷有新的资源 ID 与数据代次。检查内容后提交并审批 `volume.activate`，激活前写入和应用挂载受限。导入记录在发布前绑定接收节点的目录身份。若目录已发布而登记中断，以同一摘要重试即可重新检查字节并完成登记。只有对应已提交导入记录的目录可通过此方式恢复。[跨节点恢复与登记故障验收](../tests/test_backup_intake.py)

#### 向指定节点签发备份副本

传输前通过独立可信渠道确认源和接收方的 `Node.Get` 资源 ID。接收方也需从独立渠道固定源操作者的 Ed25519 公钥，文件内容为 32 字节原始公钥。源服务停止后，以服务账户使用其独有的 Ed25519 PKCS#8 DER 私钥签发证明。私钥文件须由当前账户持有、权限为 `0600`、只存在一个硬链接。源端在私有 SQLite 副本中核对确定的提交见证、当前本地原生 Backup 引用和历史源卷，再验证原始清单与实际文件摘要。

```bash
psiosd --config /etc/psios/core.json attest-backup-transfer \
  --reference-file backup-ref.json --recipient-authority "$RECIPIENT_NODE_ID" \
  --signing-key-file source-private.der > transfer.json

psiosd --config /etc/psios/core.json import-backup \
  --directory /media/backup/backup-id --manifest-digest "$MANIFEST_DIGEST" \
  --attestation-file transfer.json --trusted-source-key trusted-source-public.raw \
  --expected-source-authority "$SOURCE_NODE_ID"
```

接收端核对目标节点、指定源节点、固定公钥、签名以及清单中原备份和数据代次，并验证每个数据文件后才发布。签名与公钥指纹随离线导入记录保存。登记中断后，提供完全相同的证明和信任参数重试，接收端会重新核对已经发布的确切目录。同一份导入记录始终绑定首次选择的途径和证明，摘要导入与签名导入各自保留原有重试语义。

证明记录签发时的源节点和恢复代次。它不证明此后源节点仍在线、没有恢复过、签名密钥仍获授权，或数据达到应用一致性。跨节点收到的 Backup 作为接收端备份恢复到新卷，接收端仍独立审批激活。已导入的 Backup 不能以接收节点作为原始来源再次签发。[两套独立权威、恶意输入及登记故障验收](../tests/test_backup_transfer.py)

#### 检查并完成中断的备份登记

在接收节点停止服务后，可以按原清单摘要检查该次导入。命令从私有数据库副本读取已确认的见证，核对最终目录身份和各成员内容。若报告为 `Prepared` 与 `VerifiedPublished`，可以明确批准该次登记。恢复文件只包含报告中的 `recovery` 对象。

```bash
psiosd --config /etc/psios/core.json inspect-backup-intake \
  --manifest-digest "$MANIFEST_DIGEST" > intake.json
jq -e '.recovery' intake.json > recovery.json
psiosd --config /etc/psios/core.json reconcile-backup-intake \
  --recovery-file recovery.json
```

恢复命令重新核对接收权威、恢复代次、确切导入修订和原目录，再同步目录与父目录并提交登记。签名导入使用准备阶段留存的签名和原公钥。外部副本或公钥文件遗失时，已经发布的完整目录仍可完成原登记。对仍保留的已登记备份，再次提交原恢复对象会返回原 Backup 回执。

`NoPublishedDirectory` 表示当前看不到最终目录，不能据此判定改名从未发生。`Conflict` 表示目录身份或内容不能与登记证据匹配。两种情况下均不会生成恢复对象。若最终目录缺失且源副本仍可用，可通过原导入命令重试。目录冲突需要保留现场并单独调查。已正常删除的 Backup 报告为 `Retired`。校验默认核对最多 256 MiB 负载，可以通过 `--max-bytes` 明确调整。[签名与传统导入的故障验收](../tests/test_backup_transfer.py)

核验或复制失败时工具清理未发布的暂存目录。若恢复进程被强制中止或断电，可能留下该次操作的 `.stage-*` 目录。确认没有恢复进程持有它后，可在保留备份副本的情况下清理该暂存目录。

## 检查状态盘

示例检查 raw 镜像第一分区内的 `/var/lib/psios` 和 `/var/lib/psios.witness`。将 `SERVICE_UID` 设为原服务账户 UID。若目录的访问权限依赖独立 GID，增加 `--gid`。镜像直接包含文件系统时省略 `--partition`。其他布局可用 `--state` 和 `--witness` 指定分区根目录内的绝对路径。

```bash
python3 rescue-core/rescue/inspect.py node.raw --partition 1 --uid "$SERVICE_UID" \
  --iso alpine-virt-3.23.8-x86_64.iso --core rescue-core/psiosd --core-sha256 "$core_sha"
```

返回码为零时，标准输出包含节点 ID、磁盘格式、迁移需求、SQLite 与外键检查、见证状态和所有者凭据轮换状态。数据库与 SQLite 日志在客户机的临时内存盘上核验。默认复制上限为 64 MiB，可用 `--max-copy-mib` 指定 1 至 256 MiB。核验失败时标准错误给出失败阶段；例如 `filesystem-recovery-required` 表示应先对独立副本处理文件系统恢复，`authority-PermissionDenied` 可用于排查 UID、目录权限或路径。

对于本仓库创建的 QEMU 开发实例，可在关机后将可信的系统盘导出到新目录，再按分区检查。

```bash
export_dir=$(mktemp -d)
qemu-img convert -f qcow2 -O raw ./node-a/system.qcow2 "$export_dir/node.raw"
```

启动器以只读文件描述符传递源镜像，并保留 QEMU 镜像访问锁。客户机只读挂载指定设备，关闭日志重放、设备解释和文件执行。客体内的检查进程使用原服务 UID。网络关闭，检查结果从独立串口返回。应使用已停止的镜像或存储层冻结的副本；其他直接改写文件的进程可能忽略 QEMU 锁。默认模式遇到待重放的 ext4 日志时中止。SQLite 自身的 WAL 和回滚日志由既有 Core 核验器处理。

## 从固件启动并导出救援光盘

在前面的检查命令末尾加上 `--firmware bios` 或 `--firmware uefi`，可以从 El Torito 光盘启动同一套检查。再加 `--save-rescue-iso node-rescue.iso`，核验成功后才会发布完整 ISO。输出路径必须尚不存在。光盘同时包含 SeaBIOS 和 amd64 UEFI 的启动入口，带有本次指定的 UID、GID、分区及目录。节点数据库、所有者凭据和 EFI 变量保留在各自设备上。

构建机另需 xorriso、含 x86_64-efi 模块的 GNU GRUB、mtools 及 mkfs.fat。BIOS 使用已固定的 ISOLINUX 文件。UEFI 的 GRUB 可执行文件内嵌内核、初始内存盘和启动配置，直接从自己的内存盘读取。UEFI 验收使用宿主受信任的标准 4 MiB OVMF 固件与独立变量副本，默认从 `/usr/share/OVMF` 读取，可用 `--ovmf-dir` 更改。

可先运行 `sha256sum node-rescue.iso`，通过可信渠道传递摘要，再在另一台装有 QEMU 的 Linux 主机按该摘要复核，并启动这份光盘。下面示例将已停止的节点盘作为第一个只读 virtio 块设备，复用制作光盘时的检查设置。

```bash
qemu-system-x86_64 -nodefaults -no-user-config -no-reboot -machine pc -m 768 \
  -nic none -display none -monitor none -serial null -serial stdio \
  -boot order=d,strict=on -cdrom node-rescue.iso \
  -drive file=node.raw,format=raw,if=virtio,readonly=on
```

客户机从第二个串口输出 `PSIOS-RESCUE/1`、状态和核验结果，随后关机。固件可能先在同一串口输出启动消息。光盘只允许检查只读块设备，遇到可写设备或需要文件系统日志重放的盘会停止。临时日志重放仍通过受控的直接内核启动模式执行。默认光盘已在 QEMU 的 SeaBIOS 和 OVMF 上验证。归档构建证明覆盖制盘程序；本地生成的 ISO 摘要随工具链、Core 和站点设置而定。

### 在 Secure Boot 下核验

将下列参数附加到检查命令，可生成由已登记证书签名的 UEFI 救援光盘。构建机需要 `ukify`、`sbsign`、`sbverify`、OpenSSL 及可信的 systemd EFI stub。`--ovmf-vars` 指向已登记签名证书并启用 Secure Boot 的 4 MiB OVMF 变量文件，检查器只使用该文件的临时副本。

```sh
--firmware uefi --ovmf-vars enrolled-vars.fd --save-rescue-iso node-secure-rescue.iso \
--secure-boot-key signing.key --secure-boot-cert signing.pem \
--efi-stub /usr/lib/systemd/boot/efi/linuxx64.efi.stub --efi-stub-sha256 "$VERIFIED_STUB_SHA256"
```

签名介质在触碰受检磁盘前由来宾内核确认 Secure Boot 已强制启用。成功报告增加 `secure_boot` 字段，记载签名证书与 EFI stub 摘要。已验证的 ISO 可独立启动。为 QEMU 选择 `q35,smm=on`、`OVMF_CODE_4M.secboot.fd` 和单独复制的登记变量文件，并使用 `-global cfi.pflash01.secure=on`。继续将源盘作为第一个只读 virtio 设备，报告仍在第二串口。Secure Boot 未启用或源盘可写时，救援程序停止。该配置已在 amd64 OVMF 验收，物理机与加密卷属于后续验证范围。[原生签名盘说明](native-disk.md#使用已登记证书构建-secure-boot-盘) · [独立签名介质验收](../tests/rescue_secure_boot.py)

## 在临时副本重放 ext4 日志

突然断电后，可在同一条检查命令末尾增加 `--journal-overlay-mib 128`。QEMU 将只读原盘作为后备，所有恢复写入定向到一次性 QCOW2 文件。内核以只读挂载触发必要的 ext4 日志重放，卸载后再次检查文件系统状态，把客户机块设备设为只读，再执行 Core 核验。重放或核验失败时退出，原盘始终以只读方式打开。源文件系统已标记有错误时仍需单独修复。

参数单位是 MiB，可选 16 至 512，涵盖临时 QCOW2 数据和元数据。达到宿主文件大小限制或可用空间不足会导致失败，可按实际空间调整上限。该限额与 `--max-copy-mib` 分别约束 ext4 恢复和 SQLite 核验。启用临时副本后，成功 JSON 增加 `ext4_journal`，其值为 `Clean` 或 `ReplayedOnTemporaryOverlay`。此命令退出时丢弃恢复写入；需要重新上线时，应另在可写副本上执行并验收恢复。

该路径用于单一未加密 ext4 文件系统内的只读检查。LUKS 解锁、多盘布局、物理盘写入、文件系统修复、凭据重置和启动部署选择仍需独立实现。磁盘上的数据库与见证相符只证明内部一致性。判断整个快照是否回到了较早的有效状态，还需要盘外信任锚。[故障验收](../tests/rescue_disk.py) · [宿主与救援规范](v1/docs/03-host-updates-network-devices.md)
