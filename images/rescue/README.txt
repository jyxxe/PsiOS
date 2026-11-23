PsiOS 离线救援核验（Linux amd64）

从已核验的 psios-core-linux-amd64.tar.gz 解包。在含 psiosd、psiosctl
和 SHA256SUMS 的目录工作，先运行下列命令。

    sha256sum --strict --check SHA256SUMS

准备 Python 3.11+、GNU cpio、isoinfo、qemu-system-x86_64，以及
rescue/base.json 所列的救援 ISO。下载可在另一台机器完成。检查器在
启动前核对 ISO、内核和初始内存盘摘要。受检节点必须已关机，或使用
存储层冻结的一致性快照。示例检查 raw 镜像第一分区上的未加密 ext4。

    core_sha=$(awk '$2=="psiosd" {print $1}' SHA256SUMS)
    python3 rescue/inspect.py node.raw --partition 1 --uid "$SERVICE_UID" \
      --iso "$RESCUE_ISO" --core ./psiosd --core-sha256 "$core_sha"

SERVICE_UID 为原服务账户的数字 UID。独立 GID 可用 --gid 指定。直接
检查 ext4 文件系统镜像时省略 --partition。默认目录是 /var/lib/psios
和 /var/lib/psios.witness，可用 --state、--witness 指定实际位置。

遇到需要重放的 ext4 日志时，安装 qemu-img，再添加
--journal-overlay-mib 128。恢复写入仅进入一次性的限额副本。退出码为
零时，标准输出是 Core 的核验结果。核验不会修复原盘或启动受检系统。
