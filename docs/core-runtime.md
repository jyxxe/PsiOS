# 单节点宿主与 Agent Runtime

本页接续[本机 Core 启动指南](core-local.md)。服务只接受本机 Unix socket 上经过认证的请求。Task、Run、Operation、模型步骤、接收通知和用户操作都保留各自的主体和资源引用，可以通过同一套 Resource 与 Event 接口追查。

## 从 PR #1 升级

停服并按本机恢复要求保存 state 和 witness。新版首次打开旧库时核对数据库、见证和密钥，在一次带见证的事务中升级至格式 22。格式 20 增加多人房间，格式 21 登记网络探测，格式 22 登记资源压力观测。格式 17 持久化 Provider 队列，格式 18 登记备份发布观测工具，格式 19 登记会话准入工具。旧格式 1 还会迁移到多工作线程的资源和预算结构。格式 3 登记了网络接口观测，格式 4 增加备份核验。格式 5 给模型租约记录 Linux 启动身份和经过时间期限。格式 6 登记路由观测工具，仍需显式授权。格式 7 要求新恢复卷凭已记录的目录树摘要通过激活校验。升级前已经结算的模型回执可继续查询。从格式 4 或更早升级时，未结算的旧模型调用缺少启动证明，Provider 需要使用新的领取键并消耗新一轮预算。旧二进制拒绝打开超出自身支持范围的格式，软件回退需恢复升级前配对保存的 state 和 witness。离线 `psiosd verify` 报告是否需要升级，`check` 或启动服务会执行升级。已有 Agent 的权限、预算、凭据、取消墓碑、审批和任务身份按原记录保留。涉及绑定卷的新派发会检查卷版本。若旧应用启动计划缺少这个前提，需要提交新计划。

## 宿主与调度

- `psiosctl node` 返回本机权威分配的 Node 身份、Linux boot ID、工作线程数、各阶段队列数量和调度模式。`node.inventory`、`node.processes` 和 `node.devices` 是独立授权的 Task 工具。进程查询支持 `after_pid` 和 `limit`。boot ID、PID 和 start ticks 共同标识一次进程实例。块设备来自 sysfs，结果和进程页都是调用时的观测。
- 配置中的 `workers` 取 1 至 16，默认 4。工作线程共享事务权威，在数据库锁外等待宿主调用。调度器在有积压的主体之间轮转。同一卷、应用或服务上的冲突写入继续受持久占用保护。所有者用 `Node.Configure` 提交 Node 的 `expected_resource_version` 和 `scheduling`，取值为 `Cordoned` 或 `Enabled`。暂停领取期间可接受任务，在途工作可结算，超时及撤权仍会释放预留。
- `Task.History` 使用 `id` 和可选 `limit`。下一页带回 `through_version` 和服务器返回的 `next_after_version`（请求字段为 `after_version`），读取同一历史区间。`Task.Archive` 使用任务 ID 和修订，将已结算的终态移出调度表，保留幂等回执、历史、Run 和结果。当前仍需为结果及审计历史预留磁盘空间。
- `Event.Wait` 复用 Event.Poll 的授权游标，支持 1 至 10000 毫秒的 `timeout_ms` 和可选 `limit`。`psiosctl watch CURSOR --wait-ms 5000` 可直接使用。提交后通知订阅者，等待时释放数据库锁，撤权和超时重新检查。事件、任务、信箱和模型领取共用每个守护进程的 8 个等待名额。
- `Task.Wait` 接受 Task 的 `id`、1 至 10000 毫秒的 `timeout_ms` 和可选 `after_version`。传入此前读到的 `task_revision` 可等待新修订。省略时等待 `Succeeded`、`Failed`、`Cancelled`、`Paused` 或 `WaitingApproval`。到期返回最新快照。每次唤醒均复核 `Task.Get` 的同一读取权限，连接关闭会释放名额。CLI 和 Rust SDK 的完成等待使用此接口，等待名额用完时按退避间隔读取任务。

### 权威存储余量

`psiosctl node` 的 `authority_storage` 显示 state 和 witness 所在文件系统的容量、普通进程可用字节和 inode、配置阈值及观测时间。部署可在 Core 配置中分别设置 `authority_reserve.state_bytes` 与 `authority_reserve.witness_bytes`。默认值为零，仅观测余量。

```json
{"authority_reserve":{"state_bytes":16777216,"witness_bytes":2097152}}
```

任一已配置文件系统低于阈值时，新任务、新输入和新模型租约返回可退避重试的 `QuotaExceeded`。幂等重复请求仍返回原回执，已接受任务继续运行、结算或取消。压力期间的准入拒绝直接返回给调用方，节约原来的持久审计空间。容量采样用于准入，物理空间仍由宿主文件系统共享。可用空间恢复后，当前守护进程重新接受请求。[容量与任务回收测试](../tests/test_platform.py) · [模型租约结算测试](../tests/test_runtime.py)

### Linux 资源压力与守护进程 cgroup

向操作者授予目标 Node 上的 `node.pressure`、`task.submit` 和 `task.read`。用空对象提交 `node.pressure`，结果随 Task 持久保存。已存在的委托及所有者默认委托需要单独授予此能力。以下命令接续本机指南中的 `ctl` 函数。

```bash
node=$(ctl node | jq -er .reference.resource_id)
grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",actions:["task.submit","task.read","node.pressure"],
  tool_targets:[$node],ttl_seconds:300,units:8}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
task=$(echo '{}' | ctl submit node.pressure --delegation "$grant" --key pressure-observation-1 | jq -er .task_id)
ctl wait "$task" | jq '{reference:.result.artifact_refs[0]}' | ctl call Artifact.Read
```

