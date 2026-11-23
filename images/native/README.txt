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
      --console-hash operator.hash

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
