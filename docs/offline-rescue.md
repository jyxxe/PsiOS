# 从外部核验无法启动的节点

`images/rescue/inspect.py` 在独立的 amd64 QEMU 虚拟机中运行 `psiosd verify`。它适用于已停止的节点或一致性快照，输入为普通文件形式的 raw ext4 文件系统，或含有 ext4 分区的 raw 磁盘。操作员需要宿主机上读取镜像的权限、可信的 Core 和原服务账户的数字 UID。即使被检系统的内核、服务管理器和用户程序无法启动，仍可执行核验。

## 准备可信的工具

Linux 宿主需要 Python 3.11+、GNU cpio、`isoinfo` 和 `qemu-system-x86_64`。临时日志重放还需要 `qemu-img`。从可信的 PsiOS checkout 运行。救援内核与初始内存文件系统来自固定的 Alpine virt 3.23.8 测试介质，读取前先核验整个 ISO，提取后再校验这两个文件。介质摘要及上游发布签名者指纹记在仓库中。[固定输入](../images/rescue/base.json) · [上游发布目录](https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/)

先取得所选主线提交的独立 Core 归档和构建证明。下面的 `EXPECTED_MAIN_SHA` 取自可信主线记录。`trusted_root.jsonl` 是事先通过可信连接取得的信任根。核验后的归档提供救援程序及其预期摘要。[取得和核验独立 Core](core-local.md#portable-amd64-core)

```bash
tools/verify-portable-core psios-core-linux-amd64.tar.gz "$EXPECTED_MAIN_SHA" provenance.jsonl trusted_root.jsonl
mkdir rescue-core
tar -xzf psios-core-linux-amd64.tar.gz -C rescue-core
core_sha=$(awk '$2=="psiosd" {print $1}' rescue-core/SHA256SUMS)
```

## 检查状态盘

示例检查 raw 镜像第一分区内的 `/var/lib/psios` 和 `/var/lib/psios.witness`。将 `SERVICE_UID` 设为原服务账户 UID。若目录的访问权限依赖独立 GID，增加 `--gid`。镜像直接包含文件系统时省略 `--partition`。其他布局可用 `--state` 和 `--witness` 指定分区根目录内的绝对路径。

```bash
python3 images/rescue/inspect.py node.raw --partition 1 --uid "$SERVICE_UID" \
  --iso alpine-virt-3.23.8-x86_64.iso --core rescue-core/psiosd --core-sha256 "$core_sha"
```

返回码为零时，标准输出包含节点 ID、磁盘格式、迁移需求、SQLite 与外键检查、见证状态和所有者凭据轮换状态。数据库与 SQLite 日志在客户机的临时内存盘上核验。默认复制上限为 64 MiB，可用 `--max-copy-mib` 指定 1 至 256 MiB。核验失败时标准错误给出失败阶段；例如 `filesystem-recovery-required` 表示应先对独立副本处理文件系统恢复，`authority-PermissionDenied` 可用于排查 UID、目录权限或路径。

对于本仓库创建的 QEMU 开发实例，可在关机后将可信的系统盘导出到新目录，再按分区检查。

```bash
export_dir=$(mktemp -d)
qemu-img convert -f qcow2 -O raw ./node-a/system.qcow2 "$export_dir/node.raw"
```

启动器以只读文件描述符传递源镜像，并保留 QEMU 镜像访问锁。客户机只读挂载指定设备，关闭日志重放、设备解释和文件执行。客体内的检查进程使用原服务 UID。网络关闭，检查结果从独立串口返回。应使用已停止的镜像或存储层冻结的副本；其他直接改写文件的进程可能忽略 QEMU 锁。默认模式遇到待重放的 ext4 日志时中止。SQLite 自身的 WAL 和回滚日志由既有 Core 核验器处理。

## 在临时副本重放 ext4 日志

突然断电后，可在同一条检查命令末尾增加 `--journal-overlay-mib 128`。QEMU 将只读原盘作为后备，所有恢复写入定向到一次性 QCOW2 文件。内核以只读挂载触发必要的 ext4 日志重放，卸载后再次检查文件系统状态，把客户机块设备设为只读，再执行 Core 核验。重放或核验失败时退出，原盘始终以只读方式打开。源文件系统已标记有错误时仍需单独修复。

参数单位是 MiB，可选 16 至 512，涵盖临时 QCOW2 数据和元数据。达到宿主文件大小限制或可用空间不足会导致失败，可按实际空间调整上限。该限额与 `--max-copy-mib` 分别约束 ext4 恢复和 SQLite 核验。启用临时副本后，成功 JSON 增加 `ext4_journal`，其值为 `Clean` 或 `ReplayedOnTemporaryOverlay`。此命令退出时丢弃恢复写入；需要重新上线时，应另在可写副本上执行并验收恢复。

该路径用于单一未加密 ext4 文件系统内的只读检查。LUKS 解锁、多盘布局、物理盘写入、文件系统修复、凭据重置和启动部署选择仍需独立实现。磁盘上的数据库与见证相符只证明内部一致性。判断整个快照是否回到了较早的有效状态，还需要盘外信任锚。[故障验收](../tests/rescue_disk.py) · [宿主与救援规范](v1/docs/03-host-updates-network-devices.md)