`kernel` 记录内核可见的 CPU、内存和 I/O PSI、最近 10/60/300 秒的停顿比例、累计微秒及 OOM kill 计数。系统级 CPU PSI 只返回 `some`。`core_cgroup` 记录包含 `psiosd` 的 cgroup v2 的设备号、inode、各项 PSI、CPU 时间和节流、内存与交换用量、配置限额、OOM 事件及 PID 用量和限额。限额是该 cgroup 的本地控制值。计算继承后的有效上限还要检查父级。发起者无权另选 PID 或文件路径。[Linux PSI](https://docs.kernel.org/accounting/psi.html) · [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)

结果包含节点引用、boot ID、挂载与 cgroup 命名空间和 `CLOCK_BOOTTIME` 的采样起止点。各项按 `Observed`、`Unsupported`、`Denied` 或 `Unavailable` 报告。若复核发现守护进程已离开采样的 cgroup，则该组结果报告 `ChangedDuringObservation`。这些样本用于诊断。任务准入继续使用权威的配置阈值和预算。[真实限额、授权和内核降级验收](../tests/test_pressure.py) · [格式 21 升级](../tests/pressure_upgrade.py)

### 网络接口与地址

`node.interfaces` 通过 Task 返回本地网络命名空间中接口的 ifindex、名称、MTU、链路类型、运行状态、标志和已分配的 IPv4/IPv6 地址。地址含前缀长度和 scope，点对点配置另有 peer。结果同时记录 Linux boot ID、网络命名空间 inode 和观测时间。网卡驱动与地址由现有 Linux 网络管理器配置。

详细拓扑需要显式授权。以下命令接续本机指南中的 `ctl` 函数。为其他 Agent 颁发时替换 principal_id 并使用其凭据执行 Task。

```bash
node=$(ctl node | jq -er .reference.resource_id)
grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",actions:["task.submit","task.read","node.interfaces"],
  tool_targets:[$node],ttl_seconds:300,units:8}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
task=$(echo '{"limit":32}' | ctl submit node.interfaces --delegation "$grant" --key interface-page-1 | jq -er .task_id)
ctl wait "$task" | jq '{reference:.result.artifact_refs[0]}' | ctl call Artifact.Read
```

使用结果的 `next_after_ifindex` 作为下一次 Task 的 `after_ifindex`，并为新页提供新的幂等键。单页上限 128 个接口。本机 iproute2 后端最多读取 192 KiB 输出；缺少该命令或只有 BusyBox 时，Core 直接读取 Linux RTNETLINK 的接口与地址，每次内核转储上限 1 MiB。两个后端均有五秒期限，权限及结果字段一致。独立 UEFI 开发盘启动时启用环回接口，盘点可在无外部网卡和无 iproute2 时完成。超限或驱动不可用时返回错误。各页反映各次采样时的配置；接口热插拔后 index 可以复用。打包单元允许读取 netlink，服务以普通专用账户运行。

### 路由表观测

`node.routes` 在本机权威所在的网络命名空间直接读取 RTNETLINK。动态路由和管理员配置均由 Linux 路由表提供。可用 `family`（`inet` 或 `inet6`）和数字 `table` 过滤。省略两者时读取两种地址族的全部路由表。默认路由表示为 `0.0.0.0/0` 或 `::/0`。路由记录包含按 Linux 编号表示的 `kind`、`protocol` 和 `scope`，以及目标、源前缀、优先级 `metric`、首选源、网关、出口 ifindex、多下一跳及可选 `nexthop_id`。内核编号和 ifindex 适用于返回的 boot ID 和网络命名空间。下一跳对象细节与可达性验证仍待开发。[Linux RTNETLINK](https://docs.kernel.org/userspace-api/netlink/intro.html)

```bash
node=$(ctl node | jq -er .reference.resource_id)
grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",actions:["task.submit","task.read","node.routes"],
  tool_targets:[$node],ttl_seconds:300,units:8}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
task=$(echo '{"family":"inet","table":254,"limit":32}' | ctl submit node.routes --delegation "$grant" --key route-page-1 | jq -er .task_id)
ctl wait "$task" | jq '{reference:.result.artifact_refs[0]}' | ctl call Artifact.Read
```

`next_cursor` 可直接作为同一主体、委托和过滤条件下新 Task 的 `cursor`。每页重新核对排序后的可见路由摘要。结果变动时新 Task 以 `ResyncRequired` 结束，可从首页重新观测。已结算页作为原 Task 的制品保留。默认每页 32 条，上限 128 条和 128 KiB 路由数据。每种地址族的内核转储限 1 MiB，整个查询限时五秒。丢包、中断或超限时 Task 失败。两种地址族分别采样，结果适合作为诊断观察。[命名空间与变更测试](../tests/test_network.py)

### 策略路由规则

`node.routing_rules` 通过相同的 Task 和制品入口读取 Linux IPv4、IPv6 策略路由规则。向节点目标授予 `node.routing_rules`、`task.submit` 和 `task.read` 后，可用 `{"family":"inet","limit":32}` 提交查询。省略 `family` 时分别采样两个地址族，沿用路由表的分页、字节和时间限制。使用 `next_cursor` 读下一页，主体、委托、过滤条件、命名空间或规则变化时须从首页重查。[策略路由验收](../tests/test_network.py)

`order` 在每个地址族内从零开始，保留内核对同优先级规则的匹配顺序。`action` 和 `flags` 使用 Linux 编号，`table` 为 0 表示未指定固定表，`goto_priority` 表示跳转目标。`selectors` 包含地址前缀、接口名、mark 和掩码、隧道 ID、VRF 选择、UID 和端口范围及 IP 协议。UID 与端口范围为包含两端的 `[start,end]`，UID 按结果中的用户命名空间解释。`suppress_prefixlen` 为 0 时表示抑制默认路由。未解析的内核属性以类型和摘要标记在 `unsupported_attributes`，相关规则的解读覆盖已解析的字段。[Linux FIB 规则协议](https://docs.kernel.org/next/networking/netlink_spec/rt_rule.html)

### 实际选路查询

`node.route_lookup` 通过 `RTM_GETROUTE` 观察一条本机流量的当前选路。将该动作单独授予目标节点后，使用 `{"destination":"127.0.0.1"}` 查询。接受 IPv4 或 IPv6 数字地址，以及可选的同族 `source`、`fwmark`、`output_ifindex` 和 `uid`。端口字段 `source_port`、`destination_port` 与 `transport`（`tcp` 或 `udp`）一起使用。`lookup_uid` 记录用于选路的 UID，省略 `uid` 时使用守护进程 UID。[Linux 路由查询协议](https://docs.kernel.org/next/networking/netlink_spec/rt_route.html)

`Resolved` 返回 Linux 选中的路由表、出口 ifindex、网关和首选源。用 `route.kind` 识别本机目的，表编号保留内核回执。`KernelRejected` 保留内核 `errno`，例如没有可用路由或规则禁止本次查询。每次提交都会重新查内核，已结算结果可作为制品读取。查询在内核中完成，可达性需独立验证。[真实规则和路由变更验收](../tests/test_network.py)

### 核实已登记的网络端点

管理员在 Core 配置文件中加入 `network_probes`，重启后按 `Resource.List` 的 `kind: NetworkProbe` 获取资源引用。

```json
{"network_probes":[{"name":"registry","endpoint":"192.0.2.10:443","timeout_ms":2000,
  "dns":{"resolver":"192.0.2.53:53","name":"registry.example."}}]}
```

将 `node.network_probe`、`task.submit` 和 `task.read` 授给操作者，`tool_targets` 设为对应端点资源 ID。使用 `NetworkProbe.Get` 查询获准目标配置。通过 `Task.Prepare` 和 `Task.Submit` 提交 `node.network_probe`，输入仅含 `id` 和 `expected_resource_version`。结果随 Task 保存。注册配置最多 32 项，使用数字 IPv4 或 IPv6 端点。IPv6 链路本地地址须带该命名空间的数字 scope ID。

Core 可选地先向该目标指定的解析器发出 TCP DNS 请求。按端点家族查询 A 或 AAAA，在回复内核对有界 CNAME 链，并要求最终地址集合与配置的单个地址一致。回复不匹配、不可解析或超限时，端点连接不会发出。TCP 使用固定地址和端口，仅建立连接。发给解析器的内容只有 DNS 查询。两步共享 50 至 5000 毫秒的期限，默认 2000 毫秒。

制品包含目标引用、启动 ID、网络命名空间和观测时间，并分别记录解析与连接结果。Connected 的范围是指定 TCP 端点。连接被拒绝或只有有效 DNS 回复时报告 Limited。没有这两类响应时报告 Offline。DNS 解析器的记录和 TCP 连通性证据不能证明应用层身份。改变端点、解析器或 DNS 名称会注销原资源并生成新授权目标。仅改超时保留身份并更新修订。队列中的旧目标请求按原引用复核。[TCP 与 DNS 进程验收](../tests/test_network_probe.py) · [格式 20 升级](../tests/network_probe_upgrade.py) · [Hickory DNS 协议库](https://docs.rs/hickory-proto/0.26.3/hickory_proto/)

### 受管 systemd 服务

运维人员在本机配置中显式加入 `services`。先使用观察配置并重启守护进程。

```json
{"services":[{"unit":"example.service","manager":"user","allow_control":false}]}
```

这段是配置文件中的一个字段。`manager` 支持 `user` 和 `system`。用 `service.inspect` 查询 `service-example.service` 的当前资源版本，输入为 `id` 和 `expected_resource_version`。确认服务定义后，把返回的 `definition_digest` 写入配置项，并按需开启 `allow_control`。摘要涵盖 `systemctl cat` 返回的 unit 和 drop-in 定义。服务账户必须具备对应 manager 的访问权，控制操作沿用 Linux 的权限。

`service.start`、`service.stop` 经过审批、目标占用、定义摘要和 manager 加载状态检查。已派发后丢失回执时，先执行新的 `service.inspect` Task，再向 `Operation.Reconcile` 提交 Operation 的 ID、修订和新的 `evidence_ref`。服务会核对证据来源、调度先后、受管目标、定义和期望状态。系统记录可观察到的结果。具有宿主管理权的其他程序也可以改变该 unit，外部 unit 依赖和程序包变更属于宿主运维的信任边界。[systemctl 官方手册](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html)

## 卷文件与应用版本

| Task 工具 | 参数与行为 |
| --- | --- |
| volume.files | `target` 为卷的 ID 和预期修订。`path` 默认为根，`after` 和 `limit` 用于按名称翻页，最多 128 项。 |
| volume.mkdir、volume.remove_entry | `target, path`。创建目录，或删除单个普通文件及空目录。 |
| volume.append | `target, path, offset, data_base64`。偏移为零时排他新建，后续追加要求实际长度等于偏移。每块最多 96 KiB，单文件上限 256 MiB。沿用主体的 Task 输入保留额度。 |
| volume.read_chunk | `target, path, offset, limit`。取回 Base64 数据、块摘要、下一偏移和 EOF。可以从待激活的恢复卷读取。 |
| application.stage_release | `target` 指应用，`release` 使用安装参数，省略 volume。依据本机 OCI 摘要生成属于同一应用的清单和不可变 Release，保留当前部署。 |
| application.select_release | `target, release_ref`。应用停止时经审批切换至选定 Release，生成新部署。选回已有版本使用相同接口。卷数据世代保持连续，应用层数据库迁移须另外协调。 |
| application.refresh | 应用 ID 和预期修订。把引擎观测写回 Application 和 RuntimeInstance。进程意外退出时记录 Exited 和 NotReady，并保留现场及数据占用，直到显式停止清理。 |

多段路径通过 Linux openat2 在已核实的卷根目录内解析。符号链接与嵌套挂载不能作为中间路径。追加和目录修改需要批准。未知追加可以用 `Operation.Reconcile` 核对原卷、预期长度和原始字节范围。应用启动、停止会把应用与绑定卷同时列入计划前提和 ExecutionPermit。还原卷会检查目标 StoragePool 的版本和分配状态。[Linux openat2 手册](https://man7.org/linux/man-pages/man2/openat2.2.html)

新启动的容器会记录 Docker Engine ID。如果停止操作在引擎删除容器后丢失回执，应用和卷仍保留占用。运行新的 `application.health` Task，由同一引擎确认原容器及该应用的受管实例均已消失。把该检查的 ArtifactRef 作为 `evidence_ref` 提交至 `Operation.Reconcile`，服务才会一并结算停止操作、运行实例和卷解绑。引擎不可达、引擎身份变化、容器仍在或使用旧版缺少引擎身份的记录时，都无法用缺失观测释放卷。
启动流程会在核验容器配置后、请求 Docker 启动前，将引擎 ID、容器 ID、原 Operation 和应用、卷修订存入带见证的检查点。Core 在派发前和收到启动回执后检查 Docker 报告的 CPU、内存、禁用 swap、PID 和临时目录限额，同时核对只读根、独立命名空间、无额外设备与特权，以及获准的卷绑定。原生临时目录 `/tmp` 带有 `noexec`、`nosuid` 和 `nodev`。

使用本机 cgroup v2 时，针对运行中的容器，Core 还会核对宿主 PID 所属的 Docker cgroup，从内核读取 CPU 配额与 burst、内存、swap 和进程数限额。`effective_limits.source` 为 `LinuxCgroupV2` 时，观测附带实际限额及 cgroup inode。旧 cgroup v1 配置沿用引擎检查，观测来源为 `DockerCgroupV1`。[Linux cgroup v2 控制项](https://docs.kernel.org/admin-guide/cgroup-v2.html)

若启动回执丢失，先运行新的 `application.health`，再向 `Operation.Reconcile` 提交该 ArtifactRef。同一引擎须确认原容器仍在运行，镜像、隔离配置和限额观测符合获准范围，Core 才会登记运行实例、挂载卷并结算原任务。容器只完成创建、已经退出、配置或读出的限额不符、引擎不可达，或中断发生在检查点写入之前时，原操作继续保留占用。旧版生成的观测须用新检查刷新后再提交对账。[真实 Docker 对账验收](../tests/oci_recovery.py)

### 核实备份发布回执

本地备份在持有原卷期间写入私有暂存目录。发布前重新检查执行许可，将卷引用、Operation 和清单摘要及暂存目录身份一起提交至带见证的权威，然后以原子重命名发布备份。新协议下若在授权发布前崩溃，原 Task 以已知失败结算并释放卷。存在发布检查点而原回执丢失时，原 Task 暂停并保留卷占用。

给恢复主体授予限定到原 Operation 的 `backup.inspect_publication`、`task.submit` 和 `task.read`。对未知 Operation 的当前修订运行该观测 Task。它核实最终目录与原暂存目录同一身份，检查清单及全部数据摘要，并同步备份所在目录。管理员将新制品的引用作为 `evidence_ref` 提交给 `Operation.Reconcile`。Core 复核证据与当前目录身份，在一个事务中登记原 Backup，保存原 Task 回执并释放卷。缺少检查点、最终目录不可达或数据校验失败时仍保留未知结果。[强杀测试](../crates/psios-core/src/recovery_tests.rs) · [截断复制测试](../tests/test_storage_recovery.py)

## 身份与权限

`Grant.Issue` 可提供 `tool_targets`，把工具限制在明确的资源 ID 集合中。对已绑定卷的应用启停，授权需覆盖应用和卷。通用资源读取仍按独立权限和资源所有者裁决。设置 `allow_delegation=true` 和 `max_delegation_depth` 可启用最多 4 层转授。子委托可以收窄动作、目标、期限和剩余层数，共享根预算。沿用旧配置的 Grant 保持原有的单层状态委托范围。每次派发检查完整祖先链及其主体。

`Secret.Rotate` 接受秘密 ID、预期修订和新值。新密文使用新随机 nonce，并将秘密身份及密钥版本纳入认证数据。轮换前接受且尚未派发的旧计划会失效。旧库中的密文可以继续使用并原地轮换。撤销的秘密不能通过轮换重新激活。

## 任务调度

`Schedule.Create` 为本机 Task 保存一个未来触发时间和截止时间。两者接受带时区偏移的 RFC 3339 时间并转为 UTC，触发依据节点墙钟。授予 `schedule.manage`、`task.submit`、`task.read` 及选定工具，并将权限限制在该工具的目标资源。截止时间须在委托有效期内。同一主体以相同 `key` 重发等价请求会取回原调度状态。输入、授权或时间改变时需要新键。

```bash
grant=$(printf '%s' '{"principal_id":"local-owner","actions":["schedule.manage","task.submit","task.read","system.status"],
  "tool_targets":["profile-local"],"ttl_seconds":900,"units":1}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
run_at=$(date -u -d '+1 minute' +'%Y-%m-%dT%H:%M:%SZ')
deadline_at=$(date -u -d '+10 minutes' +'%Y-%m-%dT%H:%M:%SZ')
schedule=$(jq -n --arg grant "$grant" --arg at "$run_at" --arg until "$deadline_at" \
  '{key:"check-once",tool:"system.status",input:{},delegation_id:$grant,run_at:$at,deadline_at:$until}' \
  | ctl call Schedule.Create | jq -er .reference.resource_id)
jq -n --arg id "$schedule" '{id:$id}' | ctl call Schedule.Get
```

在截止前重启、解除封锁或腾出队列空间时，Core 会用原 `trigger_id` 提交一次 Task。Task、预算预留和触发回执原子提交。派发仍按照普通 Task 重新授权和审批。队列满或权威存储低于预留空间时，触发继续等待。截止后记录 `Expired`，撤权或目标修订失效时记录 `Rejected`。`Schedule.Cancel` 接受 ID 和 `expected_resource_version`，用于取消待触发的调度。`Triggered` 包含可用 `Task.Get` 或 `ctl wait` 跟踪的 Task 回执，已经触发的 Task 使用原有取消入口。[调度故障与权限验收](../tests/test_schedule.py)

在 `Schedule.Create` 请求中加入 `repeat` 可创建有限次的 UTC 固定间隔调度。例如每 60 秒运行一次，共运行三次，并在恢复时最多保留两次可执行的触发。

```json
{"interval_seconds":60,"count":3,"missed":{"mode":"BoundedCatchUp","limit":2},"overlap":"Skip"}
```

间隔为 1 至 86400 秒，触发次数为 2 至 32。每次触发时间和截止时间都以首轮输入为基准，按间隔顺延。末轮截止时间须在委托和 Grant 的共同有效期内。`Skip` 在积压时只保留最新的到期触发。`BoundedCatchUp` 保留最新的至多 8 个到期触发，具体数量由 `limit` 指定。这两种策略都会先记录超出自身截止时间的触发。`overlap` 可取 `Allow` 或 `Skip`。取 `Skip` 时，上一项 Task 尚未结算或结果知识为 `Unknown` 会跳过本轮。

进行中的调度通过 `Schedule.Get` 返回 `next` 和已结算的 `trigger_refs`。同一逻辑时刻的触发 ID 与调度定义绑定，重启后继续使用。对引用中的 ID 调用 `Schedule.TriggerGet` 可读取该轮的状态、原因及 Task 回执，读取权限限于调度所有者与本机管理员。序列用尽后返回 `Completed`。每轮分别记录已触发、已过期或已跳过。遇到永久的准入拒绝时记录该轮拒绝并结束序列。取消仍需当前修订，取消后的已提交任务可以继续用原入口管理。[格式 10 升级](../tests/schedule_repeat_upgrade.py)

`Schedule.Create` 也可用 `calendar` 提交有限期的当地日历规则。日历请求省略 `run_at`、`deadline_at` 和 `repeat`。以下规则从 2027 年 11 月 7 日起每天当地 01:30 触发，持续两天。

```json
{"timezone":"America/New_York","start_date":"2027-11-07","local_time":"01:30:00","days":2,
 "window_seconds":120,"fold":"Both","gap":"Skip","missed":{"mode":"BoundedCatchUp","limit":2},"overlap":"Skip"}
```

每份定义覆盖 1 至 30 个当地日期，最多产生 32 个 UTC 触发。`fold` 可取 `Earlier`、`Later` 或 `Both`，用于选择夏令时回拨时的一个或两个实际时刻。`gap` 可取 `Skip` 或 `Reject`，用于跳过缺失时刻或拒绝整份定义。`window_seconds` 以实际 UTC 时长计算，支持 1 至 86400 秒。委托及 Grant 都须覆盖最晚截止时间。

Core 在创建时固定 IANA 数据版本、UTC 窗口和因 `gap` 跳过的当地日期。`Schedule.Get` 的 `calendar_plan` 可查询这些值。服务重启或更新时区数据库后，既有定义继续采用原窗口和触发 ID。更改规则时取消仍待触发的旧定义，并使用新键创建新定义。`missed`、`overlap`、重试和 Task 回执遵守上述序列规则。[夏令时 UTC 对照与断电验收](../tests/native_calendar.py) · [格式 13 升级](../tests/calendar_upgrade.py)

### 通过调度创建 Agent 会话

将 `agent.session.start` 作为 `Schedule.Create` 的 `tool`。输入包括 `goal`、完整的 `skill_ref` 和 `model_ref`、`context_policy` 及 `max_turns`。所选委托需要包含 `schedule.manage`、`agent.session.start`、`session.run`、`task.submit`、`task.read` 及 Skill 所需工具。启用资源范围时，计划同时核对指定的 Model 和 Skill，后续工具依然各自核对资源。也可通过普通 Task.Submit 接受同一个工具。

Task 成功时返回 `session_ref`，会话记录原 Task 引用。会话进入选定 Model 的持久队列，可由正在等待的 Provider 接续。系统一次提交会话、Task 回执和准入用量，后续模型调用及工具沿用同一根预算。调度时间窗约束准入。已经创建的会话仍通过 `Session.Cancel` 独立管理，周期调度的重叠判定跟踪准入 Task，模型并发由 Model 和根预算约束。该准入工具不能登记为模型 Skill 的工具依赖。[故障与权限验收](../tests/test_model_queue.py)

## 本机 PsiFlow

`Workflow.Define` 登记一份不可变的本机 Task 依赖图。每步提供 `id`、`tool`、字面量 `input` 和所依赖的 `after` 步骤 ID。当前支持 1 至 16 步，定义体最多 128 KiB。Core 检查重复 ID、悬空引用与回环，并固定当时登记的工具引用。以同一键重发相同定义会得到原引用。修改定义时使用新键。

`Workflow.Start` 接受 `key`、`definition_ref`、`delegation_id` 和带偏移的 `deadline_at`。委托须授予 `workflow.manage`、`task.submit`、`task.read` 和各步所需工具，覆盖实际目标，且在最终截止时间内有效。每步在派发时重查原授权、预算、工具引用和目标版本。Workflow 串行释放已满足依赖的步骤，由普通 Task 执行与审批。

```bash
node=$(ctl node | jq -er .reference.resource_id)
grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",tool_targets:["profile-local",$node],
  actions:["workflow.manage","task.submit","task.read","system.status","node.inventory"],units:2,ttl_seconds:900}' \
  | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
definition=$(printf '%s' '{"key":"inventory-flow","steps":[
  {"id":"status","tool":"system.status","input":{},"after":[]},
  {"id":"inventory","tool":"node.inventory","input":{},"after":["status"]}]}' \
  | ctl call Workflow.Define | jq -c .reference)
jq -n --argjson definition "$definition" --arg grant "$grant" --arg until "$(date -u -d '+10 minutes' +'%Y-%m-%dT%H:%M:%SZ')" \
  '{key:"inventory-run-1",definition_ref:$definition,delegation_id:$grant,deadline_at:$until}' | ctl call Workflow.Start
```

`Workflow.Get` 返回运行修订、定义引用、截止时间，以及各步骤的 Task 回执和已观察修订。后继步骤只在全部前驱已知成功时进入准入。前驱出现未知效果时保留 `Blocked/Unknown`，原 Operation 完成对账后继续。用 `Workflow.Cancel` 提交 ID 和当前修订可关闭派发并取消在途 Task，已经发生的效果仍可从原 Task 和 Operation 查阅。[审批、撤权和队列验收](../tests/test_workflow.py) · [升级验收](../tests/workflow_upgrade.py)

## 本机会话

### 私聊

通过 `Conversation.OpenDirect` 提交 `key`、`name`、`recipient_principal_id` 和邀请 `ttl_seconds`。发起者需要 `chat.invite` 与 `chat.read`，受邀主体可用 `Conversation.List` 查找邀请，再以 `Conversation.Respond` 提交会话 ID、`expected_resource_version` 和 `accept`。一份会话固定两位主体，同一主体对可创建不同名称的多个会话。

双方加入后，发送者用 `Message.OpenSendWindow` 获取本会话和主体的 `namespace`。`Message.Prepare` 接受 `conversation_id`、形如 `namespace:client-key` 的 `client_send_id`、`text`，以及可选的同会话 `reply_to_message_id`。将生成的请求原样提交 `Message.Send`，Core 按 v1 摘要校验，在一个权威事务内提交消息及发送回执。有效窗口内，同键同请求重发返回原消息 ID 和序号。同键内容改变会冲突。窗口绑定实际授权发送者读写和接收者读取的委托。无关授权变化保留原窗口。窗口到期、相关委托失效，或恢复涉及会话成员的本机所有者凭据后，旧命名空间会被拒绝。

`Message.List` 的 cursor 绑定当前主体、会话、成员修订和授权代次。客户端可用 `Message.AckRead` 单调确认该游标处的已读位点。`Message.Get` 以 `conversation_id`、`message_id` 和可选 `content_ref` 查询固定正文。对邀请、正文或历史的读取均按两位成员和实际 `chat.read` 裁决。成员会话记录使用独立的有序拉取接口。会话所有者可按当前修订归档，双方保留有权读取的历史。

当前每条消息包含至多 4096 个 UTF-8 字节，每页最多 32 条。会话最多保留 1024 条消息，节点最多保留 16384 条。权威容量闸门仍保护状态盘的提交余量。回执的持久性范围为本机进程故障。[私聊规范](v1/docs/10-conversations.md) · [成员、重发和故障验收](../tests/test_chat.py) · [格式 12 升级](../tests/chat_upgrade.py)

### 本机多人房间

`Conversation.OpenRoom` 用 `key`、`name`、`capacity` 和 `ttl_seconds` 创建 `AdHocRoom`。容量含拥有者，范围为 2 至 16。拥有者提交 `Conversation.Invite`，参数为会话 ID、`expected_resource_version`、`principal_id` 和邀请 `ttl_seconds`。响应包含更新后的 `conversation` 和本次 `membership_id`。受邀者用 `Conversation.Respond` 条件提交会话 ID、成员 ID、当前会话修订与 `accept`。邀请需要主动接受。私聊的两位主体保持固定。

拥有者可用 `Conversation.RemoveMember` 提交会话 ID、当前修订和成员 ID。其他活跃成员可用 `Conversation.Leave` 主动退出。移除、退出或主体撤销立即阻断后续读取和发送。拥有者撤销会归档其房间。到期或归档后停止写入，活跃成员仍可读取被授予的历史。再次邀请会签发新成员 ID。

`Conversation.TransferOwner` 接受会话 ID、当前修订和接任者的活跃成员 ID。接任者须持有该房间的 `chat.invite` 和 `chat.read`。交接只变更管理角色，保留每位成员各自的历史起点。原拥有者成为普通成员，可主动退出。手动归档会注销尚未接受的邀请和发送窗口。拥有者使用 `Conversation.Reopen` 及当前会话修订可恢复尚未到期的房间，后续发送使用新窗口。已接受的历史及可用的读位点继续保留。

房间使用与私聊相同的 `Message.Prepare`、`Message.Send`、读取与已读确认接口。历史政策固定为 `FromJoin`。接受邀请时记录当时的消息序号。之后仅能读取加入后的消息，指向此前正文的回复链接对该成员省略。发送窗口、历史游标和已读位点绑定成员实例。消息提交、成员变化和撤权都由同一个 Core 写事务裁决，消息回执记录提交时的成员修订和写者代次。

房间容量计算包括未到期的待接受邀请。节点限制会话总量、成员记录和保留正文。群聊当前提供本机持久文本、明确邀请和拉取，后续接入线程、订阅投递、附件与 Agent 唤醒。[群聊权限、并发与恢复](../tests/test_chat_rooms.py) · [格式 19 原地升级](../tests/rooms_upgrade.py)

## 运行一个持久的 Agent 会话

以下示例接续启动指南中的 `ctl` 函数和 `workdir`。模型提供者和 Agent 是两个独立凭据。模型提供者只获得处理模型步骤的权限。

```bash
umask 077
printf '{"name":"surveyor"}' | ctl call Agent.Register > "$workdir/surveyor.json"
printf '{"name":"local-provider"}' | ctl call Agent.Register > "$workdir/provider.json"
jq -r .token "$workdir/surveyor.json" > "$workdir/surveyor.token"
jq -r .token "$workdir/provider.json" > "$workdir/provider.token"
jq '{principal_id,actions:["task.submit","task.read","node.inventory","node.devices","session.run"],units:2,model_calls:3,ttl_seconds:600}' \
  "$workdir/surveyor.json" | ctl call Grant.Issue > "$workdir/grant.json"
jq '{principal_id,actions:["model.serve"],units:1,ttl_seconds:600}' \
  "$workdir/provider.json" | ctl call Grant.Issue > /dev/null
printf '{"name":"node-survey","version":"1","license":"0BSD","source":"local","instructions":"Read node inventory and count block devices.","tools":["node.inventory","node.devices"]}' \
  | ctl call Skill.Publish > "$workdir/skill.json"
jq '{name:"local-survey",revision:"1",provider_principal_id:.principal_id,max_context_bytes:131072,max_output_bytes:8192,lease_seconds:30}' \
  "$workdir/provider.json" | ctl call Model.Register > "$workdir/model.json"
agentctl() { target/debug/psiosctl --socket "$workdir/core/api.sock" --token-file "$workdir/surveyor.token" "$@"; }
providerctl() { target/debug/psiosctl --socket "$workdir/core/api.sock" --token-file "$workdir/provider.token" "$@"; }
session=$(jq -n --slurpfile g "$workdir/grant.json" --slurpfile s "$workdir/skill.json" --slurpfile m "$workdir/model.json" \
  '{key:"survey-1",goal:"Collect node inventory",skill_ref:$s[0],model_ref:$m[0],delegation_id:$g[0].delegation_ref.resource_id,context_policy:"LocalToolResults",max_turns:3}' \
  | agentctl call Session.Open | jq -r .reference.resource_id)
for turn in inventory devices finish; do
  receipt=$(providerctl model-step "$session" --key "$turn") || break
  task=$(jq -r '.task_id // empty' <<< "$receipt")
  if [ -n "$task" ]; then agentctl wait "$task" > /dev/null; fi
done
jq -n --arg id "$session" '{id:$id}' | agentctl call Session.Get
```

`SessionModel` 是 Rust Provider 接口。其他语言可以用同一协议实现 `Model.Claim` 和 `Model.Respond`，提交 `tool`、`finish` 或 `fail` 决策。Model 登记可用 `max_in_flight` 设置 1 至 32 个并发租约，默认 4。每次签发租约计入会话次数和共享的根 `model_calls` 预算。当 Claim 返回 `usage_format: "tokens.v1"` 时，Provider 可以随决策提交 `usage`，其中 `input_tokens` 和 `output_tokens` 各为 0 至 4294967295 的整数。Core 在接受决策的同一事务中更新本次调用原有的 ModelUsage，回执的 `usage_ref` 固定该修订。相同租约只能重放原决策和原用量。缺少报告或租约被围栏隔离时，实际 Token 数保持未知。Token 数由 Provider 声明；当前可强制执行的预算按调用次数计算，耗时和供应商账单待实现。当前传输是经过本机认证的 Provider 进程，Provider 的网络隔离由部署管理。Core 没有配置自动云端回退。

模型租约在签发时绑定本节点的 Linux boot ID 和 `CLOCK_BOOTTIME`。同次系统启动中，服务重启保留原期限，系统挂起的时间计入租期。调快或调慢 UTC 时钟不会改变处理当前租约的权限。宿主重启后，旧租约的响应会被拒绝。API 的 `expires_at` 是供客户端显示的 UTC 估算。已取消但 Provider 尚未交回结果的调用，仍占用并发名额至原期限。上一启动或旧格式的调用按原 UTC 估算保留剩余名额，用于记录可能尚在外部执行的工作。绝对授权到期仍按现有 UTC 策略判断。[格式迁移测试](../tests/model_leases.py) · [虚机调时与重启测试](../tests/qemu_boot.py)

`ReferencesOnly` 给模型上一步的结果引用，`LocalToolResults` 允许将授权结果正文交给指定本机 Provider。两种模式都核对当前权限与大小。Session 固定 Skill、工具和 Model 修订。退役依赖会阻止后续派发。重复决策返回原回执，租约被取代后旧决策被拒绝。SDK 遇到已领取但结论不明的模型调用时要求保留决策或等待新租约。工具提交、会话检查点和预算更新使用同一事务。

Provider 可用 Rust SDK 的 `model_turn_durable`，或运行 `psiosctl model-step SESSION --key KEY --decision-dir DIR`。为每个并发工作进程预建独占的服务账户目录（模式 0700），使用支持目录锁、原子重命名和 fsync 的本机文件系统。SDK 在发出决策前将原节点、主体、会话、键、租约和结构化决策写入私有文件并落盘。文件至多 72 KiB，可能包含私有工具输入。重启后用原目录和参数重发已保存的决策，Core 可返回原 Task 回执。确认结算或收到该租约的 `Fenced` 后释放目录。同一目录有未决记录时，其他请求会得到 `Conflict`。

如果推理已经开始、决策尚未持久化，重启返回 `OutcomeUnknown`。损坏记录、授权被撤销及冲突决策会留待核实。需保留这类记录时，停用该工作进程并隔离其私有目录；新键仍受原租约、预算和权限约束。[进程故障验收](../tests/test_runtime.py)


本机 CPU 模型可通过 UID 绑定的 Unix socket 生成结构化盘点决策，复用同一 Provider 身份、租约和决策暂存目录。[本机推理配置与范围](local-inference.md)

`Session.History` 返回模型轮次、子任务和尝试关系。`Session.Cancel` 关闭后续派发并取消已链接的任务。`Session.Extend` 通过 CAS 增加本次会话的轮次上限，根授权额度独立生效。对于可安全重试的失败子任务，`Session.RetryTask` 使用会话 ID、会话预期修订、原 `task_id` 和新 `key`，沿用同一模型决策，在原任务和会话历史中追加记录。未知写入继续占用原目标，等待核实。

### 持久的 Provider 队列

调用 `Session.Open` 时显式提供 `"provider_queue":true`，允许选定 Model 的 Provider 自动领取该会话。Provider 可用独立凭据和上述 `model.json` 运行以下命令。省略 `--once` 时，进程持续等待并处理可领取的轮次。

```bash
install -d -m 0700 "$workdir/worker-decisions"
providerctl model-worker "$workdir/model.json" --decision-dir "$workdir/worker-decisions"
```

API 的 `Model.ClaimNext` 接受 `model_ref` 和 `key`。`Model.WaitNext` 另接受 1 至 10000 毫秒的 `timeout_ms`。返回 `available:false` 表示暂无可领取轮次。领取按进入就绪状态的顺序执行，等待工具或人工审批期间跳过该会话。每个 Model 最多接纳 128 个自动领取会话，沿用单次租约并发限制、会话轮数和共同根预算。撤权、依赖变更或额度耗尽的会话进入带原因的暂停状态，其他会话仍可领取。

系统在签发租约和计入预算的同一事务中保存 Model 与领取键映射。领取请求重放只能恢复该轮次。只有已落盘的决策可重发，丢失决策的轮次等到原租约失效后才可申请新的额度。Rust SDK 的 `model_next_durable` 先尝试交付已保存的决策，再领取新轮次。若原会话授权已撤销，`Model.ClaimStatus` 通过 Model、领取键和租约 ID 返回该次交付的 `Pending`、`Settled` 或 `Fenced`。该方法只返回状态，任务回执和上下文仍受原会话授权约束。每个工作进程需要独占的暂存目录。[竞争与崩溃验收](../tests/test_model_queue.py)

## 定向通知与验证

接收方以 `mail.receive` 调用 `Mailbox.Bind`，指定 `name` 和 `sender_principal_id`。指定的发送方以 `mail.send` 调用 `Signal.Send`，提供 `mailbox_id, key, topic` 和可选 `resource_ref`。当前 topic 为 ObservationReady、ReviewRequested、WorkAvailable。附带的资源必须已经对双方可见。接收方用 `Mailbox.Read` 读取，用 `Mailbox.Ack` 的 `previous, through` 确认；崩溃后可重读。会话创建可带 `trigger_signal_ref`，同一信号最多触发一个会话。每个信箱最多有 256 条未确认通知，历史上限 16384 条。

常驻 Agent 可用 `psiosctl inbox MAILBOX --wait-ms 5000 --limit 16` 等待投递。省略 `--wait-ms` 即刻读取。API `Mailbox.Wait` 接受 `id`、1 至 10000 毫秒的 `timeout_ms` 和可选的 `limit`（1 至 128）。返回值包含 `items`、`acked`、`lifecycle` 和 `mailbox_ref`。处理完一页再用该页的 `acked` 作 `previous`，最后一项的 `signal.seq` 作 `through` 确认。接收端在确认前退出，可在重启后重新投递同一页。空的 Open 页代表超时，Closed 信箱可以继续读取积压的信号，直到返回空页。阻塞期间的撤权会终止本次等待。等待名额已满时返回 `QueueFull`。

CI 同时执行原有契约、故障与 API 测试，以及持久会话、授权链、真实 systemd、从旧二进制升级和两个 OCI 镜像间的切换回退。在线多卷应用一致性、异机备份、多节点 fencing、整机断电、跨主机 Provider 及云模型账单仍需对应驱动与环境。[平台测试](../tests/test_platform.py) · [Agent 测试](../tests/test_runtime.py) · [systemd 故障测试](../tests/services.py) · [升级测试](../tests/upgrade.py) · [OCI 测试](../tests/oci.py)
