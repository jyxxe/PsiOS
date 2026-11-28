# PsiOS 单机 Core

`psiosd` 把资源、任务、授权账本和事件保存在同一个 SQLite 数据库中。`psiosctl`、Rust SDK 和原生 Agent 都通过 Unix socket 访问这个权威服务。这个实现使用 v1.0.0 契约，运行于 Linux。OCI 测试需要可访问的本机 Docker Engine。基础管理、存储和 Agent 状态任务可以独立运行。

节点调度、受管 systemd 服务、分块卷数据、应用版本切换和持久 Agent 会话见[宿主与 Agent Runtime 指南](core-runtime.md)。

## Portable amd64 Core

独立开发包包含 `psiosd`、`psiosctl`、按需启动的 STDIO 工具适配器 `psios-mcp`、`rescue/` 下的离线磁盘核验工具和 `native/` 下的独立开发盘构建器。这三个程序在 Alpine 上以 musl 静态链接，可直接用普通 Linux 用户运行。构建器检查 ELF 的解释器与动态库依赖。开发包不安装系统服务，操作员负责保存状态目录、见证目录与所有者凭据。宿主服务管理、OCI 和网络查询分别需要系统提供对应工具与权限。

可从独立的 `PsiOS standalone Core` 工作流下载 `psios-core-linux-amd64` 制品。通过系统测试和归档内救援工具启动验收的主线提交会同时提供 `psios-core-linux-amd64-attestation`。先从可信主线记录取得预期的完整提交 SHA，再下载同一次运行的归档和证明。从可信 checkout 使用支持 `gh attestation verify --source-digest` 的 GitHub CLI 核验归档，随后解包。

```bash
tools/verify-portable-core ./psios-core-linux-amd64.tar.gz "$EXPECTED_MAIN_SHA" ./provenance.jsonl
```

离线核验时，提前通过受信任的连接执行 `gh attestation trusted-root > trusted_root.jsonl`，将该文件作为上述命令的第四个参数。核验覆盖整个 tar.gz，并绑定仓库、构建工作流、主线提交和 GitHub 托管执行器。只提供文件摘要的早期开发包可从对应提交自行构建。

也可在安装了 Git、GNU tar、gzip 及 Docker Buildx 的 Linux 构建机上直接生成。归档保留可执行权限、完整源码提交、Cargo.lock 和文件校验和。若构建网络使用专用 CA，设置 `CARGO_HTTP_CAINFO` 指向受信任的 CA 文件。

```bash
tools/build-portable-core ./psios-core-linux-amd64.tar.gz HEAD
mkdir ./portable-core
tar -xzf ./psios-core-linux-amd64.tar.gz -C ./portable-core
(cd ./portable-core && sha256sum -c SHA256SUMS && cat source-commit)
```

下方本机启动示例中的 `target/debug/` 可替换为 `./portable-core/`，并跳过 Cargo 编译。CI 在只读、断网的 Alpine 用户空间以非特权用户运行同一份二进制，覆盖身份授权、任务幂等和进程崩溃后的恢复。CI 还将同一归档作为只读根文件系统，直接启动两个程序，强制终止服务后以只读方式核验状态卷，再重放原任务。该验收覆盖单节点 Core，系统引导和各发行版安装另行验收。[空根文件系统恢复验收](../tests/portable_recovery.py)

## 本机启动

在仓库根目录执行。第一次初始化要求状态目录及其同级见证目录为空。示例使用新建临时目录，数据随该目录保留。

```bash
cargo build --locked --workspace
workdir=$(mktemp -d)
target/debug/psiosd --state "$workdir/core" init
target/debug/psiosd --state "$workdir/core" serve &
daemon_pid=$!
for attempt in $(seq 1 100); do [ -S "$workdir/core/api.sock" ] && break; sleep 0.05; done
ctl() {
  target/debug/psiosctl --socket "$workdir/core/api.sock" \
    --token-file "$workdir/core.witness/owner.token" "$@"
}
ctl status
task=$(printf '{}' | ctl submit system.status --key first-status | jq -r .task_id)
ctl wait "$task" > "$workdir/task.json"
jq '{reference: .result.artifact_refs[0]}' "$workdir/task.json" | ctl call Artifact.Read
```

结果也可导出为持久存储的原始 JSON 字节。`ctl artifact` 接受任务回执中的固定版本引用，在输出前核对完整 SHA-256。命令默认最多读取 8 MiB，可用 `--max-bytes` 按调用方需要调整。

```bash
jq -e '.result.artifact_refs[0]' "$workdir/task.json" | ctl artifact > "$workdir/result.json"
```

`Artifact.ReadChunk` 接受 `reference`、以字节计的 `offset` 和 `length`，单块长度为 1 至 131072。返回 `data_base64`、`offset`、`total_bytes` 以及到末尾时为 `null` 的 `next_offset`。各次请求都检查当时的读取权限与制品归属。Rust SDK 的 `Client::artifact_bytes` 可在指定字节上限内合并分块并验证摘要。该入口已用超出单帧上限的真实内核盘点结果验收。[端到端网络制品测试](../tests/test_network.py)

从数据库格式 30 开始，新制品的字节分块与产生它的任务结算在同一 SQLite 事务内提交。分页仅读取请求范围内的块并核对各块摘要。停服执行 `psiosd verify` 时会再核对全部新制品的完整字节摘要。原有内联制品继续使用已签发的引用和原导出字节。只读任务提交结果失败时释放占用，报告存储错误。写操作的回执提交失败时保留原占用，等待核实副作用。[制品存储验收](../tests/test_artifact.py) · [格式 29 升级验收](../tests/artifact_upgrade.py)

本机所有者可用 `Task.EvictResults` 回收已结算只读任务的分块结果，请求包含任务 `id`、`expected_resource_version` 和 `reason`。服务校验产生结果的固定能力版本，并保护活跃 WorkflowRun、仍可继续的 Agent 正文上下文以及已用于 Operation 对账的制品。成功回收在一次事务内删除正文、记录清理凭据和审计，并将任务的结果可用性更新为 `Evicted`。原 Task 终态、Run、Operation 和提交去重记录继续可查。以同一旧修订和原因重试，会取得原清理回执。SQLite 可复用回收的页面。

`Artifact.Get` 接受固定版本 `reference`，按任务结果的读取权限返回资源元数据、可用性和清理回执。已回收的正文读取返回 `ResultEvicted`。该入口可查询超出单帧上限的制品元数据。格式 30 已经发布的分块结果在升级后也可回收。[回收和依赖验收](../tests/test_artifact.py) · [格式 30 升级验收](../tests/artifact_retention_upgrade.py)

工作流读取固定前驱制品失败时，会持久进入 `Blocked` 并记录 `ResultUnavailable` 或 `ResultEvicted`。恢复原制品后，使用 WorkflowRun 的 `id` 和当前 `expected_resource_version` 调用 `Workflow.Resume`。调度器重新验证原输入、授权和执行期限。前驱 Task 保留原执行记录，后继只有在成功解析固定输入后才获派发。[工作流恢复制品验收](../crates/psios-core/src/recovery_tests.rs)

缺失或损坏的分块可用此前导出的原 JSON 字节修复。本机所有者运行 `ctl artifact-repair`，提供原引用和导出文件。客户端先核对整个文件的长度和摘要，再逐块提交。

```bash
jq -e '.result.artifact_refs[0]' "$workdir/task.json" | ctl artifact-repair "$workdir/result.json"
```

低层 `Artifact.RepairChunk` 接受 `reference`、原始字节 `offset` 和 `data_base64`。偏移按 `Artifact.Get` 返回的 `repair_chunk_bytes` 对齐，最后一块使用原有长度。每块须与原清单的摘要匹配。相同块重复提交返回 `repaired:false`。已回收的结果返回 `ResultEvicted`。[修复验收](../tests/test_artifact.py)

本机所有者可用 `Task.VerifyResults` 核验已结算任务的固定制品引用和完整字节。请求包含 `id`、当前 `expected_resource_version` 和可选 `max_bytes`，默认 8 MiB，最多 64 MiB。发现缺失或损坏时，Task 产生新修订，在 `result.availability` 中记录 `Unavailable`，并保留 `Succeeded`、Run、Operation 和原始引用。部分修复后再次核验仍返回 `Unavailable`。所有结果通过核验后才写入 `Available`，然后可以恢复依赖的 Agent 会话或 WorkflowRun。两次状态变化均可通过 Task 历史和资源事件查询。[当前结果与权限验收](../tests/test_artifact.py)

```bash
ctl task-verify-results TASK_ID --revision TASK_REVISION
```

`kill -TERM "$daemon_pid"` 请求关闭。再次使用同一目录启动会恢复队列、保留原任务身份和已接受回执。系统服务的打包、首次初始化及升级见[宿主安装指南](host-package.md)。打包单元使用 `/usr/bin/psiosd`。使用过手工单元的主机，迁移前须检查本机单元覆盖和旧二进制路径。服务单元与配置示例使用 `psios` 用户、`/run/psios/api.sock`、`/var/lib/psios` 和独立的 `/var/lib/psios.witness`。[systemd 单元](../contrib/systemd/psiosd.service) · [配置示例](../contrib/systemd/core.json)

## 本机 PsiOS Shell

启动 Core 后，在终端执行 `ctl shell`（沿用上面的 `ctl` 函数）。也可使用同一个 `psiosctl` 的 `--config` 或 `--socket`、`--token-file` 和 `--service-uid`。在同一台机器的浏览器打开终端第一行给出的地址，输入第二行的一次性配对码。

配对后进入本机桌面。从底部 Dock 打开“节点管理”、“任务中心”、“应用管理”、Agent Chat 或“PsiFlow 项目”。宽屏可并排查看多个应用，窄屏按上下顺序排列。点选窗口或 Dock 中的应用可切换焦点。标题栏支持刷新、最小化、最大化、还原和关闭。窗口最小化或被另一最大化窗口遮住时，Shell 会清除其已显示的数据。窗口重新可见时按当前授权重读。任务中心在本次配对期间保留最小化前的页和任务选择。关闭窗口会清除该选择，已提交的 Task 继续由 Core 执行。断开会话会一并清空所有窗口。

Shell 可以查看节点状态、调度开关、执行队列和权威存储余量，以及分页浏览当前主体可见的任务和最新状态。状态盘与见证盘分别显示字节和 inode 的可用量及预留值。低于阈值、已配置 inode 预留但文件系统无法计数，以及只读挂载均有相应提示。节点面板要求 `node.inventory`。列表要求 `resource.read` 和 `task.read`，详情按 `task.read` 及任务归属检查。任务列表使用 Core 签发的快照游标。返回首页或快照过期后重新刷新会读取新快照。Core 暂停服务后可在同一会话刷新。 任务详情可按固定引用预览不超过 64 KiB 的结果制品。Shell 在展示前向 Core 核对任务归属和制品权限，再校验完整 SHA-256。选择“下载”可保存不超过 8 MiB 的原始字节。Shell 逐块读取，核对完整摘要并复核任务引用后才发送附件。浏览器复核接收到的字节，文件名包含同一 SHA-256。超过界面上限时可将固定引用交给 `psiosctl artifact` 并显式指定字节上限。

“应用管理”分页显示当前主体获准查看的本机应用名称和状态。选中应用后，使用独立的 `application.releases` 授权读取其固定版本快照。版本列表显示选用、暂存、未选用和退役状态。刷新可取得新快照。窗口被最小化或遮挡后，再次打开会按当前授权取得列表和详情。应用安装、启动、切换或退役继续沿用 Core 的 Task 与审批流程。

在任务详情展开“执行历史”可读取截至当前详情修订的历史，每页最多三条。各条显示 Core 记录的时间、当时的生命周期、结果确认程度和关联的 Run 或 Operation。翻页期间任务即使继续运行，历史上界仍固定。刷新详情后可查看更晚的修订。每次翻页都重新检查当前 `task.read` 授权和任务归属。

以 `ctl shell --cancel-pending-tasks` 启动时，可以在详情中取消排队或等待审批的任务。Shell 先用当前主体读取任务，随后带同一修订调用 `Task.Cancel`。Core 在同一事务内核对修订、归属和 `task.submit` 权限。如果任务已经开始，刷新可查看新的状态。请求中断时也可刷新核实结果。

Agent Chat 使用当前配对主体的 `chat.read` 权限查看私聊及多人房间。会话目录和消息有独立分页。消息按成员可见范围和提交顺序展示，包括发送主体和权威接收时间。最小化、遮挡或关闭会清除显示的会话名称和正文，恢复时按当前成员关系重新读取。接受邀请和已读确认可通过当前 Core 接口完成。[本机会话协议](core-runtime.md#本机会话)

“PsiFlow 项目”按项目成员资格及 `project.read` 授权显示项目和当前主体的角色。工作项单独分页，展示当前状态、负责人、优先级和未完成的依赖。点开工作项可查阅列表所指的固定修订、描述和审核记录。任务及制品证据继续使用 Core 的结果读取权限。可下载该修订引用的制品，Shell 在返回字节前重新检查项目及证据权限，并验证至多 8 MiB 的文件摘要。重新显示窗口时会重查成员资格及授权。创建、分派和提交证据可通过 CLI 或 System API 操作。[本机 Project 协议](core-runtime.md#本机-project-与工作项验收)

启动时加入 `psiosctl shell --review-work-items` 可在证据详情中选择并确认审核结论。独立评审者须具有 `project.review` 和所引 Task 的 `task.results.read`。Core 复核原修订、当前授权和证据，接受后进入已验证状态，退回后仍可重新提交证据。审核回执丢失时，“重新读取工作项”显示持久的当前结论。

启动时加入 `psiosctl shell --send-chat` 可以使用历史下方的发送框。每条消息上限为 4096 个 UTF-8 字节。Core 按当前成员及授权准备并提交固定内容。回执丢失时，“核对并重试”会在有效期内复用同一份准备结果，并显示已有回执。离开窗口会清除草稿及待重试请求。再次打开有待核实发送的会话时，可先阅读当前历史确认提交结果。

页面和脚本随 CLI 提供，监听随机的 IPv4 回环端口。终端应仅供当前操作者使用。Core 凭据保留在 CLI 进程，浏览器令牌保留在页面内存，重新加载整页后需重新启动 Shell。配对窗口为两分钟，仅允许成功配对一次。服务器执行五分钟空闲和一小时绝对期限。配对页面通过一条经认证的回环连接等待会话结束。等待期间沿用原空闲期限。终端退出、连接中断或会话到期时，即使操作者没有再次点击，页面也会清除已显示的节点、任务和制品。离开页面时会中止在途请求，并清除页面内的令牌和数据。断开会话、Core 拒绝当前主体身份或终端收到退出信号会关闭监听。默认会话只开放读取操作。远程管理和桌面会话仍待实现。[进程级验收](../tests/test_shell.py)

## Agent 授权与执行

以下命令接续上述本机示例，使用 `jq` 保存一次性返回的 Agent 凭据。Agent 只获得本机状态工具和读取自己任务的权限。本例预留最多三个 Run 执行单位，授权有效期十分钟。

```bash
umask 077
printf '{"name":"node-observer"}' | ctl call Agent.Register > "$workdir/agent.json"
jq -r .token "$workdir/agent.json" > "$workdir/agent.token"
delegation=$(jq '{principal_id, actions:["task.submit","task.read","system.status"], units:3, ttl_seconds:600}' \
  "$workdir/agent.json" | ctl call Grant.Issue | jq -r .delegation_ref.resource_id)
target/debug/psiosctl --socket "$workdir/core/api.sock" --token-file "$workdir/agent.token" \
  agent --key observe-1 --delegation "$delegation"
```

`Client::execute_agent` 先通过 Tool Gateway 读取状态、发现已授权能力，再调用 `AgentProvider::propose`。Provider 返回工具名和结构化输入。随后 SDK 提交 Task、观察 Run 结果、按 ArtifactRef 取回正文。内置 `StatusProvider` 可以离线运行。第三方程序可以实现同一 trait 或直接使用本机协议。[Rust 客户端与 Provider 接口](../crates/psios-api/src/client.rs)

在多用户部署中，把 socket 放在服务账户拥有的目录中，通过组开放连接。保持 state 和 witness 目录为服务账户独占的 0700 目录。用 `Agent.Register.peer_uid` 绑定 Agent 的 Linux UID，客户端用 `--service-uid` 或 `Client::with_service_uid` 固定服务 UID。能读取服务账户文件的进程属于同一信任边界。Docker socket 只交给可信宿主服务。

## API 绑定

每个连接交换一次请求和响应。帧是 4 字节大端长度加 UTF-8 JSON，最大 512 KiB。每个请求包含 `schema_version`、`request_id`、`method`、`token` 和 `params`。响应回显 `request_id`，携带 `result` 或符合 v1 的 `error`。服务拒绝重复键、超限输入及安全请求里的未知字段。资源引用固定 `resource_id`、`version` 和 `digest`。本机修订是不可比较的协议字符串。用 `psiosctl call METHOD FILE` 调用下表接口，省略 FILE 时从标准输入读取 JSON。

| 接口 | 请求与行为 |
| --- | --- |
| Principal.WhoAmI、Agent.Register、Principal.Revoke | 注册返回独立主体和凭据。撤销使用 id 与 expected_resource_version，服务同时关闭该凭据。 |
| Grant.Issue、Grant.Revoke、Delegation.Create、Delegation.Revoke、Budget.Get | Grant 限定主体、动作、有效期和执行单位。允许转授的父委托可为另一主体派生一层状态任务委托，共用父预算。每次派发核验当前授权。 |
| Resource.Get、Resource.Resolve、Resource.List | Get 使用 id。Resolve 使用 reference 并验证确切版本和摘要。List 支持 kind、limit、cursor，返回 v1 SnapshotPage。 |
| Event.Poll、Event.Ack、Event.Offset | 从列表的 watch_start_cursor 拉取。重读同一位置得到相同 event_id。Ack 用 consumer、previous、cursor 条件更新检查点，Offset 用 id 取回检查点。授权变化或超时需要重新列举。 |
| Task.Prepare、Task.Submit、Task.LookupSubmission | Prepare 把 tool、input、key、delegation_id 和可选 deadline_at 变成可重传的 v1 TaskSubmit。Submit 和 Lookup 接受这个完整对象。SDK 在 ACK 丢失后先查询原键。 |
| Task.Get、Task.Cancel、Task.CancelOrigin、Task.Retry | Cancel 使用任务版本。CancelOrigin 接收 v1 CancelOrigin 并支持先到墓碑。Retry 需要旧任务版本和新键，创建关联的新任务。 |
| Approval.Decide、Run.Get、Operation.Get、Operation.Reconcile | 决策绑定已显示的计划摘要、审批请求版本和单次使用单元。Reconcile 使用 Operation 的 id 和 expected_resource_version。可核实文本写入、分块追加、受管服务启停和 OCI 容器删除后的停止结果，以及有启动检查点的在运行容器。 |
| Artifact.Get、Artifact.Read、Artifact.ReadChunk、Artifact.RepairChunk、Task.VerifyResults、Task.EvictResults | 制品查询使用固定版本 reference，分块读取另需 offset 和 length。本机所有者可修复分块、核验已结算结果，或回收已结算的只读结果。 |
| Secret.Put、Secret.Rotate、Secret.Revoke、Audit.List | 写入返回 SecretRef，轮换和撤销使用原修订。secret.mac 可使用活动秘密计算 HMAC。Audit.List 支持 after 和 limit。 |
| Tool.Call、Capability.Discover | 即时入口支持 system.status。执行其他注册工具通过 Task.Submit，服务端承担审批、预算、派发和回执保存。 |
| StoragePool.Get、StoragePool.Configure、Application.Configure | 本地池实时报告容量。所有者用 CAS 开关新卷分配。应用停止后可用 CAS 修改非秘密环境配置。 |

服务嵌入仓库中的 JSON Schema，无需在线获取 Schema。CLI 的 `validate task-submit FILE` 可检查结构和提交摘要。摘要使用带用途前缀的 JCS 和 SHA-256。已启用的写接口会进一步检查引用类型、当前授权、版本与真实资源身份。[v1 Schema](v1/schemas/README.md) · [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785)

本机授予 `secret.manage` 的主体可用 `psiosctl secret put NAME --value-file FILE` 导入秘密，用 `psiosctl secret rotate ID --revision REV --value-file FILE` 轮换，并用 `psiosctl secret revoke ID --revision REV` 撤销。文件须由调用者拥有且仅调用者可读。省略 `--value-file` 时从管道或重定向读取。输入为 1 到 8192 字节的 UTF-8，末尾换行作为原值保留。CLI 输出仅包含新的 SecretRef。Rust SDK 对应 `Client::put_secret`、`Client::rotate_secret` 和 `Client::revoke_secret`。[真实 CLI 与权限验收](../tests/test_secret_cli.py)

三项操作均可使用 `--idempotency-key KEY` 或 SDK 的 `*_secret_with_key` 方法。每个逻辑请求生成独立的客户端键。如果连接在提交后中断，带同一个键、版本和输入重试会取得原回执，即使 Secret 随后已被轮换或撤销。服务每次核验当前授权，拒绝用旧键提交不同请求。写入与回执同一事务提交，回执保存键控摘要和 SecretRef。离线核验与检查点发布验证保留的回执。[丢失响应与回执验收](../tests/test_secret_receipts.py)

## 核对本机挂载拓扑

`node.mounts` 接受空对象，通过普通只读 Task 返回 Core 当前 Linux 挂载命名空间的制品。Grant 须包含 `task.submit`、`task.read` 和 `node.mounts`，可用 `tool_targets` 限定节点 ID。结果包含启动 ID、挂载命名空间 inode、观察时间，以及各挂载的 ID、父 ID、设备主次号、文件系统类型、根和挂载点。挂载点的只读标志与 superblock 的只读标志分别报告，便于区分只读 bind 与整个文件系统只读。制品也保存选定的 VFS 标志和传播组。[Linux mountinfo 定义](https://www.kernel.org/doc/html/latest/filesystems/proc.html)

内核转义的路径按原字节解析，非 UTF-8 名称使用 `{ "base64": "…" }`。命名空间文件系统的根可能是 `net:[…]` 等内核名字。挂载 ID 与当前观察绑定，卸载后内核可重新使用 ID。制品只返回选定的 VFS 字段。一次最多观察 4096 项和 512 KiB 内核输入，超限时返回 `QuotaExceeded`。现有制品分块接口可读取超过单个 RPC 帧的报告。[真实 namespace 与 bind 挂载验收](../tests/test_mounts.py) · [格式 15 升级](../tests/mounts_upgrade.py)

## 应用、卷与审批

| Task 工具 | input |
| --- | --- |
| system.status | 空对象 |
| secret.mac | secret_id、message |
| volume.create | name |
| volume.read、volume.write | id、expected_resource_version、单个文件名 name；write 另需 UTF-8 content，最大 64 KiB |
| volume.usage | `target` 包含卷 ID 和预期修订。可选 `path` 指向卷内子目录，默认查询整个数据目录。 |
| volume.delete、volume.activate、backup.create、backup.restore、backup.delete | id、expected_resource_version。restore 的目标是 Backup，产生新 Volume，读取核实后通过 activate 开放写入。 |
| application.install | 本地 OCI 镜像配置摘要 image_id、name、release_version、license_expression、argv、environment、cpu_millicores、memory_max_bytes、pids_max、ephemeral_max_bytes，可选 volume 指向卷 id 和 expected_resource_version |
| application.start、application.stop、application.uninstall、application.health、application.logs | 应用 id、expected_resource_version |

volume.write、volume.delete、volume.activate、backup.delete、application.install、application.start 和 application.uninstall 会等待审批。读 Task 的 ApprovalRequired 条件可取得审批请求 ID，Resource.Get 读取该请求和 OperationPlan。检查 plan 的 allowed_effects、输入、目标版本、预算和期限后，向 Approval.Decide 提交审批请求的 id、expected_resource_version、plan_digest 和布尔值 approve。服务端在实际执行前再次验证这些条件。

单机 OCI 适配器接受已在本机引擎中的 Linux 镜像配置摘要，生成固定的 ApplicationManifest 与 ReleaseSpec。Deployment 固定 Release、配置和卷世代，RuntimeInstance 记录引擎容器身份及运行观测。启动从该 Release 解析 argv 和资源限制，在引擎创建容器后核验配置，再启动进程。隔离配置包括非 root UID、只读根、禁用网络、移除 capabilities、no-new-privileges、CPU/内存/PID 限制和有界 tmpfs。health 提供进程存活观测，logs 返回有界尾部。支持显式 /data 绑定，拒绝隐式镜像卷。安装来源记录为 UserSuppliedUnverified，组件清单覆盖所见镜像层。

应用停止后，backup.create 才能取得排他占用。后端保存文件内容、长度、模式、目录和摘要，支持服务 UID/GID 的普通文件及目录。遇到符号链接、硬链接文件、扩展属性或嵌套挂载会拒绝备份。单份备份默认上限为 256 MiB，可通过 `backup_max_bytes` 调整，并限 4096 个条目。发布前重新核对源卷身份、目录与普通文件的 inode、大小、权限、修改及状态变更时间。发现变化会终止本次备份并清理暂存。直接访问宿主文件系统的写入者仍须在备份期间停写，此后端未提供原子文件系统快照。恢复核验权威记录的清单摘要和每个文件的内容摘要，使用目录文件描述符逐级创建文件并恢复权限。新备份还保留普通文件、目录及卷根的纳秒级修改时间。对旧清单继续按原格式恢复；访问时间、状态变更时间及创建时间不在此配置的恢复范围。Core 在暂存目录完成核验后发布新卷。工作负载创建的深层目录可按原路径还原。故障域为同一本机文件系统，故障保证为单卷离线 crash_consistent。卸载应用保留卷，原卷与恢复卷具有不同身份及数据世代。

备份复制的最长许可窗口由 `backup_capture_timeout_seconds` 配置，默认 30 秒，范围为 1 至 86400 秒。`StoragePool.Get` 返回实际配置。Task 截止时间与授权期限仍共同约束许可，发布前复核执行许可和目录身份。复制和遍历期间会检查取消、撤权、期限及守护进程关机。已知的发布前中断会清理暂存并释放原卷占用。[取消、撤权及超时验收](../tests/test_backup_capture_control.py)

卷与备份使用独占的内部暂存目录，通过拒绝覆盖已有名称的原子改名发布。已知的发布前失败会当场清理暂存。若服务在发布前中断，重启时持有独占权威锁回收。这包括旧版备份暂存以及已恢复为只读权限的子目录。遇到嵌套挂载或异常的暂存目录会保留并记录。只有暂存清理证据时，在途操作仍等待效果对账。[真实写入失败与进程中断测试](../tests/test_storage_recovery.py)

新恢复卷在权威状态中记录通过核验的目录树摘要。`volume.activate` 持有卷的独占占用，从实际目标重新读取内容、路径、权限和清单中保留的修改时间，再核对摘要。校验失败、提交前取消或服务中断时，卷仍待激活，可重新审批。成功时记录 `restore_verification_ref`，并开放后续写入。源备份已按策略删除时，新恢复卷仍可凭保存的摘要核验。旧版本已经创建的待激活卷沿用身份检查，旧清单经新版恢复时会生成新摘要。[激活完整性测试](../tests/test_storage_recovery.py)

### 删除卷和备份

批准的 `volume.delete` 与 `backup.delete` 在改名前持久记录目录及父目录标识，随后把原目录原子移出活动路径。操作成功时资源进入 `Deleted`，`StorageRetirement` 进入 `PendingCleanup`。后台只回收已提交删除的原目录，遇到身份不符或挂载边界时保留回收记录并重试。可以通过 `storage-retirement-OPERATION_ID` 查询清理进度及 `Reclaimed` 的目录观测。

Core 启动和 `psiosd verify` 双向核对待回收记录与回收队列，并验证原删除审批、执行凭据和目标修订。队列行缺失、指向已清理记录或无法证明原删除归属时，Core 报告 `RecoveryRequired`。合法的重试时间在校验和重启后继续生效。[队列损坏与重试验收](../tests/test_storage_retirement.py)

结算中断时保留原目标的排他占用与目录证据。本机管理员可用 `Operation.Reconcile` 和未知 Operation 的当前修订复核已移出的原目录，或确认原目录仍处于活动路径。有匹配证据时结算同一个 Operation。缺少匹配证据时继续保留占用。格式 39 升级保留等待中的删除审批。[回执丢失和重启验收](../tests/test_storage_retirement.py) · [原审批迁移](../tests/storage_retirement_upgrade.py)

### 查询卷用量

用 `volume.usage` 提交只读 Task，并从 `Artifact.Read` 获取结果。授权需要 `task.submit`、`task.read` 和 `volume.usage`，`tool_targets` 可限制为指定卷。结果固定卷引用及数据世代，分别报告普通文件的逻辑字节、文件分配字节、目录分配字节和总分配字节。目录计数包含本次查询的根。分配字节采用 Linux inode 的 512 字节块计数，稀疏文件的空洞会体现在两个数值的差异中。池级剩余空间可通过 `StoragePool.Get` 查询。[Linux inode 字段](https://kernel.googlesource.com/pub/scm/docs/man-pages/man-pages/+/refs/tags/man-pages-6.13/man/man7/inode.7)

测量持有本地卷的排他占用，且支持读取待激活的恢复卷。Core 通过目录文件描述符遍历并复查 inode 及分配量。每次最多遍历 4096 项和 32 层子目录。新扫描可选择更小的子树。扫描按原始文件名字节区分目录项，可统计含非 UTF-8 名称的目录树。查询参数 `path` 仍使用 UTF-8。符号链接、硬链接文件、设备等特殊文件和嵌套挂载会拒绝整个扫描。直接从宿主写入卷的进程须在测量时暂停写入，以获得稳定观察结果。[文件、权限与扫描边界验收](../tests/test_volume_usage.py) · [格式 14 升级](../tests/volume_usage_upgrade.py)

本机创建备份、恢复、克隆卷，以及从目录离线导入备份时，Core 对实际分配量小于逻辑长度的文件查询 Linux 文件系统报告的数据段和空洞。目标保留这些空洞的逻辑长度，SHA-256 包含空洞处的零字节。源文件已完整分配或文件系统不支持区间查询时，Core 逐字节复制。配额和目标空间预检仍使用逻辑长度。跨节点分块导出传递文件内容，接收方的分配布局可能不同。[稀疏文件验收](../tests/test_volume_clone.py)

### 原地核验一份备份

`backup.restore` 先按清单的逐文件长度估算目标文件系统所需数据块及 inode。目标与 state 或 witness 共享文件系统时扣除配置中相应的容量余量，发布前再检查余量。容量不足时以 `QuotaExceeded` 结束并清理暂存，原备份可在释放空间后重新提交恢复。统计基于实际目标，后续并发写入仍会改变可用空间。[恢复容量验收](../tests/test_backup.py)

停用新卷分配时也能用 `backup.verify` 检查已存备份。给实际备份所有者签发含 `task.submit`、`task.read`、`backup.verify` 且 `tool_targets` 指向该 Backup ID 的 Grant，使用它的 delegation 提交 Task。输入与恢复一样要求备份 ID 和预期修订。核验与还原共用清单摘要和逐文件流式内容检查。核验期间占用该备份，阻止其他 Task 同时删除或恢复它。成功结果记录 `backup_ref`、`manifest_digest`、`entries`、`bytes` 和 `verified_at`，不会分配新卷。启动应用及验证其中数据库的可读性仍需实际恢复演练。

## 持久状态与恢复

任务元数据、输入保留、幂等回执、预算预留、Operation 及事件在同一个 SQLite 事务内提交。每个资源版本同时是一条有序持久事件。服务保存历史修订、操作记录和取消墓碑。每个主体最多保留 4096 个不同输入，总输入上限 64 MiB。队列另有独立上限。当前版本尚无归档与检索历史的 GC。

领取 Run 与签发宿主调用分属两个事务。派发事务先记录单用途 ExecutionPermit、Effect、Run 代次、审计及必要的资源占用。进程在领取后丢失，调度器可以在预算内建立新 Run。已派发的写操作丢失回执时，查询会返回 Unknown，相关资源保持占用。对 volume.write，可用 Operation.Reconcile 在原卷身份和占用下按已批准输入核实文件内容。服务附加证据并结算任务。分块追加按长度和范围内容核实，受管服务可用新观测对账。其他未知宿主效果保留待核实。取消中的调用收到成功回执时会同时显示 Cancelled 和 Known，并保留结果。

独立 witness 目录保存提交标记、密钥与本机所有者凭据。服务先写入本次事务的候选标记，提交数据库，再固定已确认标记，之后才能返回回执或调用宿主。启动时核对标记，可恢复中断提交的相邻两种状态。单独回滚数据库或替换密钥会触发 RecoveryRequired。运行 `psiosd check` 或 `psiosd recover-owner` 需要停服并持有可信状态目录。后者轮换本机所有者令牌。见证和数据库被一起回滚、整机丢失和跨节点接管需要独立信任锚及新的恢复流程。

## 升级或救援前的离线核验

停服后，以状态文件所属的 Linux 账户运行。独立源码部署可将同一命令改为 `psiosd --state /绝对路径 verify`。

```bash
sudo systemctl stop psiosd
sudo -u psios /usr/bin/psiosd --config /etc/psios/core.json verify
```

`verify` 取得现有状态锁，在私有临时目录复制数据库及 WAL 或回滚日志，重建临时 SQLite 视图，检查数据库结构、外键、见证、密钥摘要和所有者凭据。报告包含节点 ID、状态格式、是否需要迁移、待完成的见证提交和所有者凭据轮换。活动服务占用状态、见证不匹配、数据损坏或副本超限时命令以非零状态结束。格式 1 可在升级前核验，原库、日志、见证、凭据和待执行任务均留在原处。

停服后可用目标版本的 `psiosd verify --rehearse-migration` 对同一份私有副本执行正常迁移，并核验迁移后的格式、节点身份、完整性和见证。`--max-copy-mib` 限制数据库和日志的输入大小。`--scratch-dir` 应指向具有足够写入空间的受保护目录，演练过程还需要空间容纳迁移后的数据库和日志。原生停机安装器在容量受限的救援环境中执行演练。[原生停机升级](native-disk.md#停机升级到另一份原生磁盘)

默认复制上限为 1024 MiB；用 `--max-copy-mib N` 调整。`--scratch-dir PATH` 选择状态和见证以外的可信本机磁盘目录，需有足够空间容纳数据库、日志和 SQLite 工作文件。临时副本包含私有 Core 数据。正常退出自动删除，进程被强杀后须由服务账户清理该目录中的本次副本。卷内容、备份文件、宿主实际效果和整机一致回滚不在这次检查范围内。需要完成待提交标记或受支持的迁移时，原有的离线 `check` 和服务启动路径会执行这些变更。

## 验证与后续依赖

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --workspace
cargo test --locked --workspace
python3 tests/system.py
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 tests/oci.py
```

最后一项需要测试账号能够访问本机 Docker、可用的 cgroup/默认 seccomp 和静态 C 编译器。测试在本机生成独立的 scratch 镜像，检查真实容器、服务重启、卷内容和恢复结果，清理本次测试创建的容器和镜像。[进程与故障测试](../crates/psios-core/src/recovery_tests.rs) · [系统 API 测试](../tests/system.py) · [OCI 验收](../tests/oci.py)

当前覆盖单节点、并发宿主工作线程、本机文件系统、受管 systemd 服务与 Docker。跨节点 fencing、在线应用一致性快照、异机备份、完整恢复集重新登记、硬盘断电和 PVE 验证需要下一批环境与驱动。Task 和模型租约分别计量派发次数。模型 token、费用和长期运行配额需要后续计量器。任意外部清单导入、软件包级 SBOM、定时调度、事件推送消费者及自动重启策略也留待扩展。
