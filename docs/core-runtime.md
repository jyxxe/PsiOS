# 单节点宿主与 Agent Runtime

本页接续[本机 Core 启动指南](core-local.md)。服务只接受本机 Unix socket 上经过认证的请求。Task、Run、Operation、模型步骤、接收通知和用户操作都保留各自的主体和资源引用，可以通过同一套 Resource 与 Event 接口追查。

## 从 PR #1 升级

停服并按本机恢复要求保存 state 和 witness。新版首次打开旧库时核对数据库、见证和密钥，在一次带见证的事务中升级至当前版本支持的最新格式。格式 24 持久化工作流条件选择和汇合证据，格式 25 为新授权记录经过时间，格式 26 将同一时限机制用于新任务与执行许可，格式 27 将它用于工作流准入与步骤期限，格式 28 增加定时触发窗口的经过时间限制，格式 29 支持固化任务制品输入。格式 20 增加多人房间，格式 21 登记网络探测，格式 22 登记资源压力观测，格式 23 登记块设备布局核对。格式 17 持久化 Provider 队列，格式 18 登记备份发布观测工具，格式 19 登记会话准入工具。旧格式 1 还会迁移到多工作线程的资源和预算结构。格式 3 登记了网络接口观测，格式 4 增加备份核验。格式 5 给模型租约记录 Linux 启动身份和经过时间期限。格式 6 登记路由观测工具，仍需显式授权。格式 7 要求新恢复卷凭已记录的目录树摘要通过激活校验。升级前已经结算的模型回执可继续查询。从格式 4 或更早升级时，未结算的旧模型调用缺少启动证明，Provider 需要使用新的领取键并消耗新一轮预算。旧二进制拒绝打开超出自身支持范围的格式，软件回退需恢复升级前配对保存的 state 和 witness。离线 `psiosd verify` 报告是否需要升级，`check` 或启动服务会执行升级。已有 Agent 的权限、预算、凭据、取消墓碑、审批和任务身份按原记录保留。涉及绑定卷的新派发会检查卷版本。若旧应用启动计划缺少这个前提，需要提交新计划。

## 宿主与调度

- `psiosctl node` 返回本机权威分配的 Node 身份、Linux boot ID、工作线程数、各阶段队列数量和调度模式。`node.inventory`、`node.processes` 和 `node.devices` 是独立授权的 Task 工具。进程列表支持 `after_pid` 和 `limit`。块设备来自 sysfs，结果和进程页都是调用时的观测。
- `node.processes` 每项返回由 Node、boot ID、procfs 视图、PID 和 start ticks 组成的 `process_ref`。将这份引用原样用于 `node.process_inspect` 可取得该实例的状态、线程数、CPU ticks、虚拟内存和驻留页。需要单独授予 `node.process_inspect`，并将 `tool_targets` 限定到 Node。退出或不可见的进程返回 `NotFound`，身份或视图变化返回 `PreconditionFailed`。[本机进程验收](../tests/test_process_observation.py) · [Linux procfs](https://docs.kernel.org/filesystems/proc.html)
- 配置中的 `workers` 取 1 至 16，默认 4。工作线程共享事务权威，在数据库锁外等待宿主调用。调度器在有积压的主体之间轮转。同一卷、应用或服务上的冲突写入继续受持久占用保护。所有者用 `Node.Configure` 提交 Node 的 `expected_resource_version` 和 `scheduling`，取值为 `Cordoned` 或 `Enabled`。暂停领取期间可接受任务，在途工作可结算，超时及撤权仍会释放预留。
- 状态和见证均提交后，守护进程会唤醒一个空闲 Task worker。领取成功也触发同一通知，让其他 worker 继续领取。`tick_ms` 保留为没有提交通知时处理时限的后备间隔。关闭时唤醒全部空闲线程。[派发与重启验收](../tests/test_task_wait.py)
- `Task.History` 使用 `id` 和可选 `limit`。下一页带回 `through_version` 和服务器返回的 `next_after_version`（请求字段为 `after_version`），读取同一历史区间。每个修订包含当时的 `result_availability`。`Task.Archive` 使用任务 ID 和修订，将已结算的终态移出调度表，保留幂等回执、历史、Run 和结果。当前仍需为结果及审计历史预留磁盘空间。
- `Event.Wait` 复用 Event.Poll 的授权游标，支持 1 至 10000 毫秒的 `timeout_ms` 和可选 `limit`。`psiosctl watch CURSOR --wait-ms 5000` 可直接使用。提交后通知订阅者，等待时释放数据库锁，撤权和超时重新检查。事件、任务、信箱和模型领取共用每个守护进程的 8 个挂起名额。本机所有者可使用全部名额，其他主体各有 2 个名额。Core 先执行授权查询，已有结果或可立即领取的模型租约直接返回。确需挂起且额度不足时返回 `QueueFull`，连接关闭后释放名额。[主体隔离与就绪回执验收](../tests/test_task_wait.py)
- `Resource.List` 可同时按 `kind` 和精确 `id` 取得固定快照。CLI 使用 `psiosctl list Node --id NODE_ID`，SDK 使用 `Client::resource_snapshot`。将返回的 `watch_start_cursor` 传给 `Resource.Watch`、`Client::watch_resources` 或 `psiosctl watch`，只接收该筛选范围内可读的事件。可选 `timeout_ms` 启用等待。`Event.Ack` 为同一消费者保留筛选范围，`CursorExpired` 时重新取得快照。格式 44 按资源、类型及所有者索引修订序列，按源序合并主体与系统的可见事件。原有有效游标可跨升级继续投递。[确切资源与撤权验收](../tests/test_resource_watch.py) · [已确认游标升级](../tests/indexed_events_upgrade.py)
- 格式 73 为超过游标长度限制的精确筛选保存短期作用域。每个主体最多同时保留 64 个、每个权威最多 2048 个，重复筛选复用作用域。游标失效后，可用同一筛选的新快照更新该消费者的确认位置。`Event.Ack` 仍核对筛选范围及位置。[长筛选与确认恢复验收](../tests/test_bounded_watch_scopes.py)
- 格式 75 将新增的 `Event.Ack` 消费者确认位置限制为每主体 128 个、每权威 4096 个。确认位置持续保留，达到上限仍可推进已有位置。`Event.Offset` 传入 `include_revision: true` 可取得当前修订。用 `Event.OffsetDelete` 提交 `consumer` 和 `expected_revision` 精确清理。相同删除请求在位置仍不存在时返回 `deleted: false`。同一消费者重建或推进后，旧修订会得到 `Conflict`。
- `Event.OffsetList` 按消费者 ID 分页，使用返回的 `next_consumer` 作为下一页的 `after`。本机所有者可为已撤销的主体传入 `owner`，列出其位置并逐项删除。实际删除记入审计。[确认位置容量与清理验收](../tests/test_event_offset_capacity.py)
- 推进已有位置时，`Event.Ack` 可将 `Event.Offset` 返回的 `revision` 作为 `expected_revision`，同时传入 `previous` 游标。成功后返回当前 `cursor` 和 `revision`。同名消费者删除并重建后，即使游标相同，旧修订也会得到 `Conflict`。若回复丢失，原请求仅在其直接后继仍为当前确认位置、且游标仍属于当前授权时重放成功。首次创建及原有仅游标确认保持既有请求格式。命令为 `psiosctl event-ack CONSUMER CURSOR --previous PREVIOUS --expected-revision REVISION`，Rust SDK 使用 `Client::acknowledge_events_checked`。[确认位置修订验收](../tests/test_event_offset_generation_ack.py)
- CLI 使用 `psiosctl event-offset CONSUMER` 或 `psiosctl event-offsets` 取得修订，再以 `psiosctl event-offset-delete CONSUMER --expected-revision REVISION` 精确删除。Rust SDK 提供 `Client::event_offset`、`Client::event_offsets` 和 `Client::delete_event_offset`。
- 消费者处理事件后，可用 `psiosctl event-ack CONSUMER CURSOR --previous PREVIOUS_CURSOR` 或 `Client::acknowledge_events` 确认位置。初次确认省略前一游标。回执丢失时，同一主体在原授权代次及恢复世代内可重送已确认的相同游标。即使短期作用域已回收，回执仍可核验，不产生新修订或重复提交审计。游标失效后，继续订阅仍需要重新列举。[作用域清理与回执恢复验收](../tests/test_bounded_watch_scopes.py)
- 格式 52 开始，新建的多资源快照按资源首次提交顺序分页，每项解析为快照边界时的修订。高频状态更新不会改变既有资源在列表中的位置。升级前签发的有效分页游标继续使用原有顺序，事件订阅继续按修订提交顺序投递。[固定快照和事件续读](../tests/resource_snapshot_upgrade.py)
- `Task.Wait` 接受 Task 的 `id`、1 至 10000 毫秒的 `timeout_ms` 和可选 `after_version`。传入此前读到的 `task_revision` 可等待新修订。省略时等待 `Succeeded`、`Failed`、`Cancelled`、`Paused`、`WaitingApproval` 或 `WaitingInput`。到期返回最新快照。每次唤醒均复核 `Task.Get` 的同一读取权限，连接关闭会释放名额。CLI 和 Rust SDK 的完成等待使用此接口，等待名额用完时按退避间隔读取任务。
- 回执丢失后，`Task.Cancel` 可用当前 Task 修订关闭 `Paused / Unknown` 任务的执行流程。`Task.CancelOrigin` 也可用原始提交的来源、键、摘要和取消范围记录此请求。此时 Task 为 `Cancelled / Unknown`，保留原 Run、Operation、效果证据及资源占用。原 Operation 完成对账时，Task 保持 `Cancelled`，同时更新结果知识并保存已核实的效果。待核实时仍无法重试、归档或回收结果。[真实写入故障后的取消验收](../tests/test_volume_write_reconciliation.py)

### 块设备代次与布局核对

用 `node.devices` 取得当前设备号，向操作者显式授予目标 Node 上的 `node.block_layout`、`task.submit` 和 `task.read`。提交 `{"major_minor":"253:0"}` 观测指定整盘，设备号需替换为目标的当前值。结果随 Task 结算。该工具需要 Linux sysfs 的 `diskseq`。[Linux 块设备 ABI](https://docs.kernel.org/admin-guide/abi-stable.html)

本机所有者可以通过 `node.block_layout` 的 `trackable: true` 获取可登记附件的 `fence`，再用目标 Node 上的显式授权提交 `node.track_block_device`。请求包含同一个 `major_minor` 和完整的 `expected` 围栏。Core 复核启动标识、内核设备代次、附件摘要和几何后保存 `BlockDevice`，并在 Task 结果中返回 `device_ref`。

格式 103 按 Linux 启动和原附件登记设备。支持不可移除的非堆叠直连整盘，以及可通过只读设备描述符核验文件身份的只读 loop。对登记设备显式授予 `device.block_layout` 后，可用 `BlockDevice.Get` 读取最近观测及 `on_current_boot`，也可提交带设备 ID 和当前修订的 `device.block_layout` 获取新的观测。几何变化推进原设备修订。确认缺席、设备替换或原启动结束时，结果保留原身份以及对应状态。替换附件经所有者重新登记并取得独立授权后可观测。设备状态变化随资源版本发布。[loop 复用与恢复验收](../tests/test_block_layout.py)

在本机 Core 配置中设置 `"block_device_reconcile_ms": 1000` 可开启已登记设备的后台复查。省略或设为 `null` 时关闭。取值 250 至 60000 毫秒，格式 111 起每批从活动索引按首次登记顺序选取最多八台 `Present` 或 `Absent` 的设备。每轮固定扫描终点，轮内新增登记在下一轮检查。只有已核实的生命周期或布局摘要变化才产生新修订，所有者可在 `Event.Poll` 或 `Resource.Watch` 中观察。sysfs 被遮蔽、读取失败或采样中遇到身份变化时，保留上次状态并等待下一轮复查。事件记录实际提交的采样变化，采样间的短暂状态可能未被观察到。[后台复查、重启和 sysfs 遮蔽验收](../tests/test_block_layout.py)

本机所有者可用 `psiosctl block-device-retire DEVICE --revision REVISION` 或 `BlockDevice.Retire` 退役一条精确的活动登记。Core 保存终态 `Retired`、原设备身份和上次观测修订，既有读取权限可继续查询保留状态。退役只更新 Core 权威，设备本身仍由 Linux 管理。同一附件的登记 ID 永久保留，重新插入且通过独立身份核验的附件需要重新登记和授权。[退役、事件和附件复用验收](../tests/test_block_layout.py)

结果含启动 ID、内核盘代次、挂载命名空间、逻辑及物理扇区大小、分区起点和大小，以及当前 holders 与直接后端设备。可取得 WWID 或序列号时返回带来源的摘要。读取期间重新打开内核节点，核对设备身份并重读布局。保存返回的 `fence`，用新的幂等键提交含原设备号与 `expected` 字段的 Task。把原 `fence` 对象作为 `expected` 值。更换设备、替换 loop 后端或改变布局时，原观察失效。[真实 loop 与 Device Mapper 验收](../tests/test_block_layout.py) · [格式 22 升级](../tests/block_layout_upgrade.py)

此结果可用于安装前复核。写入安装介质还需要核验盘上内容签名、取得排他设备控制并通过独立的写入审批。[安装规范](v1/docs/03-host-updates-network-devices.md#3-安装与本地恢复)

### 权威存储余量

`psiosctl node` 的 `authority_storage` 显示 state 和 witness 所在文件系统的容量、普通进程可用字节和 inode、配置阈值、只读挂载状态及观测时间。部署可分别配置字节和 inode 阈值，旧配置省略的新字段默认为零。

```json
{"authority_reserve":{"state_bytes":16777216,"witness_bytes":2097152,"state_inodes":128,"witness_inodes":32}}
```

任一已配置文件系统低于阈值时，新任务、新输入和新模型租约返回可退避重试的 `QuotaExceeded`。文件系统未报告可用的 inode 计数时，`available_inodes` 为 `null`，启用 inode 阈值的准入返回 `UnsupportedCapability`。幂等重复请求仍返回原回执，已接受的读取、删除、结算和取消仍可继续。压力期间的准入拒绝直接返回给调用方，节约原来的持久审计空间。可用空间恢复后，当前守护进程重新接受请求。[容量与任务回收测试](../tests/test_platform.py) · [模型租约结算测试](../tests/test_runtime.py) · [Linux statvfs 手册](https://man7.org/linux/man-pages/man3/statvfs.3.html)

创建卷、备份和恢复，以及向未挂载卷分配文件或目录时，Core 在写入前按实际目标文件系统重新核对字节和 inode。状态与见证位于同一文件系统时，分别应用两者中较高的字节和 inode 阈值。原子替换在旧文件仍存在时计入临时文件的空间和 inode。核对根据 Linux 实时余量处理新分配，宿主进程、容器及并发分配仍共享资源，真实写入错误按原效果结算规则处理。[真实 extents 与 inode 压力验收](../tests/test_storage_reserve.py)

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

结果包含节点引用、该节点记录的 boot ID、挂载与 cgroup 命名空间和 `CLOCK_BOOTTIME` 的采样起止点。读数来自已核验的 procfs 和 cgroup v2 文件。被其他挂载遮蔽的计数项会报告不可用状态，其余计数可独立返回。各项按 `Observed`、`Unsupported`、`Denied` 或 `Unavailable` 报告。若复核发现守护进程已离开采样的 cgroup，则该组结果报告 `ChangedDuringObservation`。结果发布前再次核对授权。任务准入继续使用权威的配置阈值和预算。[真实限额、授权和内核降级验收](../tests/test_pressure.py) · [格式 21 升级](../tests/pressure_upgrade.py)

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

Core 可选地先向该目标指定的解析器发出 TCP DNS 请求。按端点家族查询 A 或 AAAA，在回复内核对有界 CNAME 链，并要求最终地址集合与配置的单个地址一致。回复不匹配、不可解析或超限时，端点连接不会发出。TCP 使用固定地址和端口，仅建立连接。发给解析器的内容只有 DNS 查询。两步共享 50 至 5000 毫秒的期限，默认 2000 毫秒。期限使用 Linux `CLOCK_BOOTTIME`，计入系统挂起时间。到期时关闭等待中的套接字，后续阶段保持未执行。[Linux timerfd](https://man7.org/linux/man-pages/man2/timerfd_create.2.html)

制品包含目标引用、启动 ID、网络命名空间和观测时间，并分别记录解析与连接结果。Connected 的范围是指定 TCP 端点。连接被拒绝或只有有效 DNS 回复时报告 Limited。没有这两类响应时报告 Offline。DNS 解析器的记录和 TCP 连通性证据不能证明应用层身份。改变端点、解析器或 DNS 名称会注销原资源并生成新授权目标。仅改超时保留身份并更新修订。队列中的旧目标请求按原引用复核。[TCP 与 DNS 进程验收](../tests/test_network_probe.py) · [格式 20 升级](../tests/network_probe_upgrade.py) · [Hickory DNS 协议库](https://docs.rs/hickory-proto/0.26.3/hickory_proto/)

需要把探测用于自动化时，另行授予限定目标的 `node.observe_network_probe`。Task 输入沿用 `id` 与 `expected_resource_version`。提交成功后，从结果制品获取完整测量和 `network_probe_state_ref`。也可通过 `Resource.List` 查询 `NetworkProbeState`，按 `spec.probe_ref` 找到对应探针的状态。公共资源只包含探针引用、节点 ID、生命周期、规范化原因及状态变更时间。地址、端口和 DNS 细节保存在有授权限制的 Task 制品中。

两种探测都在发起 DNS 请求、连接已登记端点及提交结果前复核原执行许可。探测期间撤销授权或取消 Task 会阻止后续连接和结果提交。

`Resource.Watch` 可以观察状态变更，`Schedule` 可按 `LifecycleEntered` 触发。探测成功连通时状态为 `Healthy`，明确拒绝或配置所需的 DNS 结果不符合时可为 `Unhealthy`，超时和无法确认的测量为 `Unknown`。Core 按持久派发顺序接受测量，重复状态只推进私有位置，不生成新的公共修订。最新测量制品用于离线核验，更新或重置状态后可回收前一制品。同一配置和网络命名空间内的服务重启保留状态。配置修订、内核启动或网络命名空间变化后重新观察，注销后状态为 `Retired`。[状态与事件的本机验收](../tests/test_network_probe_state.py)

### 本地备用网口试配置

管理员在宿主预置两个 NetworkManager 静态 IPv4 Profile，把原 Profile 启用在专用备用网口，再在 root 所有且仅 root 可写的 Core 配置中登记该网口。两个 Profile 必须使用同一网段内的不同地址。只允许此网口现有的直连路由，两个 Profile 均禁用自动连接、默认路由、IPv6 和 DNS 配置。候选地址须与本机其他网口上的地址无冲突。此后端通过显式配置的本地 root 权威启用。[NetworkManager 检查点](https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Checkpoint.html)

```json
{
  "network_link": {
    "interface": "psios-spare0",
    "mac": "02:50:53:00:00:01",
    "original_uuid": "1aad7685-9a7d-49ad-8132-0c9c56cfb34e",
    "candidate_uuid": "597da7b3-c8ba-4134-bfa7-57377f1ab670",
    "confirmation_seconds": 40
  }
}
```

`psiosctl network-link network-link-025053000001` 返回登记的资源修订。在取得新审批后，用该修订提交 `network.trial`。Core 持久记录原 Operation 和网口占用，调用范围明确为此网口的 NetworkManager 检查点，并在切换前保存其标识。确认前，先按站点要求用备用链路测试候选地址，然后用操作 ID 和当前操作修订取得及提交本地挑战。可以通过管道交付挑战。

```sh
psiosctl network-trial-confirmation "$OPERATION_ID" --revision "$OPERATION_REVISION" \
  | psiosctl network-trial-confirm "$OPERATION_ID" --revision "$OPERATION_REVISION"
```

挑战绑定原操作、配置代次和期限。超时后，Core 拒绝提交，NetworkManager 按其同一进程的定时检查点尝试回退。网络管理器、检查点、设备或实际地址发生变化时，需要通过原 Operation 的授权观察读取实际结果。无法独立确认的结果继续占用网口。当前范围限于本机 Unix API 和专用备用链路。远程新连接认证、管理网口与 DNS 变更仍需产品验收。[Core 端到端验收](../tests/network_trial.py) · [隔离链路验收](../tests/network_manager_trial.py) · [v1 网络契约](v1/docs/03-host-updates-network-devices.md)

试配置仍在等待确认时，本机所有者可以指定同一个 Operation 立即回退。

```sh
psiosctl network-trial-rollback "$OPERATION_ID" --revision "$OPERATION_REVISION"
```

Core 在请求 NetworkManager 回退前持久记录派发，核对原检查点、设备和当前试配置。确认与回退竞争同一个操作修订和派发位置。只有该设备返回成功且 Core 重新观察到原网络配置后，原 Task 才结算为 `Cancelled / Known`，结果制品记录 `RolledBack`。回执丢失时保留原操作的未决状态和网口占用，通过授权观察核对结局。守护进程重启后沿用原检查点的恢复路径。[NetworkManager CheckpointRollback](https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.html)

原执行器已收到候选激活对象，但激活状态写入失败时，Core 会尝试独立保存这份回执。只有存在该回执时，原操作才可在检查点到期后通过 `network.inspect_trial` 取得回退证据。Core 复核同一个管理器和网口、检查点已消失、原配置已恢复且没有待激活的候选连接，再通过 `Operation.Reconcile` 结算原操作并释放网口。候选配置仍在运行、缺少精确检查点或无法证明调用已返回时，网口继续由原未决操作占用。[激活回执丢失验收](../tests/network_trial_activation_recovery.py)

### 受管 systemd 服务

运维人员在本机配置中显式加入 `services`。先使用观察配置并重启守护进程。

```json
{"services":[{"unit":"example.service","manager":"user","allow_control":false}]}
```

这段是配置文件中的一个字段。`manager` 支持 `user` 和 `system`。用 `service.inspect` 查询 `service-example.service` 的当前资源版本，输入为 `id` 和 `expected_resource_version`。确认服务定义后，把返回的 `definition_digest` 写入配置项，并按需开启 `allow_control`。摘要涵盖 `systemctl cat` 返回的 unit 和 drop-in 定义。服务账户必须具备对应 manager 的访问权，控制操作沿用 Linux 的权限。

`service.start`、`service.stop` 经过审批、目标占用、定义摘要和 manager 加载状态检查。已派发后丢失回执时，先执行新的 `service.inspect` Task，再向 `Operation.Reconcile` 提交 Operation 的 ID、修订和新的 `evidence_ref`。服务会核对证据来源、调度先后、受管目标、定义和期望状态。系统记录可观察到的结果。具有宿主管理权的其他程序也可以改变该 unit，外部 unit 依赖和程序包变更属于宿主运维的信任边界。[systemctl 官方手册](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html)

`service.logs` 通过只读 Task 查询已登记服务在当前启动中的 journald 尾部记录。给操作者显式授予 `service.logs`、`task.submit` 和 `task.read`，并用 `tool_targets` 指定目标 HostService。输入为 `id`、`expected_resource_version` 和可选 `limit`，默认 16 条，最多 32 条。服务侧按 Linux 启动 ID 和准确的 systemd 单元筛选，用户服务还核对 manager 的属主 UID。日志查询可与控制操作同时进行。

制品包含固定服务引用、采集时间和可核验的日志游标。消息按 UTF-8 或 Base64 返回，每条最多 4096 字节，无法表示的消息标为 `Omitted`。`journalctl` 不可用、权限不足导致查询失败或执行超时返回 `DependencyUnavailable`。总输出超限时减小 `limit` 后重新提交。升级到格式 34 会安装能力描述，已有授权范围继续由原授权记录决定。[真实日志与格式升级验收](../tests/service_journal.py) · [journalctl 官方手册](https://www.freedesktop.org/software/systemd/man/latest/journalctl.html)

`service.logs_since` 从保留的日志位置按顺序续读。调用需要针对同一 HostService 的显式 `service.logs_since`、`task.submit` 和 `task.read` 授权。输入包含 `id`、`expected_resource_version`、`boot_id`、`after_cursor` 和可选 `limit`。可用尾部制品中的 `boot_id` 和一条记录的 `journal_cursor` 发起首个续读任务。

回执的 `entries` 从已核对的游标之后开始，`next_cursor` 指向最后一条返回记录。空页沿用原游标。若 `has_more` 为真，持该游标继续提交即可读取下一页。Core 要求当前启动、登记单元和管理器属主匹配，原位置丢失或属于另一单元时会拒绝续读。CLI 使用 `psiosctl submit service.logs_since --key KEY --delegation ID` 读取请求 JSON，SDK 使用同一 Task 接口。[真实分页验收](../tests/service_journal_since.py) · [journalctl 官方手册](https://www.freedesktop.org/software/systemd/man/latest/journalctl.html)

格式 92 安装独立的续读能力描述。已经发放的尾读权限不变。尾读及续读在制品发布前再核对当前执行授权，迟到回执不保存日志正文。升级后的资源接口会遮蔽旧迟到回执中的日志正文。

`service.restart` 需要目标 HostService 上单独授予的权限、当前资源版本和审批。将 `service.inspect` 的 `boot_id`、`invocation_id` 作为输入的 `expected_boot_id`、`expected_invocation_id`。执行前核对已批准的运行实例和配置定义，只对仍处于活跃状态的单元执行 `systemctl try-restart`。成功回执包含在同一次 Linux 启动中核验的新调用代次。

丢失回执时，服务占用保留到对账完成。使用派发后最新的 `service.inspect` 制品作为 `Operation.Reconcile` 的证据。对账核验原配置和启动 ID，确认新代次已处于活跃状态，记录旧、新调用身份。期间保持原任务及幂等记录。格式 35 安装独立的重启能力。[真实重启与崩溃恢复验收](../tests/service_restart.py)

格式 69 起，`service.start`、`service.stop` 和 `service.restart` 的派发意图绑定原 Operation、Plan、Run、Effect、执行许可、批准记录及受管服务修订。Core 再次核对审批和授权，在同一事务持久化调用前采样及 `EffectStarted`，随后执行 `systemctl`。调用前检查点未提交的中断会以 `NotStarted` 结算并释放服务占用。检查点已提交且结果未知时，保留原 Operation 和服务占用，等待独立观测。正常结算和对账都返回 `checkpoint_ref`，并保存独立于制品保留期限的结算证据。

调用前与结算时的采样记录配置摘要、预期状态、systemd 配置的管理范围、UID 及 Linux 启动 ID。两次采样的管理范围、UID 和启动 ID 必须一致。重启证据还须含有效的新调用代次。系统重新启动或管理范围与 UID 改变后，旧操作保持待核实。持有宿主权限的程序仍可改变服务，回执记录 Core 实际看到的状态。Core 在启动调度器前核对未结算服务操作的排他占用。所有引用同一 `service.inspect` 制品的对账都已持久确认并通过校验时，管理员可回收该检查制品的数据。已在格式 69 前派发的操作沿用原对账和制品保留要求。[检查点拒绝和进程中断验收](../tests/service_restart.py)

格式 77 起，新服务控制还在原检查点绑定 systemd 管理器的单调启动时间。通过同一配置的 `systemctl` 连接在单元查询前后读取 `UserspaceTimestampMonotonic`，将稳定值记录为 `manager_started_monotonic_us`。持久提交检查点前后都会复核管理器。一个新的管理器即使在同次 Linux 启动内让相同单元达到期望状态，也无法凭该观测确认旧操作。检查点已提交但管理器无法确认时，保持原操作未知并保留占用。较早派发的检查点保留其原有证据要求。该字段记录独立调用期间采样到的连续性，宿主管理任务仍须协调同一单元的外部写入。[systemd 管理器接口](https://www.freedesktop.org/software/systemd/man/255/org.freedesktop.systemd1.html) · [管理器替换验收](../tests/service_manager_epoch.py)

`service.observe` 将指定 HostService 的实际运行状态写入 `HostServiceState`。显式授予目标服务上的 `service.observe`、`task.submit` 和 `task.read`，输入使用 HostService 的 `id` 和 `expected_spec_generation`。结果包含固定 `service_state_ref` 和本次观测。控制操作更新 HostService 的当前修订后，同一配置代次仍可继续采样。采样确认规格和 Linux 启动一致，按持久派发顺序发布，重复状态保持同一观察资源修订。

订阅 `HostServiceState` 的资源事件可观察 `Active`、`Inactive`、`Failed` 等变化。systemd 明确报告单元缺失或屏蔽时可查询 `Missing`、`Masked`。使用现有 Schedule 的 `LifecycleEntered` 可将观测结果送入授权任务。观测和 Task 结果在同一事务中发布，已登记配置移除时状态变为 `Retired`。格式 36 创建状态并安装能力，配置完成后的初始状态为 `Unknown`。[真实服务、事件和故障验收](../tests/service_state.py)

发布前再次核对原服务观测的委托、许可和当前 Task。同一配置代次内较早的有效样本可作为 `Superseded` 返回，当前状态保持最新。执行中的撤权或取消会阻止该样本推进状态、触发计划或生成制品，已结算制品仍按原读取策略保留。历史迟到回执只显示围栏结果。[真实撤权和取消验收](../tests/service_observation_authority.py)

格式 108 增加只读 `service.resources`。需对已登记的 HostService 显式授予 `service.resources`、`task.submit` 和 `task.read`。输入是服务的 `id`、`expected_resource_version`，以及 `service.inspect` 返回的 `boot_id` 和 `invocation_id`，分别填入 `expected_boot_id` 和 `expected_invocation_id`。支持主进程直接位于其服务 cgroup 的运行实例。采样期间会两次核验 systemd 管理器、实例、主进程身份、cgroup 挂载和目录；实例退出、配置修订变化或读取被拒绝时，任务会返回对应错误。

结果附带 Linux cgroup v2 的进程计数、CPU 使用与节流计数、内存与交换用量，以及可见祖先层级中的最紧 CPU 配额比率、内存、交换和进程数上限。各指标可报告不支持或不可用。CPU 配额比率描述长期带宽上限，可用容量还受其他工作负载及调度影响；命名空间外的祖先可能无法观测。报告只保留已核验的资源身份和各项读数。[真实 systemd 与遮蔽验收](../tests/service_resources.py) · [Linux cgroup v2](https://www.kernel.org/doc/html/v6.8/admin-guide/cgroup-v2.html)

## 卷文件与应用版本

| Task 工具 | 参数与行为 |
| --- | --- |
| volume.files | `target` 为卷的 ID 和预期修订。`path` 默认为根，每页最多 128 项。同一目录续页时将 `next_after` 的值原样传入 `after`，值为 `null` 时已到最后一页。 |
| volume.mkdir、volume.remove_entry | `target, path`。创建目录，或删除单个普通文件及空目录。 |
| volume.append | `target, path, offset, data_base64`。偏移为零时排他新建，后续追加要求实际长度等于偏移。每块最多 96 KiB，单文件上限 256 MiB。沿用主体的 Task 输入保留额度。 |
| volume.read_chunk | `target, path, offset, limit`。取回 Base64 数据、块摘要、下一偏移和 EOF。可以从待激活的恢复卷读取。 |
| application.stage_release | `target` 指应用，`release` 使用安装参数，省略 volume。依据本机 OCI 摘要生成属于同一应用的清单和不可变 Release，保留当前部署。 |
| application.stage_artifact_release | `target, reference`。在应用和制品范围内批准新的不可变 Release，继承现有应用的卷绑定。制品保留和来源凭据随版本提交。 |
| application.releases | `application_id`、可选 `cursor, limit`。在授予的应用范围内分页读取版本及选定状态。 |
| application.select_release | `target, release_ref`。应用停止时经审批切换至选定 Release，生成新部署。选回已有版本使用相同接口。卷数据世代保持连续，应用层数据库迁移须另外协调。 |
| application.retire_release | `target, release_ref, reason`。经审批退役当前无部署使用的 Release，保留审计记录并释放源制品保留关系。 |
| application.refresh | 应用 ID 和预期修订。把引擎观测写回 Application 和 RuntimeInstance。进程意外退出时记录 Exited 和 NotReady，并保留现场及数据占用，直到显式停止清理。 |

多段路径通过 Linux openat2 在已核实的卷根目录内解析。符号链接与嵌套挂载不能作为中间路径。追加和目录修改需要批准。未知追加可以用 `Operation.Reconcile` 核对原卷、预期长度和原始字节范围。应用启动、停止会把应用与绑定卷同时列入计划前提和 ExecutionPermit。还原卷会检查目标 StoragePool 的版本和分配状态。[Linux openat2 手册](https://man7.org/linux/man-pages/man2/openat2.2.html)

未挂载卷的文件读取、目录列表、用量和完整性结果在发布前再次核对原 Task 的许可。撤权或取消后的读取不生成新制品，迟到的读取结果仅记录围栏状态。已结算的制品仍按原有读取权限提供。

直接 OCI 安装、暂存、选择和卸载的变更与原 Task 回执在同一权威事务提交。调用 Docker 时仅检查镜像。提交目录变更前，Core 复核原许可、授权、审批及目标修订。格式 124 记录精确的 `PermitBound` 派发边界。重启后仍处于该边界的 Task 以 `NotStarted` 结束并释放应用、卷占用。若 SQLite 已提交而见证尚待确认，重启沿用原成功回执。两种情况都保留审批使用及幂等记录。升级后禁止旧版写入，历史上已标为启动效果的记录继续沿用原恢复边界。[真实 Docker、进程中断与升级验收](../tests/oci_catalog_settlement.py)

新启动的容器会记录 Docker Engine ID。如果停止操作在引擎删除容器后丢失回执，应用和卷仍保留占用。运行新的 `application.health` Task，由同一引擎确认原容器及该应用的受管实例均已消失。把该检查的 ArtifactRef 作为 `evidence_ref` 提交至 `Operation.Reconcile`，服务才会一并结算停止操作、运行实例和卷解绑。引擎不可达、引擎身份变化、容器仍在或使用旧版缺少引擎身份的记录时，都无法用缺失观测释放卷。
启动流程会在核验容器配置后、请求 Docker 启动前，将引擎 ID、容器 ID、原 Operation 和应用、卷修订存入带见证的检查点。Core 在派发前和收到启动回执后检查 Docker 报告的 CPU、内存、禁用 swap、PID 和临时目录限额，同时核对只读根、独立命名空间、无额外设备与特权，以及获准的卷绑定。原生临时目录 `/tmp` 带有 `noexec`、`nosuid` 和 `nodev`。

若 Docker 已返回创建的完整容器 ID，但首次启动前发现配置偏离批准范围，Core 会重新核对同一引擎、容器和未启动状态。仅对确切原容器执行一次不带强制或删卷参数的删除，随后核对同一引擎上的容器与应用实例均已消失。确认后，原任务以已知失败结算并释放占用。引擎不可达、删除回执丢失或无法确认对象时，原任务保留未知结果及应用和卷占用。[真实 Docker 创建与删除回执验收](../tests/oci_unstarted.py)

使用本机 cgroup v2 时，针对运行中的容器，Core 还会核对宿主 PID 所属的 Docker cgroup，从内核读取 CPU 配额与 burst、内存、swap 和进程数限额。`effective_limits.source` 为 `LinuxCgroupV2` 时，观测附带实际限额及 cgroup inode。旧 cgroup v1 配置沿用引擎检查，观测来源为 `DockerCgroupV1`。[Linux cgroup v2 控制项](https://docs.kernel.org/admin-guide/cgroup-v2.html)

若启动回执丢失，先运行新的 `application.health`，再向 `Operation.Reconcile` 提交该 ArtifactRef。同一引擎须确认原容器仍在运行，镜像、隔离配置和限额观测符合获准范围，Core 才会登记运行实例、挂载卷并结算原任务。容器只完成创建、已经退出、配置或读出的限额不符、引擎不可达，或中断发生在检查点写入之前时，原操作继续保留占用。旧版生成的观测须用新检查刷新后再提交对账。[真实 Docker 对账验收](../tests/oci_recovery.py)

从格式 109 起，Core 在发送 Docker 创建请求前记录原批准、引擎与启动身份、应用与卷修订，并把一次性标识写入创建请求。收到完整容器 ID 后，先保存创建回执，再进行启动检查或现场补偿。丢失 Docker 创建回执时，可由本地所有者取得限定于原 Operation、应用与绑定卷的两项授权。先用 `application.inspect_start_create` 按原 Operation 的 ID 与修订检查同一引擎。检查核对完整容器 ID、创建标识、从未启动的状态、原静态配置和卷身份。

确认原容器存在后，提交 `application.discard_start_create`，参数为应用的 `target`、原 `operation_ref` 和检查制品 `evidence_ref`。独立审批后，Core 在同一连接上重新读取确切容器，提交删除检查点，再请求一次不带强制或删卷选项的删除。删除回执丢失时，先用新的独立检查结算清理 Operation。原 Operation 还需由检查制品确认删除后才能结算为已知失败并释放原应用与卷占用。删除前缺少容器、引擎或启动身份变化、旧格式缺少创建证据，以及已经达到启动检查点的记录均不能使用这条清理路径。适用范围为受信任的本地 Docker Engine。[真实回执丢失与格式升级验收](../tests/oci_create_intent.py)

### 校验卷内文件

给核验主体签发 `volume.verify_file`、`task.submit` 和 `task.read`，并在 `tool_targets` 指定 Volume ID。请求使用 `target` 内的卷 ID 与预期修订，以及相对 `path`。可选 `expected_digest` 为 SHA-256，`max_bytes` 默认 32 MiB，最高 256 MiB。结果制品包含卷和数据世代、文件 inode 及时间戳、逻辑字节数、内容摘要和整个观察的摘要。

Core 通过已打开的目录和文件描述符流式校验私有普通文件。校验结束时核对原目录中的路径和文件版本，落库前复核执行许可。可在恢复卷激活前核对文件。改变文件身份、超过字节上限或撤销执行权都会得到明确结算结果。[二进制文件、路径限制、数据世代及撤权验收](../tests/test_volume_file_integrity.py)

同一未挂载卷可同时执行文件读取、分块读取、目录列表、空间统计和文件校验。Core 为每项在途读取保存独立的持有记录。写入、备份、激活、删除和应用挂载仍要求排他访问。冲突按现有 Task 语义结算，进程恢复会清除原读取持有并按剩余预算重试。读取成功时只释放本次持有。[读写隔离与恢复验收](../crates/psios-core/src/recovery_tests.rs) · [格式 48 在途读取升级](../tests/volume_read_holds_upgrade.py)

### 卷写入的持久对账

`volume.write` 或 `volume.append` 丢失回执时，本机所有者可对原 Operation 调用 `Operation.Reconcile`。Core 在卷仍由该操作独占时读取原计划指定的文件和字节，逐项同步该文件与父目录，再核对路径和 inode 后提交结算。同步期间数据库仍可处理其他请求。任一路同步失败或文件被替换时，原 Operation 保持 `Unknown` 并保留卷占用。成功后的恢复证据包含文件修订。[SIGKILL 与 fsync 故障注入验收](../tests/test_volume_write_reconciliation.py)

### 卷内目录创建和对账

从数据库格式 120 起，`volume.mkdir` 在独占原 Volume、重新验证审批和期限后，在 `/data` 外的本机私有目录暂存一个空目录。Core 固定原 Volume、目的父目录、暂存区和空目录 inode，刷新目录后提交检查点，再以 `RENAME_NOREPLACE` 发布。正常成功仍返回原有的卷引用和 `receipt.path`。当前目录后端保留普通 POSIX 权限和同组 setgid 继承，拒绝需要跨组继承、默认 ACL 或继承安全标签的父目录配置。

回执丢失后，本机所有者以原 Operation 的精确修订调用 `Operation.Reconcile`。Core 核验并同步原空目录及两侧父目录。原目录位于获准位置时结算成功，仍处于私有暂存区且目的位置为空时结算 `NotApplied`。两处都缺失、身份或父目录被替换、新目录出现内容或元数据不符时保留 `Unknown` 和原卷占用。结算不会重发 mkdir 或 rename。已核实未发布的私有暂存项只能用空目录删除，其他文件会保留。旧版本已派发而无检查点的操作在升级后仍保留未知结果。[实际目录发布与故障验收](../tests/test_volume_mkdir_recovery.py)

### 卷内条目删除和对账

从格式 113 起，`volume.remove_entry` 在当前卷修订和原批准下记录准确的源路径与 inode，再将条目原子移入同一文件系统上的私有保留目录。源目录与保留目录完成同步后，Core 提交卷修订和逻辑删除回执。支持单链接普通文件，以及服务账户可以检查为空的目录。卷挂载或其他写入占用期间继续拒绝删除。

操作回执丢失时，本机所有者可对原 Operation 调用 `Operation.Reconcile`。Core 保留原卷占用并核验源路径和私有保留项。原条目仍位于源路径时记录未执行，原条目在保留目录且源路径为空时结算删除。两处都存在、两处都缺失、身份变化或待删除目录已包含内容时，均保留未知结果。旧格式已派发且缺少检查点的删除仍无法通过此协议确认。

逻辑删除结算后，后台回收与回执绑定的原 inode，不会递归删除目录，也不会触碰复用原路径的新条目。未结算或未回收条目全节点合计最多 128 项，普通文件逻辑大小合计上限为 1 TiB。待回收项仍保留时，Core 拒绝删除整个卷。托管卷的外部写入须与 Core 协调并在操作前停止。[缺失、替换、重启和清理中断验收](../tests/test_volume_entry_retirement.py)

### 原子改名及对账

`volume.rename_file` 在同一未挂载 Volume 内移动一个私有普通文件。请求包含卷的当前 `target`、相对 `source` 和 `destination`，以及先前 `volume.verify_file` 的 `expected_observation_digest`。需要对卷显式授予工具权限和批准本次计划。Core 重读原文件，提交包含内容摘要、inode 和目录身份的检查点，再使用 Linux 原子改名。已有目的项会使请求以 `Conflict` 结算。

若实际改名后丢失回执，原 Operation 保留卷。给恢复主体授予限定于该 Operation 的 `volume.inspect_rename`，可独立检查实际目录、inode 和文件摘要。将新核验制品交给 `Operation.Reconcile`，Core 在原 Operation 上确认改名或确认仍处于原位置，再释放卷。普通队列饱和时，核验可使用恢复准入。[原子改名与回执丢失验收](../tests/test_volume_file_rename.py) · [格式 49 观测复用](../tests/volume_file_rename_upgrade.py)

### 重启正在运行的 OCI 实例

配置 `oci_effective_volume_observation: true` 可为带卷的 OCI 启动、重启与检查启用 Linux 实际挂载核验。此配置默认关闭。启用需要本机 cgroup v2 和服务 UID 对容器进程的 `/proc` 观测权限。Core 在启动后固定进程身份，对照卷已登记的设备与 inode，核实独立挂载命名空间中的 `/data` 是私有、可写且没有嵌套挂载。运行实例的 `mount_observation` 保存卷引用、挂载 ID、启动标识和采样时间。

原启动或重启的检查点会保留核验要求。内核进程进入批准 UID 与执行入口的短暂过程中，Core 可在同一固定进程上重新读取证据。若 Docker 已执行启动而无法核实挂载，操作维持 `OutcomeUnknown` 并保留占用。重新对账须取得同一容器现有运行的有效核验样本。持续访问拒绝、不同 inode 或容器已退出时，旧操作继续等待处置。格式 102 保留已有运行实例与原检查点，可对其采集新的独立证据。[真实挂载和故障验收](../tests/oci_mount_observation.py) · [格式升级验收](../tests/oci_mount_observation_upgrade.py)

`application.inspect_instance` 校验实际容器、发布配置、卷挂载、Linux 启动身份和有效资源限制。将结果中的 `boot_id`、`started_at` 分别作为 `expected_boot_id`、`expected_started_at`，连同应用 ID 与当前修订提交 `application.restart`。两项能力均需显式授权。应用绑定卷时，授权目标同时包含应用和卷。

重启需独立审批。Core 在调用 OCI 引擎前核对启动身份并持久记录检查点，在回执中保存同一容器和 Linux 启动下的新启动身份。未知结果保留应用与卷的操作约束。用新的观察制品对账，确认后登记在原 Task 与 Operation 下。[真实 OCI 引擎验收](../tests/oci_restart.py)

从格式 74 起，安装或暂存 OCI Release 时，可在直接请求或签名制品的 `install` 段显式加入 `health_check`。字段为 `argv`、`interval_ms`、`timeout_ms` 和 `retries`。按镜像配置摘要核对 Docker 的 exec 形式 `CMD`，要求命令和时间设置与批准输入完全一致。探针最多 16 个参数，首项为容器内的绝对路径。间隔为 1000 至 60000 毫秒，探针进程超时为 100 至 5000 毫秒且不得超过间隔，重试次数为 1 至 5。省略 `health_check` 时 Docker 健康检查保持禁用。

格式 129 允许成对指定 `start_period_ms` 和 `start_interval_ms`，由 Docker Engine 25.0 及更新版本执行。启动间隔为 1000 至 60000 毫秒，须至少等于探针超时且严格短于至多 120000 毫秒的宽限期。两项设置在镜像准入和容器实际配置中均须与批准值精确一致。Docker 在启动阶段按指定间隔探测，成功即可进入 `Ready`。 [真实健康检查验收](../tests/oci_health.py) · [Docker HEALTHCHECK](https://docs.docker.com/reference/dockerfile/#healthcheck)

`health_check_ref` 固定经核验的镜像探针。原始批准、固定发布物和镜像摘要共同绑定是否启用及全部参数。容器启动前核对生效配置。探针沿用应用的非 root UID、只读根文件系统、挂载和网络隔离。Docker 单独管理启动探针进程的等待时间。`application.health`、`application.refresh`、`application.inspect_instance` 和启动核实均按当前容器调用的近期样本报告健康和就绪。初始检查未完成时，健康为 `Unknown`，就绪为 `NotReady`。通过历史证据对账后，已启用探针的运行应用保持 `Unknown`，直到下次观测。探针的标准输出和错误输出留在 Docker 引擎内。[Docker 健康监视实现](https://raw.githubusercontent.com/moby/moby/v28.5.2/daemon/health.go)

Core 在发布这三项应用观测前复核原执行许可和当前目标修订。授权撤回或任务取消会阻止样本进入新制品或应用状态，并保留既有应用与运行实例修订。已合法发布的制品沿用其读取权限。[真实 Docker 观测验收](../tests/oci_restart.py)

格式 71 将重启检查点与原执行许可、单次批准、Run、应用及绑定卷修订关联。Core 先提交 `EffectStarted` 和检查点，再请求 Docker 重启。调用前中断以 `NotStarted` 结算并释放本次应用占用。丢失调用回执时，原 Operation 保持待核实。Core 启动前检查未结算操作的应用占用及现有卷绑定。离线核验同时核对历史应用、实例及确认记录，允许在任务归档后继续校验。格式 43 至 70 的重启沿用当时记录的证据。旧版在结算失败后可能留有缺少检查点的未知结果，仍需保持占用并等待单独核实。[检查点失效与离线验收](../tests/oci_restart.py)

### 恢复 OCI 停止操作

若停止操作丢失回执，Core 保留原 Application 占用和卷挂载记录。使用 `application.health` 核实原容器。若容器已退出但仍存在，本机所有者可用显式授予的 `application.continue_stop` 提交清理，授权目标应包含应用及其挂载卷。提交参数为 `target`（应用 ID 与当前修订）、`operation_ref`（原未知 Operation）和 `evidence_ref`（新健康检查制品）。

Rust SDK 的 `Client::prepare_oci_stop_continuation` 与 `psiosctl application-prepare-stop-continuation --key KEY --delegation ID request.json` 接收包含 `application_ref`、`operation_ref` 和 `evidence_ref` 的固定资源引用。返回的请求可提交给 `Task.Submit`，并使用系统现有的单次审批。

审批后，Core 重新核对容器身份和退出状态，仅对原容器执行非强制删除。普通队列饱和时，可使用恢复准入。删除回执丢失时，先用新的健康检查确认原容器已移除，再对账清理 Operation。原停止操作也要提交明确确认容器已移除的健康检查制品，完成对账后才释放占用。若 Docker 无法确认删除结果，操作继续保留未知状态。恢复路径的停止日志标记为不可用。[真实 Docker 回执丢失验收](../tests/oci_recovery.py)

调用前核对的 `StartedAt` 来自 Docker 的实例观测。Docker 提供按完整容器 ID 的非强制删除，但不支持将该时间戳作为删除条件。对 PsiOS 托管实例的生命周期写入须通过 Core 串行执行。其他可信宿主服务直接写入 Docker 时，须先协调相关实例的操作。

### 估算备份容量

格式 130 登记只读工具 `backup.estimate`。使用与 `backup.create` 相同的卷 ID 和预期修订，并为该卷显式授予 `backup.estimate`。结果给出源数据代次、逻辑字节、文件和目录数量、当前备份上限，以及逻辑载荷是否在上限内。测量和备份共享源树的类型、权限、深度和条目检查。测量期间持有卷读取保留，创建备份时仍会重新校验源和目标空间。 [稀疏文件与实际备份验收](../tests/test_backup_estimate.py)

### 核实备份发布回执

本地备份在持有原卷期间写入私有暂存目录。发布前重新检查执行许可，将卷引用、Operation 和清单摘要及暂存目录身份一起提交至带见证的权威，然后以原子重命名发布备份。新协议下若在授权发布前崩溃，原 Task 以已知失败结算并释放卷。存在发布检查点而原回执丢失时，原 Task 暂停并保留卷占用。

给恢复主体授予限定到原 Operation 的 `backup.inspect_publication`、`task.submit` 和 `task.read`，对未知 Operation 的当前修订运行观测 Task。Core 验证原目录的身份、清单和全部数据摘要，并同步备份所在目录。管理员将新制品的引用作为 `evidence_ref` 提交给 `Operation.Reconcile`。核实已发布时，Core 在一个事务中登记原 Backup、保存原 Task 回执并释放卷。

从数据库格式 123 起，发布检查点绑定私有暂存名、父目录和捕获时的文件集合。原目录仍在该暂存位置、所有成员保持原身份及内容且最终路径不存在时，Core 可以结算 `NotApplied`，保存原 Task 回执并释放源卷。两个位置都存在、均不存在，目录身份变化或核验失败时保留 `Unknown`。旧检查点仍可核实已发布的备份。节点最多留存 128 项待对账或待回收的备份暂存目录。

已确认未发布的暂存目录由后台重命名隔离。清理中断后，Core 按证据核验剩余文件，只回收原捕获中记录的普通文件。发现其他成员时保留该目录。原请求重放返回原结算，新的备份操作需要独立提交。[真实中断与清理验收](../tests/test_storage_recovery.py) · [旧格式回执验收](../tests/backup_creation_publication_upgrade.py)

### 核实恢复卷发布回执

`backup.restore` 在原子发布前记录源备份、目标卷和暂存目录身份。Task 截止点和授权期限约束本次发布许可，上限为一天。进程在发布检查点提交前中断时，可以凭原意图结算为已知的未发布失败，随后重新提交恢复。检查点已提交而结果未知时，源备份继续被占用。

从数据库格式 112 起，新备份记录卷数据根目录的普通 Unix 权限位。恢复先写入成员，再设置根目录权限和修改时间并刷盘。托管数据根须保留所有者的读取和搜索权限，可以是只读目录。恢复发布核验、`volume.activate` 和 `application.rebind_restored_volume` 会核对新证明中的根目录权限。旧备份没有该字段，仍按 `0700` 恢复并使用原证明算法。旧版 Core 无法打开已升级的权威执行写入。[权限恢复与升级验收](../tests/test_backup_root_mode.py)

备份创建和预检要求数据根归 Core 的服务用户与组所有，并拒绝特殊权限位及扩展属性。新备份在暂存前检查这些条件，发布前复核。恢复校验和激活使用同样的根元数据规则，数据根变更后须重新核验方可激活。 [根元数据验收](../tests/test_backup_root_mode.py)

授予核验主体 `volume.inspect_publication`、`task.submit` 和 `task.read`，并将目标限于原 Operation。观察任务重新核对实际卷目录和树内容。管理员用其制品作为 `Operation.Reconcile` 的 `evidence_ref`，Core 再检查目录的当前身份和元数据。核实已发布后登记最初的恢复卷，保留原 Task、Operation 与数据世代。卷仍需独立批准激活。[回执丢失、篡改与激活验收](../tests/test_storage_recovery.py) · [原格式升级](../tests/restore_publication_upgrade.py)

格式 76 的检查点额外记录暂存目录名。完成数据与父目录同步后才提交检查点，启动时留存其绑定的暂存卷。最终目录中的原卷通过核验后报告 `Published`。原暂存目录仍有同一父目录、卷根、数据目录、身份标记及树内容，且最终路径不存在时报告 `NotApplied`。两处都存在、均不可达或身份被替换时保留原未知结果。为早期已发布检查点保留原回执核验路径。

管理员提交 `NotApplied` 证据后，Core 复核当前路径及元数据，在同一事务内将原 Task 结算为已知失败，释放备份占用并登记暂存清理。后台按原目录身份隔离并删除暂存卷，清理中断后可继续。待录证和待清理的恢复占用共同受节点级 128 项限额约束。任何再次恢复都要重新获得执行许可，原请求仍返回原结算。[文件系统提交边界强杀验收](../tests/test_storage_recovery.py)

### 核实新建空卷的发布回执

从数据库格式 118 起，`volume.create` 在发布空卷前将原 Task、获准使用的 `local` StoragePool 修订、卷身份、父目录和暂存目录身份写入带见证的权威。数据目录和父目录先同步，再提交检查点并执行不覆盖原路径的原子重命名。随后 Core 在同一事务登记卷及原 Task 回执。正常创建返回原有的 Volume 引用，卷可直接使用。

若发布回执丢失，使用限于原 Operation 的 `volume.inspect_publication` 核验目录和空数据根，再由管理员用 `Operation.Reconcile` 提交观测制品。Core 重新核验路径和元数据，只登记仍在最终位置的原卷。原目录仍在暂存位置时可结算 `NotApplied`，后台仅删除经核实的空目录和身份标记。两处都存在、均不可达、已有其他文件或身份被替换时保留未知结果。发布检查点之前中断可结算为 `NotStarted`。旧版未记录检查点的未知创建仍需保留，格式 118 的权威拒绝旧版 Core 写入。[发布边界、篡改和升级验收](../tests/test_volume_create_recovery.py)

### 本地单次复制卷克隆

格式 114 支持 `volume.clone`。本机所有者须持有显式限定到源 Volume ID 和本机 StoragePool `local` 的委托，并独立批准操作。输入包含源卷的 `target` 和新卷 `name`。SDK `Client::prepare_volume_clone` 和 `psiosctl volume-prepare-clone --name NAME --key KEY --delegation ID source-ref.json` 返回可提交的 Task 请求。

源卷须已脱离应用且无其他写入、读取或未结算占用。Core 在源卷排他保留期间把文件直接复制到新卷，核对源文件和目录身份、字节摘要、修改时间及普通 Unix 权限。数据根须归服务账户及其组所有，带有扩展属性、ACL 或特殊权限位的根目录会被拒绝。源卷与新卷分别拥有独立 ID 和数据世代，新卷记录准确的 `cloned_from`。复制使用 `backup_max_bytes` 对单次数据量的限制及本机容量保留。此能力提供已脱离的单卷崩溃一致性副本，适用于该本机目录后端。

新卷创建后进入 `AwaitingActivation`。独立批准 `volume.activate` 会核对树摘要与元数据，核验成功后才允许写入和应用挂载。发布回执丢失时用 `volume.inspect_publication` 核对原 Operation，再用 `Operation.Reconcile` 结算。Core 只核对同一暂存目录或最终目录，在证据缺失时保留源卷占用。已核实未发布的目录由后台回收，原请求重放仍返回原结算。[克隆与容量验收](../tests/test_volume_clone.py) · [发布故障与恢复验收](../tests/test_volume_clone_recovery.py)

### 将停止的 OCI 应用切换到恢复卷

先用 `backup.restore` 建立独立卷，再用 `volume.activate` 核实并激活它。`application.rebind_restored_volume` 接受应用的 `target` 以及当前卷和恢复卷的完整 `source_volume_ref`、`restored_volume_ref`。两个卷须同属该应用的数据谱系，恢复卷须保持原激活修订。为本机管理员签发包含 `task.submit`、`task.read` 和该操作的委托，并将 `tool_targets` 限于应用、当前卷、恢复卷的 ID。使用该委托提交 Task，再审批包含三个精确引用的计划。

Core 独占两个卷和应用，核对恢复证明、当前文件树和 OCI 容器缺席，然后在同一事务提交新 Deployment 和 Task 回执。原应用 ID、Release 和旧数据卷保留。应用保持停止，可用新的应用修订另行提交 `application.start`。格式 99 增加该能力及离线检查。能力范围是同一节点上经验证的单卷 OCI 恢复。 [真实 OCI 验收](../tests/oci_volume_rebind.py)

格式 116 也接受独立激活后的 `volume.clone` 卷。Core 核对原克隆 Task、已结算的发布检查点和新卷初始修订，并验证捕获的源卷 ID 与数据世代仍与应用数据绑定一致。新的审批仍固定调用时的应用和两个卷修订。重绑定再次验证克隆根目录的所有者、权限与扩展属性。新 Deployment 保留克隆发布和独立激活证据，便于离线检查。 [真实 OCI 克隆重绑定验收](../tests/oci_direct_clone_rebind.py)

### 导出备份

通过 `Grant.Issue` 把 `backup.export` 授予指定主体，并在 `tool_targets` 中列出获准访问的 Backup ID。主体可使用该 Backup 当前修订的 `ResourceRef` 调用 `Backup.Export`。权限包含清单和备份数据。通用资源查询与卷访问仍按各自权限裁决。

每次请求传入 `reference`、`member`、`offset` 和 `length`。`member` 取 `{"kind":"Manifest"}` 或 `{"kind":"Blob","index":0}`，`index` 对应清单中的零基条目位置，目录没有 blob。每块最多 131072 字节。响应包含成员完整摘要、总字节数、当前与后续偏移，以及 Base64 编码的块数据。Core 核验每个文件的内容，并在回传每一块之前重新核对授权、修订及占用状态和提交审计。[接口与故障验收](../tests/test_backup_export.py)

服务长文件的首次读取时，一个请求最多核验 16 MiB，并在每次本机读取后检查经过时间。文件还在核验时返回可重试的 `QuotaExceeded`，`details` 中包含已核验字节 `backup_verified_bytes` 和成员长度 `backup_member_bytes`。使用原偏移继续请求，直到完整 SHA-256 核验通过并返回数据。服务独立保留最近四个备份修订的核验状态，文件身份、内容元数据或清单变更后重新核验。守护进程重启后重新核验。

Rust SDK 的 `Client::download_backup_member` 可随核验进度继续等待并流式获取单个成员，`Client::resume_backup_member` 也可核验已有的私有文件前缀。`psiosctl backup-export` 校验完整副本并发布到私有父目录下。将回执清单摘要保存到独立介质，可用 `psiosd verify-backup` 在原服务离线时校验或恢复副本。[命令与脱机恢复](offline-rescue.md#验证和恢复独立的卷备份)

## 身份与权限

`Grant.Issue` 可提供 `tool_targets`，把工具限制在明确的资源 ID 集合中。对已绑定卷的应用启停，授权需覆盖应用和卷。通用资源读取仍按独立权限和资源所有者裁决。设置 `allow_delegation=true` 和 `max_delegation_depth` 可启用最多 4 层转授。子委托可以收窄动作、目标、期限和剩余层数，共享根预算。沿用旧配置的 Grant 保持原有的单层状态委托范围。每次派发检查完整祖先链及其主体。

`Grant.Issue` 可选填 `task_concurrency`，取值 1 至 16。根授权和所有子委托共享并发上限。已领取、已派发和结果未知的 Task 占用名额。等待审批或等待派发的 Task 仍按原规则预留执行次数，其他根预算可继续派发。任务结算或解除领取后腾出名额。对结果未知的操作完成对账后，排队任务可继续执行。`Budget.Get` 给出最大、已占用、未知及剩余名额。原有预算在格式 40 升级后沿用原派发规则。[失去效果回执和跨授权派发](../tests/test_task_concurrency.py) · [格式 39 升级](../tests/task_concurrency_upgrade.py)

`Secret.Rotate` 接受秘密 ID、预期修订和新值。新密文使用新随机 nonce，并将秘密身份及密钥版本纳入认证数据。轮换前接受且尚未派发的旧计划会失效。旧库中的密文可以继续使用并原地轮换。撤销的秘密不能通过轮换重新激活。

新签发的 Grant 和 Delegation 同时保存绝对 UTC 截止时间与基于 Linux `CLOCK_BOOTTIME` 的时限。Core 在同一内核启动和时间命名空间内同时检查这两个条件，包括服务重启后的权限检查、队列结算和效果准入。墙钟回拨后，已经用完的经过时间仍然计入时限。变更时间命名空间时需要重新取得适用的委托。切换到另一内核启动时使用原 UTC 截止时间。格式 25 保留旧签发记录的原期限，新时限随新签发写入权威。[原生盘回拨验收](../tests/native_grant_deadline.py) · [时间命名空间验收](../tests/grant_time_namespace.py) · [格式 24 升级](../tests/grant_deadline_upgrade.py)

时钟使用由当前 procfs 提供的 boot ID 和调用线程的 Linux 时间命名空间。支持 systemd 将同源 `/proc/sys` 只读重新挂载，也支持由主机提供的 LXC `.lxc-boot-id` 只读单文件配置。Core 在读取前核验实际挂载和文件身份，时间命名空间来自核验过的 `nsfs`。时钟来源无法核实时，依赖该时钟的授权检查会失败。[私有挂载与时钟来源验收](../tests/kernel_clock_provenance.py)

新接受的 Task 在同一事务中固定计划的经过时间截止点。审批决定、队列结算和派发按 UTC 与经过时间中先到期的一项处理。原任务在当前内核启动中的截止点同时约束重试，幂等重发仍可取回原回执。执行许可的短期窗口也保存经过时间，适用于立即准入与备份发布、OCI 启动等后续检查点。已经派发的操作仍按实际效果与回执结算。格式 26 保留早期已接受 Task 的 UTC 期限与审批状态。跨内核启动继续使用原 UTC 截止时间。[真实回拨验收](../tests/native_grant_deadline.py) · [格式 25 升级](../tests/task_deadline_upgrade.py)

## 任务输出依赖

格式 128 在只读 Task 发布结果的事务中复核原执行许可、精确资源目标、当前委托和关联的 Agent 会话。取消、撤权或改变目标修订会阻止发布。重复到达的只读样本只保留围栏状态，历史迟到回执的查询同样隐藏样本。 [Linux 读取边界验收](../tests/test_host_read_publication.py)

`Task.Prepare` 可填 `requires`，每项指定已有上游 `task_id` 和结果制品的 `digest`。依赖属于固定输入清单，参与请求摘要和审批计划。最多指定四个不同上游，所有内容校验共用 8 MiB 上限。主体须有权读取各上游。Rust SDK 使用 `Client::submit_with_requirements`，CLI 使用 `psiosctl submit --requires FILE` 读取依赖 JSON 数组。

直接 `Task.Prepare` 将输入按主体暂存在本机，返回现有 v1 提交请求。暂存跟随进程重启，期限最多十分钟，并受请求和授权原期限限制。每主体最多 128 个、单节点最多 1024 个暂存请求，各自的暂存输入上限为 8 MiB 和 64 MiB。主体可用获准的 `Resource.Get` 或 `Resource.Resolve` 预览自己的输入。Task 受理事务才发布持久输入、Task、Operation、预算使用及依赖保留关系。重复使用相同的输入在持久存储中共用内容，提交仍按各自的精确预备请求检查。

`Task.LookupSubmission` 可查询原受理回执、当前可用窗口或过期结果。可用窗口受请求截止时间、暂存期限和整条委托链的实时时钟及已用时间共同约束。使用早期 Core 保留输入的直接请求须能解析该调用者拥有的精确输入版本和摘要。无法核实输入时返回 `Indeterminate`，客户端应保留原键并停止自动重传。同键同请求在有效期内复用原窗口，更改已暂存键的请求会返回 `IdempotencyConflict`。未受理请求过期后，主体可显式重新执行 `Task.Prepare` 以获得新窗口。已受理记录和取消墓碑继续约束原键。用 `Task.DiscardPreparation` 或 `psiosctl task-discard-preparation REQUEST.json` 丢弃尚未受理的精确请求，并永久关闭该键。已经受理时返回当前受理状态。调度、Workflow 及格式 95 的已持久化输入仍可按原引用使用。

`Task.Admission` 可传入 `include_preparations: true` 查看当前主体暂存的请求数、输入字节数及限制。CLI 使用 `psiosctl task-admission --include-preparations`。授权撤销在受理入口立即生效。守护进程启动及授权世代更新后，后台分批复核仍保留的暂存，回收失效授权对应的请求和已无其他引用的暂存输入。仍有效的授权范围继续使用原请求和期限。

上游尚未成功，或结果经权威标为 `Unavailable` 时，下游保持 `WaitingResource` 并保留原执行预算。依赖全部可用后，Core 校验固定摘要及实际字节，再开启审批或排队。领取和派发时复核权限与制品。实际执行快照保存通过核验的制品引用。工具仍接收清单中的 `tool_input`。需要读取制品内容时，调用者沿用固定引用和 `Artifact.Read`。

上游失败、摘要不匹配、已生效的结果回收、授权变化或截止均可让下游在派发前结束。未结算的下游保留上游制品，直到取消、失败或完成结算。修复标为不可用的上游制品后，先用 `Task.VerifyResults` 重新确认结果，再继续派发。[重启、审批与制品恢复验收](../tests/test_task_dependencies.py) · [格式 40 升级](../tests/task_dependencies_upgrade.py)

### 跨主体读取和依赖

管理员可为 Agent 签发 `task.results.read`，在 `tool_targets` 列出已有的生产 Task ID。读取方可用 `Task.Results`、`Client::task_results` 或 `psiosctl task-results ID` 查询生命周期、结果知识、修订、条件代码和制品引用。读取方可按原引用调用 `Artifact.Get`、`Artifact.Read` 或 `Artifact.ReadChunk`。Core 逐次核对当前授予的 Task 和制品修订，记录主体与读取目标。任务详情、历史、审批、事件以及制品修改各自沿用原授权。

跨主体的 `requires` 在接受时保存实际使用的读取委托。审批、领取和派发时重新检查。撤销或到期会使尚未派发的下游释放制品保留和执行预算，包括节点处于 `Cordoned` 时。重新签发读取授权后可提交新下游。已派发的操作继续通过原 Operation 结算。授权按本机权威内的 Task ID 生效。[跨主体、撤权与到期验收](../tests/test_task_result_access.py) · [格式 44 升级](../tests/task_result_access_upgrade.py)

### 队列饱和时的结果对账

`queue_limit` 计入仍占用资源的未知操作。普通准入占满时，`recovery_queue_limit` 为既有未知操作的核验任务预留位置，默认 2，可设为 0 至 16。`Node.Get.admission` 返回两类占用和上限。预留准入一次只接受同一未知 Operation 的一项核验任务，并固定原 Operation 修订。可用的核验包括备份、恢复卷、OCI 启停与重启以及受管 systemd 服务。核验沿用原有工具授权、根预算和调度状态。带输出依赖的任务使用普通准入。

核验任务结算或取消时释放预留。重启保留尚未结算的准入。如果目标已对账，待派发的核验任务会在领取前结束。最终对账继续使用 `Operation.Reconcile` 和核验证据。[满队列与恢复验收](../tests/test_recovery_admission.py)

### 按主体限制普通任务准入

`principal_queue_limit` 可设为 1 至 `queue_limit`，默认沿用节点的普通准入上限。同一提交主体的不同 Grant 和委托共享该上限，等待、执行和未对账的普通 Task 都计入占用。已接受请求的幂等回执继续可查，其他主体仍可使用节点剩余容量。普通占用达到主体上限时，原未知操作的核验还可以使用预留位置。

`Task.Admission`、`Client::task_admission` 和 `psiosctl task-admission` 公开当前主体的普通准入用量，要求 `task.submit`。[竞争、重启及对账验收](../tests/test_principal_admission.py)

### 维护期间关闭本机工作准入

本机所有者可调用 `Node.WorkAdmission.Get` 查看持久准入模式、活动 Task 阶段、恢复队列和模型租约数量。以返回的版本调用 `Node.WorkAdmission.Set`，提交 `expected_resource_version`、`mode` 和 `reason`。CLI 对应 `psiosctl node-work-admission` 和 `psiosctl node-work-admission-set REVISION draining REASON`。维护完成后，使用当前版本设置 `Open` 或 CLI 的 `open`。Rust SDK 提供同样的读写操作。

`Draining` 暂停新普通 Task 和重试准入、Schedule 与 Workflow 的新派发以及新模型租约。已接受的 Task 可领取、审批、执行或取消，原请求重放可查询既有回执。已有模型租约可在原范围内结算本轮决定及其工具 Task，下一轮领取仍需准入开放。与原未决 Operation 精确匹配的核验 Task 可进入既有有界恢复队列，并继续遵循自身授权和预算。模式在 Core 重启后保留。

`Node.Configure` 的 `Cordoned` 继续独立控制派发。格式 93 升级时，独立准入模式初始值为 `Open`，原节点模式和已接受工作沿用原版本。本机准入计数用于观察 Task、恢复队列及模型状态。服务停止、卷释放及外部操作是否达到维护安全点，各自需要观测回执。[运维顺序](v1/docs/13-operations-release-governance.md) · [准入验收](../tests/test_node_work_admission.py) · [格式 92 升级](../tests/node_work_admission_upgrade.py)

以准入修订调用 `Node.WorkAdmission.Blockers` 可读取同一事务内的五组样本。CLI 用 `psiosctl node-work-admission-blockers REVISION --limit 16`。每组默认最多 16 条，上限 64 条。若 `truncated` 为真，该组触及采样上限，可能还有未核对的条目。Task 优先展示未决效果和等待确认的操作。独占锁关联当前资源和持有者，正在使用卷的应用包含精确附件引用，读取锁关联卷及正在读取的 Task。恢复还未登记新卷时显示源备份的锁。

应用换绑期间，独占锁组将应用、原卷和已激活的替换卷关联到同一个 Operation 和 Task。快照核对采样到的换绑 Task 和锁，并按原计划校验三个资源的修订和锁的完整性。证据不齐时返回 `RecoveryRequired`。[真实 OCI 换绑验收](../tests/oci_direct_clone_rebind.py)

模型组显示仍有效的当前租约，以及授权已失效但仍在原时限内的租约。严格租约到期而尚未结算时显示 `ElapsedPending`，已记录的未知效果继续独立可查。跨启动或计时范围变化时，残留时限使用保存的 UTC 期限估计。快照包含精确引用和采集时间，供运维确认后逐项核验。[锁与恢复验收](../tests/test_maintenance_blockers.py) · [模型租约验收](../tests/test_maintenance_model_blockers.py)

### 按主体限制块式读取结果

`read_result_bytes_per_principal` 可为普通只读 Task 的块式结果设置每主体 4 MiB 至 1 TiB 的上限。接受 Task 时为本次结果预留 4 MiB。完成时按实际 JSON 字节从预留转为保留，用量和制品在同一事务提交。失败、取消及显式回收释放对应字节。设置上限后，`Task.Admission.read_results` 显示预留、保留和上限。额度吃满时，提交返回可退避重试的 `QuotaExceeded`。

旧任务已接受的执行仍可结算。升级按现有块式制品清单和待执行 Task 回填用量。写操作的效果证据、资源状态投影和恢复核验继续由原来的权威存储保护。[配额、重启与真实回收验收](../tests/test_read_result_quota.py) · [格式 50 用量回填](../tests/read_result_quota_upgrade.py)

### 批量回收任务结果字节

本机所有者可先用 `Task.ResultEvictionCandidates` 查询早于 `updated_before` 的候选任务。截止时间使用 RFC 3339，可用 `principal_id` 限定原任务主体。`psiosctl task-result-eviction-candidates` 从标准输入接收查询。续页使用 `next_cursor`，它固定匹配范围与修订水位。Core 按原修订顺序扫描，并实时跳过已变动、受保留或不合格的结果。某一页若只有跳过项，仍可使用返回的续页游标继续。

候选项返回 Task 的完整引用和清单中的逻辑字节数。批量预检与执行会重新读取实际内容并检查保留状态。[候选分页与保留竞争验收](../tests/test_result_eviction_candidates.py)

本机所有者可用 `Task.PrepareResultEvictionBatch` 提交 1 至 16 个完整 Task 引用和非空 `reason`。可选 `max_content_bytes` 默认 16 MiB，上限 64 MiB。预检核对已结算的只读 Task、产物完整性和当前保留关系，返回每项字节数、总量以及两分钟内有效的 `plan_token`。CLI 用 `psiosctl task-result-eviction-plan` 从标准输入读取这份 JSON 请求。

`Task.ApplyResultEvictionBatch` 接收计划令牌。Core 在同一事务重新核对当前任务修订、产物和工作流、会话、项目证据等保留关系。全部满足时才回收各项任务的制品字节，并记录原 Task 引用、每项原回执和整批回执。期间出现新的保留关系或部分写入错误时，整批回滚。授权代次、启动身份、时间命名空间或控制权变化会使未执行计划失效。已结算的原请求在当前权威下可按原令牌重放查询回执。

`psiosctl task-result-eviction-apply` 可直接读取计划输出的 JSON。回执中的 `content_bytes_removed` 记录释放的逻辑载荷字节数。离线核验检查同一事务内各项回执、历史 Task 修订和分块确实已删除。[跨模块保留与回滚验收](../tests/test_task_result_eviction_batch.py)

#### 预约回收固定结果

在预检请求中设置 `delay_seconds`，取值为 1 至 86400 秒。预检令牌同时绑定回收清单和延时。所有者在两分钟内调用 `Task.ScheduleResultEvictionBatch`，或将同一计划传给 `psiosctl task-result-eviction-schedule`，Core 从预约受理起按 Linux `CLOCK_BOOTTIME` 计时。节点最多同时保留 64 个待处理预约。同一令牌重复提交会返回原预约的当前状态。

`Task.GetResultEvictionSchedule` 返回当前状态和精确版本。所有者可以用 `Task.CancelResultEvictionSchedule` 加预约 ID 和 `expected_resource_version` 取消待处理版本。对应 CLI 是 `task-result-eviction-schedule-get ID` 和 `task-result-eviction-schedule-cancel ID REVISION`。Rust SDK 提供相同操作。

同次 Linux 启动内，进程强杀或重启后会继续处理到期预约。到期时重新核对权威、授权代次、启动身份、计时命名空间、每项修订和新的保留关系。权威或计时范围变化将预约置为 `Suspended`，结果变动或新增保留将整批置为 `Rejected`。管理员可重新预检并提交新计划。界面中的 UTC 到期时间用于展示，实际准入依据经过时间。

回收分块、更新 Task、写入逐项回执及整批回执、结算预约在一个持久事务内完成。成功预约链接到批次回执，批次也链接到原预约版本。离线核验检查双向引用及原始回收清单。[进程与权限验收](../tests/test_delayed_result_eviction.py) · [格式 90 升级](../tests/delayed_result_eviction_upgrade.py)

新预约遵循节点的权威存储预留。已有预约仍可查阅、取消或到期处理。批次遇到写入错误会整批回滚，其他到期预约继续处理。报告的释放量为原制品的逻辑字节数。

只读 Task 的分块永久丢失时，先用 `Task.VerifyResults` 确认 `Unavailable`，再由本机所有者用当前任务修订和理由调用 `Task.AbandonResults`。对应 CLI 是 `task-abandon-results ID --revision REV --reason REASON`。Core 在提交时复核原不可变清单、实际损坏及所有现有保留关系。成功后保留原 Run、Operation 和制品引用，原子移除剩余分块并释放配额。独立回执记录原逻辑占用和实际移除的字节数。[损坏、恢复与配额验收](../tests/test_artifact_abandonment.py)

## 本机制品暂存与发布

`Artifact.UploadProfile` 公布 `OpaqueBytesV1` 的分块、大小、暂存期限及配额。上传主体需要以本机 Node 为目标的 `artifact.upload` Grant。`Artifact.Stage` 固定提交键、预计字节数、SHA-256、声明媒体类型和期限。`Artifact.Stage.PutChunk` 按已提交偏移接收 128 KiB 定长块及末块。中断后通过 `Artifact.Stage.Get` 取得继续位置，用同一键和输入恢复。

`Artifact.UploadCapacity` 和 `psiosctl artifact-upload-capacity` 显示调用主体的暂存字节、发布保留字节、开放的暂存、历史回执和剩余回执数。本机所有者还可查看节点总量。历史回执默认每主体最多 4096 条、每节点最多 65536 条，可在 Core 配置的 `artifact_upload_history` 中以 `per_principal` 和 `node` 设置更低的上限。上限作用于新建暂存，已受理的阶段可继续结算或按原键重放。丢弃、过期和撤回仍保留历史回执。 [Unix API 与并发验收](../tests/test_artifact_upload_capacity.py)

`Artifact.Publish` 重新核对全部字节后原子提交不可变引用及来源，发布重试返回原回执。可见内容的媒体类型为 `application/octet-stream`，提交者提供的媒体类型作为独立声明保留。发布前可以丢弃暂存，过期块由后台回收。读取原始字节需要 `artifact.read`。`Artifact.Withdraw` 使用额外的 `artifact.withdraw` Grant 和确切引用，回收字节并保留审计凭据。

本机 SQLite 上传发生已确认的内容丢失时，同一撤回授权可调用 `Artifact.AbandonUpload`。使用固定的 Artifact 引用和非空理由。`psiosctl artifact-abandon-upload --reason REASON` 从标准输入读取引用。安装、发布版本或 Agent 会话仍持有该引用时，Core 拒绝释放。回执保留原清单、损坏证据以及释放的逻辑和剩余字节。[损坏上传与安装保留验收](../tests/test_artifact_abandonment.py)

`psiosctl artifact-upload ./bundle.bin --key build-1` 和 Rust SDK 的 `Client::upload_artifact` 从已提交偏移继续，`psiosctl artifact` 使用返回的引用导出并校验原始字节。安装适配器可以按固定引用读取已发布制品。[中断、权限、配额和损坏验收](../tests/test_artifact_ingress.py) · [格式 53 升级](../tests/artifact_ingress_upgrade.py)

`Artifact.UploadProfile` 提供 `exact_delegation` 时，`Artifact.Stage` 可携带完整的 `delegation_ref`。CLI 对应 `psiosctl artifact-upload ./bundle.bin --key build-1 --delegation-ref delegation.json`。受绑定的暂存每次接收分块或发布前都会复核原委托版本、主体和本机 Node 范围。原委托失效后拒绝继续写入或发布，后台原子释放未发布分块并保留 `AuthorityRevoked` 终态回执和原提交键。已发布制品继续遵循独立的读取、留存及回收权限。未绑定的暂存仍支持持有当前 Node 上传授权的原主体续传。

### 发布模型会话的最终输出

`artifact.publish_session_summary` Task 接受成功会话的确切 `session_ref` 及声明的 `media_type`。允许的会话必须有经认证模型确认的最终摘要，原始 UTF-8 最多 64 KiB。调用者需要具备对会话的读取权、Node 范围的 `artifact.upload`，以及同时覆盖 Node 与会话的发布工具授权。

Core 核对原会话、模型租约及决策摘要，在一个事务中提交原始字节、来源、Task 结果和 Operation 回执。产物以 `LocalSessionOutput` 记录会话、模型、Skill、Provider 和发布任务的关联。下载与回收继续使用各自的制品权限。模型给出的媒体类型按声明处理。作为 OCI 应用使用时，仍经过已有的安装或新版本准备、策略审批和运行校验。[会话发布与故障验收](../tests/test_session_output.py)

### 从本机制品准备 OCI 安装

本机适配器读取 `application/vnd.psios.oci-install.v1+json` 制品。正文是 `{"format":"PsiOSLocalOciInstallV1","install":{...}}`，其中 `install` 使用 `application.install` 的字段，指定已存在的 OCI 镜像配置摘要、绝对入口、资源限制和可选卷版本。`Application.PrepareArtifactInstall` 接受 `reference`、`key`、`delegation_id` 和可选 `deadline_at`，返回可供 `Task.Submit` 使用的请求及具体部署参数。

读取源制品需要 Node 范围的 `artifact.read`，安装委托需要 `application.install_artifact` 且覆盖制品与目标卷或 `profile-local`。Task 准入固定源摘要并创建审批计划。待审批与执行期间保留制品，安装提交时由不可变 Release 接续保留。成功回执关联 Operation、Application、Release、Deployment 和制品来源。目录提交前中断会结算为已知失败并释放 Task 的保留关系。安装得到处于 `Stopped` 的应用，通过已有 `application.start` 启动。[授权及恢复验收](../tests/test_artifact_install.py) · [真实 OCI 验收](../tests/oci.py)

### 读取 OCI 应用日志

`application.logs` 继续接收应用 ID 和预期修订。Core 在采集前后核对 Docker Engine 和完整容器 ID，并检查应用、实例与原启动 Operation 的绑定。每次读取请求最多 64 行，每路输出保留末尾最多 8192 个 UTF-8 字节，超限时返回 `truncated`。回执记录来源和采集时间。正常停止时保留一次私有尾部，守护进程重启后仍可读取。没有可靠尾部时返回 `availability: "Unavailable"`。

提交日志任务和读取其制品正文都需要当前目标应用的 `application.logs` 授权。应用状态、健康和刷新回执及资源事件只提供运行信息。旧应用状态仍可按原引用解析，公开视图会省略历史日志正文。执行中失去授权的日志任务不会发布该次采集结果。[运行、停止和撤权验收](../tests/oci.py)

### 保存本机 OCI 镜像字节

`application.capture_oci_image` 接受 `image_id`、`expected_engine_id` 和可选的整数 `max_archive_bytes`。本机所有者使用显式限定在当前 Node 的任务委托和 `artifact.upload` 授权，提交后再批准包含镜像、引擎和原始输入的计划。省略上限时，任务固定使用 8 MiB，配置提高后也沿用该值。适配器从配置的本机 Docker Engine 以完整镜像配置摘要导出有层的 Linux amd64 镜像，前后核对引擎身份和层摘要。

守护进程配置 `oci_capture_max_bytes` 默认 8 MiB，最高 128 MiB。例如，可用 `psiosctl application-prepare-image-capture --image-id "$image" --engine-id "$engine" --max-archive-bytes 16777216 --delegation "$capture_delegation" --key capture-1` 准备一份显式申请 16 MiB 的请求。SDK 提供 `Client::prepare_oci_image_capture`。提交和执行时都会按当前配置检查批准的申请上限。原始 `Artifact.Stage` 的单份上限为 8 MiB。

已领取或执行中的捕获按各自批准的上限预留暂存容量。`oci_capture_staging_max_bytes` 默认 32 MiB，最高 512 MiB。已审批的捕获等待足够容量时保留在队列中，其他符合条件的任务可以执行。执行中的捕获取消后仍占用容量，直到原执行结算。`Node.Get` 的 `oci_capture_staging` 字段显示预留字节和上限。

格式 132 将已保留的 OCI 归档和原始上传分别计量。`oci_archive_storage` 的 `principal_bytes` 与 `node_bytes` 默认分别为 32 MiB 和 128 MiB，Node 最高 2 GiB。主体上限和暂存上限都须容纳一次最大捕获，主体上限须在 Node 上限以内。`Artifact.UploadCapacity` 返回 `oci_archive_retained_bytes`、`oci_archive_pending_disposal_bytes` 和 `oci_archive_byte_limit`。等待删除的 OCI 归档持续占用容量，直到文件回收结算。两个存储池仍共用上传历史回执上限。调低配置后既有归档仍可校验、读取和恢复。格式升级会阻止旧 Core 写入扩大后的归档状态。

导出前按批准的上限检查状态盘的块和 inode 余量。Docker 的输出分块写入状态盘的私有暂存目录，同时计算原始摘要。Core 复核 tar 成员，再把已同步的归档发布为 `ManagedOciArchiveV1`，将其制品元数据、`LocalOciImageCapture` 来源、`OciImageCapture` 凭据和 Task 回执记录在同一见证事务。新凭据和回执同时记录批准的上限。回执提交后才能读取归档。`Artifact.ReadChunk` 仍受独立授权并按固定分块核验字节。`artifact.withdraw` 提交后由清理器回收文件。事务回滚可清理当前未引用的文件，见证状态未确定时留待启动按已确认的数据库回收。[真实 Docker 导出和独立重载验收](../tests/oci_capture.py)

格式 66 开始，新捕获的归档保存在状态目录 `oci-archives`。格式 67 启用大于 8 MiB 的显式捕获，现有 SQLite 制品沿用原位置和引用。停机 `psiosd verify` 可用原状态盘上的只读归档核对数据库副本，离线迁移演练只修改暂存数据库。如果检查点包含这些活跃归档，`psiosd checkpoint` 写入 `psios.authority.checkpoint.v2` 清单及可独立读取的归档副本。`verify-checkpoint` 核对数据库选择的集合、字节数、摘要和实际目录成员。可用 `checkpoint-artifact --max-bytes 16777216` 从独立检查点按固定引用提取 16 MiB 以内的原始归档。调低捕获配置后，已保留归档仍可校验、读取、提取和恢复。[大归档检查点与真实恢复验收](../tests/oci_capacity.py)

捕获会在持久提交前检查原始 tar，并记录 `restore_compatibility`。`Verified` 对应 `DockerClassicTaglessSaveV1`，要求单个无标签镜像，精确匹配配置摘要和有序未压缩层 DiffID。外层 tar 支持两种读取器能一致解释的 V7、USTAR 和 GNU 普通文件或目录。库间解释歧义或标签会阻止发布。存在 OCI 索引时，两个视图的配置和层必须一致，且索引只直接指向一个镜像。镜像 ID 使用清单摘要、层使用压缩格式或布局不受支持时，原始捕获可保留并报告 `Unsupported`。升级到数据库格式 63 后，旧捕获标为 `Unverified`，其历史回执依旧省略此字段。证明覆盖归档结构及镜像身份，实际恢复还会检查层元数据及目的引擎。[归档格式测试](../crates/psios-core/src/apps/restore_archive/tests.rs)

### 安装和更新时保留 OCI 归档

归档声明使用 `application/vnd.psios.oci-install.v2+json`，正文为 `{"format":"PsiOSLocalOciInstallV2","install":{...},"image_archive_ref":{...}}`。`image_archive_ref` 指定已有的本机 OCI 捕获制品，必须与声明中的 `image_id` 和本机引擎看到的层摘要一致。Ed25519 签名覆盖包含归档引用的完整声明字节。安装和暂存权限须同时覆盖目标、声明、签名密钥和归档。`Application.PrepareArtifactInstall` 和 `Application.PrepareArtifactRelease` 返回同一组审批引用。

已准入 Task 立即保留声明和镜像归档。提交 Release 时转交保留关系，并在不可变镜像、发布溯源和目录回执中登记归档及捕获凭据。被选择、可回退或处于待审批状态的关联版本会阻止归档回收。选择和启动前重新核验归档内容。退役会释放该版本的保留关系，其他版本仍可保留同一制品。启动仍要求 Docker 本机镜像可用。[真实 OCI 安装、暂存与退役验收](../tests/oci.py)

### 恢复已保留的 OCI 镜像

本机所有者可通过 `Task.Prepare` 和 `Task.Submit` 提交 `application.restore_release_image`。输入包含应用的 `target`、精确的 `release_ref`、`image_archive_ref`、`publisher_key_ref` 和 `expected_engine_id`。委托需要限定在应用、版本、V2 声明、签名密钥、归档和本机 Node。恢复需要独立的 Node 范围 `artifact.read` 授权和所有者审批。镜像已存在时，任务返回已知的 `PreconditionFailed` 并跳过导入。

`psiosctl application-prepare-image-restore --key restore-1 --delegation "$restore_delegation" restore.json` 从文件读取 `application_ref`、`release_ref`、`image_archive_ref`、`publisher_key_ref` 和 `expected_engine_id`。`psiosctl application-prepare-image-restore-inspection --key inspect-1 --delegation "$inspection_delegation" operation-ref.json` 读取待核实的原 Operation 引用。两个命令均输出可通过 `psiosctl call Task.Submit` 提交的 JSON。Core 按应用或 Operation 的 ID 和版本解析目标，在受理时绑定完整资源引用。Rust SDK 提供 `Client::prepare_release_image_restore` 和 `Client::prepare_release_image_restore_inspection`，沿用同一 Task 流程。

该能力使用 Linux amd64 Docker Engine API v1.44 的经典 `overlay2` 或 `vfs` 镜像存储。服务复核仍有效的签名以及保留归档的完整字节，将核验过的镜像配置和层分块写入单镜像无标签归档。导入前封印文件描述符并记录两个归档的 SHA-256。恢复拒绝压缩层、稀疏层和扩展层元数据，累计层字节及生成的导入归档各以 128 MiB 为上限。导入使用绑定同一个 Engine 的本机 Unix 连接，依赖 Docker 及其文件系统报告空间或运行错误。

只有检查点提交后才向 Docker 发起一次导入。中断后，待核实的原 Operation 对应用和该引擎内的镜像保持独占。使用单独委托提交只读 `application.inspect_release_image_restore`，以原 Operation 的 `target` 为输入，并覆盖原计划资源。精确引擎上的镜像摘要及有序层 DiffID 得到确认后，可向 `Operation.Reconcile` 提供新的检查结果。镜像缺失、引擎身份变化或不匹配的结果都会保留 `OutcomeUnknown`。观察和结算原 Operation 不会重发导入。离线校验核对原检查点和结算证据。发行版本退役后，已合法回收的原始字节可只保留历史摘要和回执。[真实 Docker 进程中断和恢复验收](../tests/oci_restore.py)

### 从本机制品暂存新版本

`Application.PrepareArtifactRelease` 接受应用的 `target`、确切制品 `reference`、`key`、`delegation_id` 和可选 `deadline_at`。制品使用相同 OCI 声明格式，要求应用名称一致并省略 `volume`。调用者需要 Node 范围的 `artifact.read`，以及同时覆盖应用与制品的 `application.stage_artifact_release` 委托。`Client::prepare_artifact_release` 和 `psiosctl artifact-prepare-release --application app-ref.json --delegation "$delegation" --key upgrade-1 artifact-ref.json` 返回新版本参数、当前选定版本及可提交的 Task 请求。

批准后用真实 OCI 引擎核验本机镜像。新 Release、源制品保留及 `ApplicationArtifactReleaseStage` 凭据原子提交。选定版本通过单独审批的 `application.select_release` 更新。切换和启动前都重新核验版本的制品保留记录及源字节，损坏会阻断执行。[真实镜像升级与回滚验收](../tests/oci.py)

### 验证本机 OCI 声明的发布者

本机所有者通过 `Artifact.PublisherKey.Register` 将 32 字节 Ed25519 公钥的标准 Base64 与 `principal_id` 绑定。发布者对 RFC 8785 规范化 JSON 执行分离签名，字段是 `domain`（`psios/local-oci-declaration-signature/1`）、`artifact_digest`、`declared_media_type` 和字符串 `size_bytes`。签名覆盖确切声明的内容摘要和经服务端记录的媒体类型及字节数。

在 `Application.PrepareArtifactInstall` 或 `Application.PrepareArtifactRelease` 传入 `signature`，其中包含 `key_ref` 和 64 字节签名的 `signature_base64`。命令 `psiosctl artifact-prepare-install` 和 `psiosctl artifact-prepare-release` 通过 `--signature` 读取同结构的文件，Rust SDK 支持类型化调用。授权范围需要包含源制品、安装目标和公钥。成功结算的 ReleaseSpec 记录 `PublisherVerified`、签名凭据和源制品。审批继续绑定具体部署资源与制品。

`Artifact.PublisherKey.Revoke` 接受公钥 ID、`expected_resource_version` 及原因。Core 在版本选择的提交事务，以及启动和重启的宿主检查点重新检查发布者授权。先完成撤销会拒绝这些操作。先通过宿主检查点的调用可以完成，并按实际效果结算。运行实例可以继续观测和停止，离线验证能从安装或暂存回执校验历史版本的签名和发布者身份。[签名与撤销验收](../tests/test_oci_publisher.py)

### 退役本机应用版本

通过 Task 工具 `application.retire_release` 提交应用当前版本 `target`、确切 `release_ref` 和原因。Grant 必须覆盖应用和 Release，审批绑定这两项引用。仍有应用或 Deployment 使用的版本会被拒绝。切换后可退役已替代的版本，卸载后也可退役最后一次部署的版本。

退役提交后，原 Release 及 `ApplicationReleaseRetirement` 凭据仍可查询。启动和版本选择检查退役状态，待执行的旧回滚同样受此约束。退役会原子释放 Release 对本机制品的保留关系。没有其他保留者时，有权主体可另行调用 `Artifact.Withdraw` 回收字节。持久卷和备份延续各自的生命周期。[真实 OCI 验收](../tests/oci.py) · [格式 55 安装任务升级验收](../tests/release_retirement_upgrade.py)

### 查询应用版本历史

持有应用范围 `application.releases` Grant 的主体可以使用 `Application.ListReleases`、`Client::application_releases` 或 `psiosctl application-releases APP_ID`。Agent 也可通过同名 Task 工具发起查询并取回常规任务产物。分页固定第一次查询时的 Application 修订和已提交版本，按创建顺序返回 `Selected`、`Staged`、`Unselected` 或 `Retired`，并附带可用的源制品和退役引用。返回的 `next_cursor` 可由同一主体在同一应用范围内继续使用，有效期为十分钟。授权代际更新后需要重新查询。读取制品正文须独立具备制品读取权限。

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

从格式 28 创建的调度以 UTC 决定何时到达触发时间，同一内核启动内同时以经过时间约束各触发窗口何时结束。固定间隔和当地日历沿用定义中的窗间距。时钟回拨、节点封锁和守护进程重启后仍可按原窗口结算过期触发，已提交的 Task 继承该次窗口截止点。低于存储预留阈值时，调度仍可结算过期和跳过的窗口，提交新 Task 须满足 Broker 的存储余量要求。旧调度和跨内核启动继续使用原 UTC 截止时间。[调度状态验收](../tests/test_schedule.py) · [回拨验收](../tests/native_grant_deadline.py) · [格式 27 升级](../tests/schedule_deadline_upgrade.py)

在 `Schedule.Create` 请求中加入 `repeat` 可创建有限次的 UTC 固定间隔调度。例如每 60 秒运行一次，共运行三次，并在恢复时最多保留两次可执行的触发。

```json
{"interval_seconds":60,"count":3,"missed":{"mode":"BoundedCatchUp","limit":2},"overlap":"Skip"}
```

间隔为 1 至 86400 秒，触发次数为 2 至 32。每次触发时间和截止时间都以首轮输入为基准，按间隔顺延。末轮截止时间须在委托和 Grant 的共同有效期内。`Skip` 在积压时只保留最新的到期触发。`BoundedCatchUp` 保留最新的至多 8 个到期触发，具体数量由 `limit` 指定。这两种策略都会先记录超出自身截止时间的触发。`overlap` 可取 `Allow` 或 `Skip`。取 `Skip` 时，上一项 Task 尚未结算或结果知识为 `Unknown` 会跳过本轮。

进行中的调度通过 `Schedule.Get` 返回 `next` 和已结算的 `trigger_refs`。同一逻辑时刻的触发 ID 与调度定义绑定，重启后继续使用。对引用中的 ID 调用 `Schedule.TriggerGet` 可读取该轮的状态、原因及 Task 回执，读取权限限于调度所有者与本机管理员。序列用尽后返回 `Completed`。每轮分别记录已触发、已过期或已跳过。遇到永久的准入拒绝时记录该轮拒绝并结束序列。取消仍需当前修订，取消后的已提交任务可以继续用原入口管理。[格式 10 升级](../tests/schedule_repeat_upgrade.py)

格式 131 允许用 `Schedule.Renew` 续期已完成的固定间隔序列。请求包含原 `id`、`expected_resource_version`、新 `key`、当前 `delegation_id`、首轮 `run_at` 和 `deadline_at`。Core 复核原触发记录、Task、关联 Workflow 或 Session 的结算状态，以及新委托对原工具和目标的权限。新序列沿用原输入和间隔规则，每个原序列只接受一次续期。回执固定新旧引用和已结算的 Task，可用相同请求重放。CLI 接受 `psiosctl schedule-renew REQUEST.json`，Rust SDK 提供 `renew_schedule`。 [真实调度、并发及恢复验收](../tests/test_schedule_renewal.py)

`Schedule.Create` 也可用 `calendar` 提交有限期的当地日历规则。日历请求省略 `run_at`、`deadline_at` 和 `repeat`。以下规则从 2027 年 11 月 7 日起每天当地 01:30 触发，持续两天。

```json
{"timezone":"America/New_York","start_date":"2027-11-07","local_time":"01:30:00","days":2,
 "window_seconds":120,"fold":"Both","gap":"Skip","missed":{"mode":"BoundedCatchUp","limit":2},"overlap":"Skip"}
```

每份定义覆盖 1 至 30 个当地日期，最多产生 32 个 UTC 触发。`fold` 可取 `Earlier`、`Later` 或 `Both`，用于选择夏令时回拨时的一个或两个实际时刻。`gap` 可取 `Skip` 或 `Reject`，用于跳过缺失时刻或拒绝整份定义。`window_seconds` 以实际 UTC 时长计算，支持 1 至 86400 秒。委托及 Grant 都须覆盖最晚截止时间。

Core 在创建时固定 IANA 数据版本、UTC 窗口和因 `gap` 跳过的当地日期。`Schedule.Get` 的 `calendar_plan` 可查询这些值。服务重启或更新时区数据库后，既有定义继续采用原窗口和触发 ID。更改规则时取消仍待触发的旧定义，并使用新键创建新定义。`missed`、`overlap`、重试和 Task 回执遵守上述序列规则。[夏令时 UTC 对照与断电验收](../tests/native_calendar.py) · [格式 13 升级](../tests/calendar_upgrade.py)

### 本机资源事件调度

`Schedule.Create` 可用 `event` 代替定时字段。`event.source_ref` 固定当前资源版本，`event.expires_at` 指定 RFC 3339 截止时间，`event.max_triggers` 接受 1 至 32。`event.condition` 可用 `{"kind":"SpecChanged"}` 匹配资源配置代次增加，或用 `{"kind":"LifecycleEntered","lifecycle":"Succeeded"}` 匹配进入指定状态的转换。规则从创建时的源版本之后开始读取权威修订。

所选委托需要 `schedule.manage`、`resource.read`、`task.read`、`event.read`、`task.submit` 和目的工具的授权。调度器重新校验源可见性、委托、固定工具能力和有效期限。Task 沿用独立审批、执行许可和共享预算。前一个 Task 尚未结算或效果未知时，该次匹配记录为跳过。队列满时保留原源修订等待，截止时间到达后停止创建新任务。

每个 `ScheduleTrigger` 保存来源事件 ID、资源版本、序号和 Task 回执。队列满时可通过 `pending_trigger_ref` 查询持久等待中的原触发。接受任务时，写入触发结算、推进源游标和创建 Task 在同一事务完成。如果到期或取消，待提交触发保留终态及原事件。用 `Schedule.Get` 的 `trigger_refs` 查询已结算历史。[事件调度验收](../tests/test_resource_event_schedule.py) · [格式 31 升级验收](../tests/resource_event_schedule_upgrade.py)

定时与事件调度分别排列已就绪的触发。遇到可重试的准入限制时，Core 持久保存短暂延期，让其他拥有可用配额的主体继续。格式 101 保存原触发身份和同一 Linux 启动内的 1 秒退避。定时窗口和事件游标继续按原规则推进，到期处理优先，重试沿用原提交键。权威存储低于配置余量时，可用空间留给到期触发和已受理工作结算。[定时公平验收](../tests/test_schedule.py) · [事件游标与重启验收](../tests/test_resource_event_schedule.py)

### 通过任务启动工作流

普通 Task 或 Schedule 可选择 `workflow.start`。输入为固定的 `definition_ref` 和正整数 `max_runtime_seconds`。委托须允许 `workflow.start`、`workflow.manage`、任务读写以及图中各工具，并覆盖定义和执行目标。工作流步骤直接使用各自的工具，嵌套启动需要另行支持。

核心在同一事务写入 WorkflowRun、启动 Task 的结果与预算结算。结果包含子运行引用和发起 Task 的固定修订。通过 `Workflow.Get` 查询子运行的执行进度。子运行及其步骤继承发起 Task 的截止时间，启动和各步骤扣同一根额度。涉及宿主效果的步骤沿用普通任务审批。[工作流启动验收](../tests/test_workflow.py) · [格式 32 升级验收](../tests/workflow_launch_upgrade.py)

格式 117 为周期 `workflow.start` 提供 `overlap: "SkipWhileWorkflowActive"`，适用于 `repeat` 和 `calendar`。调度沿最近一个成功接受的触发检查原启动 Task 和确切的子 WorkflowRun。子工作流仍在运行、等待审批或结果未知时，本轮跳过且不创建 Task、不占用预算。子工作流到期时，只有已经确认的结算才解除保护。启动回执或关联无法核实时，本轮记录 `PreviousWorkflowUnverified`。

格式 119 为 `repeat` 和 `calendar` 提供 `missed: {"mode":"Coalesce"}`。调度先处理已过期窗口和重叠保护，再用当前已到时且有效的最后一个窗口代表本组。代表使用它原有的触发 ID、授权、截止时间和 Task 输入。一次提交持久保存至多一个 Task 和一份预算预留，以及 `coalescence.covered_trigger_ids` 中的原始窗口 ID。被合并的窗口以 `Skipped/Coalesced` 引用代表。队列暂满时保留当前待办位置和有限退避，恢复接收时可将新到时的有效窗口纳入同一组。若代表被永久拒绝，保留其 `Rejected` 触发记录，其他组成员标记 `RepresentativeRejected`，整组不会创建 Task。已过期窗口单独记录 `Expired`。[真实停机与队列压力验收](../tests/test_schedule_coalescence.py)

合并调度保存创建时的时钟作用域及单调时钟截止时间，后续窗口按原始时间差推进截止位置。启动时核对待处理索引和单调时钟记录。记录缺失、截止位置不符或终态仍占用待处理索引均需恢复。合法跨引导时保留原记录并按已有规则使用 UTC 截止时间。

跳过记录在 `Schedule.TriggerGet` 中提供 `overlap_evidence`，包含原触发、启动 Task 及已核实的回执和 WorkflowRun 修订。旧 `Skip` 仍按启动 Task 判断。使用 `Schedule.Cancel` 取消待触发的轮次，使用 `Workflow.Cancel` 管理已启动的子工作流。重叠保护作用于各自的调度。需要等待子工作流内的 Agent 时，在该步骤启用 `SessionTerminal`。新格式由当前写入器执行，旧写入器拒绝打开。[重启、取消和实际写入验收](../tests/test_workflow_schedule_overlap.py)

格式 121 的资源事件 `workflow.start` 可设置 `event.overlap: "SkipWhileWorkflowActive"`。跳过记录保留本轮的原始来源事件及上一组确切的启动凭据，推进源游标并消耗一个已记录触发名额，但不会准入 Task 或占用预算。队列压力后结算的触发修订保留原事件 ID、源序列和资源修订。省略此设置时，事件仍按上一启动 Task 判断重叠。`Schedule.Cancel` 和到期仅结算该事件订阅；已启动的子 Workflow 保留独立生命周期。[真实资源事件和写入验收](../tests/test_resource_event_workflow_overlap.py)

受保护的规则独立保存原始单调时钟截止时间及当前事件游标。每次推进都在同一事务核对并更新游标索引。启动和离线校验会拒绝丢失或被替换的记录。后续引导沿用既有跨引导截止规则。

### 通过调度创建 Agent 会话

将 `agent.session.start` 作为 `Schedule.Create` 的 `tool`。输入包括 `goal`、完整的 `skill_ref` 和 `model_ref`、`context_policy` 及 `max_turns`。所选委托需要包含 `schedule.manage`、`agent.session.start`、`session.run`、`task.submit`、`task.read` 及 Skill 所需工具。启用资源范围时，计划同时核对指定的 Model 和 Skill，后续工具依然各自核对资源。也可通过普通 Task.Submit 接受同一个工具。

Task 成功时返回 `session_ref`，会话记录原 Task 引用。会话进入选定 Model 的持久队列，可由正在等待的 Provider 接续。系统一次提交会话、Task 回执和准入用量，后续模型调用及工具沿用同一根预算。调度时间窗约束准入。已经创建的会话仍通过 `Session.Cancel` 独立管理，周期调度的重叠判定跟踪准入 Task，模型并发由 Model 和根预算约束。该准入工具不能登记为模型 Skill 的工具依赖。[故障与权限验收](../tests/test_model_queue.py)

## 本机 PsiFlow

`Workflow.Define` 登记一份不可变的本机 Task 依赖图。每步提供 `id`、`tool`、字面量 `input` 和所依赖的 `after` 步骤 ID。当前支持 1 至 16 步，定义体最多 128 KiB。Core 检查重复 ID、悬空引用与回环，并固定当时登记的工具引用。以同一键重发相同定义会得到原引用。修改定义时使用新键。

定义可指定 `max_parallel_steps`，默认 1，范围 1 至 16。已满足前置依赖的步骤在额度内各自提交普通 Task，共享同一根预算和运行期限，每个写步骤继续按原计划审批。运行回执的 `active_steps` 列出并行配置下尚未结算的步骤。已知失败或取消会停止新准入，取消其他待执行步骤，并等待在途步骤结算。若有步骤丢失结果，Workflow 暂停新准入，已提交的独立步骤继续报告结果。原步骤经对账恢复后才推进其后继。格式 38 保留已提交的串行步骤、审批和 Task 身份。[并行与故障验收](../tests/test_workflow.py) · [真实旧格式升级](../tests/workflow_parallel_upgrade.py)

`Workflow.Start` 接受 `key`、`definition_ref`、`delegation_id` 和带偏移的 `deadline_at`。委托须授予 `workflow.manage`、`task.submit`、`task.read` 和各步所需工具，覆盖实际目标，且在最终截止时间内有效。每步在派发时重查原授权、预算、工具引用和目标版本。Workflow 串行释放已满足依赖的步骤，由普通 Task 执行与审批。

### 等待原生 Agent 会话完成

工作流可在 `agent.session.start` 步骤中指定 `"completion":"SessionTerminal"`。DSL 4 要求固定的 Strict Model，以及不含嵌套 Agent 或工作流启动的 Skill。该模式使用原工作流委托和共享预算。省略 `completion` 时，启动 Task 创建会话后即满足该步骤的依赖条件。

Core 在启动 Task 结算时写入原工作流与 Session 的绑定。步骤可通过 `Workflow.Get` 查询 `session_ref` 和 `session_lifecycle`。会话成功结算且关联任务和模型调用都已核实时，后继步骤才可提交。结果未知时，工作流保留原会话与证据，停止派发后继步骤。步骤的任务制品仍是启动回执。

会话的模型租约和后续工具任务继承工作流的 UTC 截止时间及同次内核启动的经过时间上限。工作流取消或到期时会取消绑定的原 Session。已派发的操作依照原任务和模型租约的规则结算，未核实的效果保持 `Unknown`。格式 115 拒绝旧写入器打开包含该状态的数据库。[本机会话与工作流验收](../tests/test_workflow_session_join.py)

新 WorkflowRun 在接受时保存同一内核启动内的经过时间截止点，每步 Task 继承该上限。封锁调度期间也会结算到期实例。尚未开始的实例以 `Failed/NotStarted` 和 `DeadlineExceeded` 结算，恢复调度后仍保留原回执与预算；已有在途效果按原 Task 取消和对账规则处理。格式 26 的工作流继续使用原 UTC 截止时间。跨内核启动也使用原 UTC 截止时间。[原生盘回拨验收](../tests/native_grant_deadline.py) · [格式 26 升级](../tests/workflow_deadline_upgrade.py)

步骤遇到可重试的队列或容量限制时，Core 将原 WorkflowRun 移到其他就绪运行之后，并在同一 Linux 启动和时间命名空间内等待 1 秒再试。同一主体的队列满载时，其他有空余配额的运行可以继续。格式 98 持久保存这次延期，重启后沿用原步骤提交键及预算。延期期间继续结算到期运行和已接受子任务的状态。[跨主体调度与重启验收](../tests/test_workflow.py)

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

步骤可以用 `when` 对一个直接前驱已结算的结果执行布尔值或字符串比较。下面的例子选择 `status` 结果中带有给定节点身份的分支。

```json
{"step":"status","pointer":"/authority_id","equals":"node-example"}
```

将此对象填入相应步骤的 `when`，并在该步骤的 `after` 列出 `status`。Core 从前驱 Task 的固定修订取得唯一结果制品，在派发前持久提交所选结果、谓词摘要、Task 和制品引用及值摘要。条件为假时记录 `Skipped`，依赖被跳过步骤的后继也跳过。缺少字段或字段类型不符时以 `PreconditionFailed` 结算。跳过的步骤不申请 Task、审批或派发预算。条件定义使用 DSL 2。数据库格式 24 拒绝使用早期写入者打开带条件的权威。[分支、重启和权限验收](../tests/test_workflow.py) · [格式 23 升级](../tests/workflow_conditions_upgrade.py)

汇合步骤可在 `after_selected` 列出可选分支。它等待这些分支逐一确认成功或跳过，并要求至少一个分支成功才派发。回执保存成功参与的步骤和 Task 修订。所有可选分支都跳过时，汇合步骤以 `NoSelectedPredecessor` 跳过。`after` 继续声明必需前驱，两份清单中的步骤 ID 必须互不重复。

本机 DSL 3 可在步骤的 `inputs_from` 中指定一个必要前驱的制品字符串。以下配置填充 `volume.write` 的 `content`。在同一步的 `input` 中固定卷 ID、预期修订、文件名和字符串占位值，并把 `snapshot` 加入 `after`。

```json
{"inputs_from":{"content":{"step":"snapshot","pointer":"/authority_id"}}}
```

目前可映射 `volume.write.content` 和 `secret.mac.message`。Core 要求委托具备 `task.read`，从同一实例已确认成功的 Task 锁定制品修订。派发前保存来源 Task 与制品引用、值摘要、最终 Input 和请求摘要。恢复时沿用原输入。目标、参数上限、审批和预算均由普通 Task 约束，审批计划绑定最终 Input。路径不存在或值类型错误时，步骤带 `PreconditionFailed` 结算。[权限与输入验收](../tests/test_workflow.py) · [格式 28 升级](../tests/workflow_input_upgrade.py)

`Workflow.Get` 返回运行修订、定义引用、截止时间、分支判定以及各步骤的 Task 回执和已观察修订。可执行的后继在全部必需前驱已知成功后进入准入。前驱出现未知效果时保留 `Blocked/Unknown`，原 Operation 完成对账后继续。用 `Workflow.Cancel` 提交 ID 和当前修订可关闭派发并取消在途 Task，已经发生的效果仍可从原 Task 和 Operation 查阅。[审批、撤权和队列验收](../tests/test_workflow.py) · [早期 DAG 升级验收](../tests/workflow_upgrade.py)

## 本机 Project 与工作项验收

本机所有者通过 `Project.Create` 建立私有 Project。使用 `psiosctl project create` 从标准输入发送 `{"key":"local-ops","title":"本机运维"}`，得到稳定 ID 和资源修订。随后用 `Grant.Issue` 给实际操作者签发 `project.read` 和所需的 `project.manage`、`project.contribute` 或 `project.review`。`tool_targets` 必须列出已有的 Project ID。所有者也需要为日常调用签发相应的限定授权。

`Project.SetMember` 用当前 Project 修订指定 `Viewer`、`Contributor`、`Reviewer` 或 `Manager`。传入显式的 `"role":null` 移除成员。每次项目操作都同时验证当前成员角色和授权。`psiosctl project list --limit 16` 只返回当前可读项目，续页使用上一页返回的 `next_after_id`。Project、Conversation、Task 结果和宿主资源按各自的权限读取。普通 `Resource` 和 `Event` 查询不会返回 Project 的私有记录。

`WorkItem.Create` 指定 `project_id`、`expected_project_version`、`key`、`title`、`assignee_principal_id`，以及有界的可选 `blocked_by`。`criterion` 设为 `{"kind":"TaskResult","tool":"system.status","target_ref":…}`，其中 `target_ref` 是该受管目标的完整引用。依赖必须指向同一 Project 中已存在的工作项。用 `psiosctl work-item list PROJECT_ID` 或 `work-item get ID` 查询工作项、未结束的依赖和当前修订。

指派者用 `WorkItem.SubmitEvidence` 提交自己的已知成功 Task 和实际输出 Artifact 的固定引用。Core 校验计划、执行尝试、操作结算和制品摘要，再把工作项从 `Ready` 推进至 `InReview`。独立评审者需另有针对生产 Task 的 `task.results.read`，可通过 `psiosctl work-item review ID --revision REV accept` 进入 `Verified`，或以 `reject` 退回 `Ready`。项目管理者通过 `work-item close ID --revision REV` 接受结束。依赖项在被依赖的工作项关闭后才可提交。`InReview` 和 `Verified` 保留已提交的制品字节，拒绝或关闭解除保留。关闭后，审核结论以及经独立结果权限核准的证据元数据仍可查询。

指派者离开项目后，项目管理者可用 `psiosctl work-item reassign ID --revision REV PRINCIPAL` 将没有在审证据的 `Ready` 工作项交给有明确项目授权的 Contributor。新指派者仍需通过普通 Task 权限执行任务并提交自己的结果。`work-item abandon ID --revision REV --reason REASON` 终止未关闭的工作项。原审核和证据记录保留，在审产物解除保留，依赖项仍显示阻塞。并发修改使用同一版本检查。

原生 Agent 可在版本化 Skill 中声明 `project.submit_evidence`，再通过普通 Session 的工具派发提交证据。Session 的委托需包含该工具，目标范围含精确 WorkItem ID。执行者还需独立持有对所属 Project 的 `project.read` 和 `project.contribute`，并且是当前指派者。启用 `tool_context_window` 后，模型可从已成功 Task 的上下文获取 `task_ref` 和 `artifact_refs`，连同当前 WorkItem 的 `id` 和 `expected_resource_version` 提交。第三方 Agent 也可用 `Client::submit_work_item_evidence_task` 或 `psiosctl submit project.submit_evidence` 走同一 Task 路径。

Core 在真正派发时重查项目成员、授权和工作项修订。在同一事务提交证据、Task 回执和预算扣减。成功回执含更新后工作项的固定引用和 `InReview` 状态。提交失败时依旧可由原 Task 查询错误，独立评审继续沿用 `WorkItem.Review` 的结果授权。[Agent 工具与故障验收](../tests/test_agent_work_item_evidence.py)

`psiosctl work-item history ID --limit 32` 按修订顺序返回固定快照。下一页使用 `--cursor` 和上页的 `next_cursor`，快照可跨守护进程重启继续。成员名单或授权范围变化时，Core 会重新校验访问，旧游标失效。`psiosctl work-item revision` 从标准输入接收一份完整资源引用，返回当时持久保存的指派、审核与终止状态。精确证据引用仍需生产 Task 的独立结果权限。即使制品字节已回收，授权主体仍可查看原证据元数据。[历史与授权验收](../tests/test_work_item_history.py)

项目管理者可用 `Milestone.Create` 按 `project_id`、`expected_project_version`、`key`、`title` 和 `work_item_ids` 组织 1 至 32 个同一项目下已有的工作项。CLI 提供 `milestone create`、`milestone get ID` 和 `milestone list PROJECT_ID`，查询继承当前项目成员资格和精确读取授权。创建后，Milestone 固定这组工作项的身份。

`psiosctl milestone close ID --revision REV` 需要每个工作项已经通过独立评审并由管理者关闭。Core 记录实际已关闭的工作项版本。`milestone abandon ID --revision REV --reason REASON` 保留结束原因和原工作项关联。Milestone 与工作项各自结算，证据制品的保留继续沿用原 WorkItem 规则。[里程碑生命周期验收](../tests/test_project_milestones.py)

工作项也可采用 `WorkflowTaskResult` 条件，固定 `definition_ref`、`step_id`、`tool` 和 `target_ref`。创建者需有该工作流定义的读取权限，指派者需能按现有 Workflow 权限使用同一份定义。提交证据仍使用原 Task 和 Artifact 引用。Core 核对原 WorkflowRun 中已知成功的确切步骤和生产回执。Project 成员可查看授权范围内的工作项，工作流身份和步骤详情仍按原 Workflow 读取权限裁决。[工作流证据验收](../tests/test_work_item_workflow_evidence.py)

创建工作项时可用 `parent_id` 指向同项目中处于 `Ready` 的已有工作项。父关系随新工作项固定。Core 检查父子关系和 `blocked_by` 共同形成的等待图，拒绝直接或间接循环。每个子项仍各自提交证据、审核并关闭。父项经过评审后，须等直接子项均关闭才能执行最后的关闭操作。`psiosctl work-item children ID` 在当前项目授权范围内分页查询直属子项。[父子关系与关闭验收](../tests/test_work_item_hierarchy.py)

`WorkItem.Assigned` 按当前身份和项目授权分页列出自己负责的 `Ready` 工作项。请求提供 `project_id`，可选 `after_id`、`limit`、`include_blocked` 和 `min_priority`，每页最多 16 项。默认只返回可开始的工作项，打开 `include_blocked` 可查看自己尚被依赖阻塞的工作项。最低优先级适用于直接查询和 Agent 工具派发，分页仍按工作项 ID 排序。CLI 使用 `psiosctl work-item assigned` 读取标准输入的 JSON。

工作项支持 `Low`、`Normal`、`High` 和 `Urgent`。创建时可指定 `priority`，省略时为 `Normal`。项目管理者可用 `psiosctl work-item reprioritize ID --revision REV high` 修改没有保留证据的 `Ready` 工作项。修改仅影响项目工作发现，生成独立修订并保留原优先级和评审历史。可用 `WorkItem.History` 核对每个固定版本。[权限、并发和恢复验收](../tests/test_work_item_priority.py)

原生 Agent 的 Skill 可登记只读工具 `project.list_assigned_work`。工具请求还需提供 `expected_project_version`，委托须包含该工具和所选 Project，实际执行时重新检查项目成员、`project.read`、`project.contribute` 和精确项目修订。查询结果以普通 Task 回执持久保存，继续按 Task 结果权限读取。Agent 可用返回的工作项引用选择后续操作。执行普通 Task 和提交证据仍需要各自的授权。[原生 Agent 领取工作验收](../tests/test_agent_assigned_work.py)

当前支持单节点和单个 Task 或 Workflow 步骤验收条件。多条件验收、外部 Issue 与 CI 同步仍待实现。[多主体与重启验收](../tests/test_project_work_items.py) · [Project 规范](v1/docs/11-workflows-projects.md#118-project-与-workitem)

## 本机会话

节点支持最多 512 个占用名额的会话，单主体最多 64 个。活跃私聊、未到期私聊邀请以及活跃房间会占用相应名额。拒绝、到期和归档释放名额，已授权的历史和原幂等回执继续保留。`Conversation.List` 按会话 ID 排序，默认及最大每页 64 条。续页传回 `next_after_id` 作为 `after_id`，本页按调用时的权限返回。[格式 124 容量迁移](../tests/chat_capacity_upgrade.py)

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

房间席位计入活跃成员和未到期的待接受邀请。重开归档房间时在同一事务内为节点和现有成员重新核对名额。保留正文仍受会话和节点配额约束，新增记录受权威存储余量约束。群聊当前提供本机持久文本、明确邀请和拉取，后续接入线程、订阅投递、附件与 Agent 唤醒。[群聊权限、并发与恢复](../tests/test_chat_rooms.py) · [格式 19 原地升级](../tests/rooms_upgrade.py)

## 通过 MCP STDIO 调用本机只读工具

`psios-mcp` 提供 MCP `2025-11-25` 的本地 C1 工具接入，支持初始化握手、`ping`、`tools/list` 和 `tools/call`。从 Debian 软件包使用 `/usr/bin/psios-mcp`，或通过 `cargo build --locked -p psios-core --bin psios-mcp` 构建。将可执行文件的绝对路径和以下参数填入 MCP 客户端的 STDIO 配置。[MCP 生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) · [STDIO 传输](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) · [工具协议](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

```sh
psios-mcp --socket /run/psios/api.sock --service-uid PSIOS_UID \
  --token-file /path/to/private-agent.token --delegation DELEGATION_ID \
  --tool system.status --tool node.pressure
```

使用专用 `Agent.Register` 凭据，保存为调用用户拥有的私有普通文件。指定 `psiosd` 的实际数字 UID。通过 `Grant.Issue` 为该主体授予 `task.submit`、`task.read` 和希望使用的工具。`tool_targets` 必须覆盖实际目标：`system.status` 使用 `profile-local`，`node.pressure` 使用 `Node.Get` 返回的 Node ID。省略 `--tool` 时选择 `system.status`。两种工具只接受空参数。`tools/list` 核验配置指定的委托及其目标，不创建 Task 或草稿。

调用使用正常的 `Task.Prepare`、`Task.Submit`、任务等候和 `Artifact.Read`，并在返回结果前重新核对指定授权。成功结果包含本地 Task ID 和受权结果。提交回执丢失时，SDK 用原始提交键查找已接受的 Task 或核对是否可重试。连接关闭或客户端取消通知不改变已经提交的只读 Task。它可在授权与 30 秒 Task 截止时间内继续，由 Core 保存结果。本地所有者可按原 Task 修订调用 `Task.Cancel`。适配器对结果等待 10 秒，Core Unix 套接字无法立即连接时返回传输错误，连接建立后沿用 SDK 的读写超时。每个 STDIO 连接串行处理，限制单帧 64 KiB，并按秒限制该连接的工具调用次数。

这个接入面提供同步工具调用。MCP Tasks、跨连接调用映射和 HTTP 沿用各自的独立协议要求。[独立 STDIO 客户端、官方 Python 客户端和原 Task 验收](../tests/test_mcp_stdio.py)

### MCP `2026-07-28`

在相同启动参数中加入 `--protocol 2026-07-28`，选择逐请求协议。每个请求的 `params._meta` 必须携带 `io.modelcontextprotocol/protocolVersion` 和 `io.modelcontextprotocol/clientCapabilities`。自报 `clientInfo` 仅用于协议信息，本地 Agent 凭据决定权限。客户端可以先调用 `server/discover`，也可以直接调用 `tools/list` 或 `tools/call`。发现和工具列表为私有且立即失效，重新读取列表时核对最新授权。省略 `--protocol` 时仍使用 `2025-11-25` 握手。[逐请求协议与版本](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning) · [发现](https://modelcontextprotocol.io/specification/2026-07-28/server/discover) · [缓存](https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/caching)

现代 STDIO 每次只接纳一个在途请求。其他有效请求收到忙碌错误，客户端可在在途请求结束后重试。收到与在途请求 ID 精确匹配的 `notifications/cancelled` 后，适配器取消待发的回复，并在首次提交和回执丢失后的补发前检查取消状态。若 Core 已受理 Task，适配器尝试用其当前修订调用本机取消接口。已完成的 Task 保留其记录。关闭 STDIO 使客户端停止等待，已准入的 Task 仍受原授权和截止时间约束。此模式沿用同一 Core 授权路径与持久 Task，支持同步结果和按请求取消。[STDIO 与取消](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio) · [独立客户端与官方 SDK 2.0.0 验收](../tests/test_mcp_modern_stdio.py)

### 绑定指定资源的只读工具

两代协议都可用 `--volume-usage VOLUME_ID` 公开一个该 Agent 所有的已可用 Volume 的根目录容量，用 `--service-inspect HOST_SERVICE_ID` 公开一个 Core 已登记的 systemd 服务状态。指定资源须在该委托的有效范围内，并由根 Grant 或叶 Delegation 显式列出。授权需包含 `task.submit`、`task.read` 和对应的 `volume.usage` 或 `service.inspect`。可通过 `--tool` 同时选择原来的诊断工具。未指定任何工具时默认启用 `system.status`。

两项资源工具的 MCP 参数均为空。适配器从 `Capability.ResolveLocalReadTools` 取得声明资源的当前精确引用，以该修订构造 Native Task，并在返回结果前复查授权和精确引用。列表只显示当前允许的工具名称，不消耗任务预算或创建任务草稿。Core 的卷持有、扫描限制和 systemd 登记校验照常执行。[实际 Volume 和 systemd 验收](../tests/test_mcp_bound_read_tools.py)

### 现代 MCP Tasks

在 `--protocol 2026-07-28` 上指定 `--tasks-endpoint ENDPOINT_ID` 可启用本机轮询式 Tasks。为每个部署选择稳定的端点标识。调用方在每个 Tasks 请求的 `io.modelcontextprotocol/clientCapabilities.extensions` 中声明 `io.modelcontextprotocol/tasks: {}`。PsiOS 对提供持久调用键的 `tools/call` 采用异步结果，调用键通过 PsiOS 定义的 `_meta.io.psios/mcpInvocationKey` 传递，使用 16 至 96 个 ASCII 字母、数字、短横线或下划线。省略调用键的工具请求仍同步返回。客户端丢失创建回执时，用同一端点、凭据、工具、资源绑定和调用键重连查询原句柄。JSON-RPC 请求 ID 仅用于当次回包关联。[MCP Tasks 2026-07-28](https://tasks.extensions.modelcontextprotocol.io/specification/2026-07-28/tasks)

Core 在同一持久提交中保存原 Task、Operation 与随机外部句柄。`tasks/get` 沿原 Task、Run 和 Operation 读取结果，卷与服务工具还会复核当前资源修订。`tasks/cancel` 通过原 Task 的当前修订请求取消，收到回执后可继续查询实际状态。`tasks/update` 校验句柄和当前原授权，忽略无对应请求的输入。工具级失败返回 `completed` 及含 `isError` 的工具结果。原任务效果未知时继续反映 `working` 和 `Unknown` 原因。

同一 Linux 启动时间域内，重启 Core 或适配器后可继续查询。句柄在内核重启、时间命名空间或权威恢复代次改变后失效。`--tasks-ttl-seconds` 可设置 1 至 600 秒，默认 300 秒。每个 Agent 最多保留 32 个、每个 Core 最多保留 512 个句柄。结果在句柄有效期间免于结果淘汰，轮询不会延长此期限。句柄过期只关闭 MCP 查询。原 Task 历史按原规则保留，相同调用键不会再次受理。[真实 Unix 验收](../tests/test_mcp_tasks.py)

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

`SessionModel` 是 Rust Provider 接口。其他语言可以用同一协议实现 `Model.Claim` 和 `Model.Respond`，提交 `tool`、`finish` 或 `fail` 决策。Model 登记可用 `max_in_flight` 设置 1 至 32 个并发租约，默认 4。每次签发租约计入会话次数和共享的根 `model_calls` 预算。当 Claim 返回 `usage_format: "tokens.v1"` 时，Provider 可以随决策提交 `usage`，其中 `input_tokens` 和 `output_tokens` 各为 0 至 4294967295 的整数。Core 在接受决策的同一事务中更新本次调用原有的 ModelUsage，回执的 `usage_ref` 固定该修订。相同租约只能重放原决策和原用量。缺少报告或租约被围栏隔离时，实际 Token 数保持未知。Token 数由 Provider 声明，供应商账单待实现。当前传输是经过本机认证的 Provider 进程，Provider 的网络隔离由部署管理。Core 没有配置自动云端回退。

`Grant.Issue` 可选 `model_lease_ms` 为同一根预算下的所有会话和委托设置租约时间额度。`Budget.Get` 返回最大额度、已记账时间和剩余时间。领取时预扣本次租约的完整上限，剩余额度较少时缩短签发的租约。当前租约响应获接受时，权威按同一 Linux 启动与时间命名空间的经过时间结算并退回剩余预扣，`usage_ref` 记录 `authority_elapsed_lease_ms`。取消、过期或无法确认结束的租约保留原预扣。暂时占满额度的根预算仍可等待退款，其他预算可继续领取。格式 37 保留原调用次数、在途租约和未指定时间额度的授权。[时间预算与故障验收](../tests/test_model_queue.py) · [真实旧格式升级](../tests/model_time_upgrade.py)

`Session.ModelUsage`、Rust SDK 的 `Client::session_model_usage` 和 `psiosctl session-model-usage ID` 使用与 `Session.History` 相同的读取授权。每个会话报告已签发调用数、当前租约预扣、按经过时间结算的额度、未知调用保留的额度，以及未启用计时的调用数。每次调用会显示当前有效性和实际记账时间。Token 合计只使用已接受的 Provider 报告，同时给出有报告的调用数。根授权下全部会话的共享额度仍可用 `Budget.Get` 查询。模型切换前后的用量分别保存在各自的会话中。[实时租约和读取授权验证](../tests/test_model_provider_outcome.py)

`Session.Wait` 使用与 `Session.Get` 相同的读取授权。请求包含 `id` 和 1 至 10000 毫秒的 `timeout_ms`。省略 `after_version` 时等待会话暂停或终止，传入当前引用的 `version` 时等待更新。到期返回最新授权快照，与 `Session.Get` 返回格式一致。可以用 Rust SDK 的 `Client::wait_session` 或 `psiosctl session-wait ID --wait-ms 5000`，按需追加 `--after-version VERSION`。[授权变更和状态观察验证](../tests/test_model_provider_outcome.py)

模型租约在签发时绑定本节点的 Linux boot ID 和 `CLOCK_BOOTTIME`。同次系统启动中，服务重启保留原期限，系统挂起的时间计入租期。调快或调慢 UTC 时钟不会改变处理当前租约的权限。宿主重启后，旧租约的响应会被拒绝。API 的 `expires_at` 是供客户端显示的 UTC 估算。已取消但 Provider 尚未交回结果的调用，仍占用并发名额至原期限。上一启动或旧格式的调用按原 UTC 估算保留剩余名额，用于记录可能尚在外部执行的工作。绝对授权到期仍按现有 UTC 策略判断。[格式迁移测试](../tests/model_leases.py) · [虚机调时与重启测试](../tests/qemu_boot.py)

格式 68 支持由管理员在 `Model.Register` 为新模型版本设定 `inference_outcome_policy: "Strict"`。在此模式下，签发租约就视为可能开始推理。重取同一个领取键只核对原租约或返回已结算回执。没有收到结算报告的租约到期后，会话暂停并记录 `Unknown`。Watchdog 主动收口，直接指定会话与队列领取也会处理旧租约，随后继续服务其他符合条件的会话。未选择严格策略的模型仍沿用原租约行为。

Provider 可用 `Model.ReportFailure` 提交 `session_id`、`model_ref`、`lease_id` 和 `outcome`。Core 校验 Provider 权限与固定模型版本，在当前租约内接受 `NotStarted` 或 `Unknown`。完全相同的报告可重复提交，不同报告发生冲突。此接口提交的是传输结果，模型生成的 `tool`、`finish` 和 `fail` 仍通过 `Model.Respond`。`NotStarted` 保留已使用的调用次数并结算已过去的租约时间。会话所有者可用版本校验的 `Session.Resume` 重验授权、上下文和剩余预算，随后由新租约重新计费。

`Unknown` 保留本次分配的时间额度与原期限内的 Provider 并发名额。该会话无法领取新租约、恢复推理或切换模型。所有者可取消会话，取消仍保留 `Unknown` 与历史报告。直接取消尚未收到响应的严格租约也保留未知结果。`Session.History` 将这些回执记录在对应轮次的 `inference_failure`。[严格推理结算与故障恢复验收](../tests/test_model_inference.py)

格式 79 的新严格租约在签发时见证该次使用的 `model.serve` 委托、祖先委托、根授权和 Model 修订。重取上下文、接受未结算决策或接受故障报告都要核对原授权仍有效。替代授权只用于新的领取。原委托或根授权撤销后，未结算的调用维持原有期限与并发占用，并按严格模式记录不确定结果。升级时缺少签发授权证据的旧严格租约进入同一处理路径。已提交的原回执继续按当前读取权限提供。[授权替换和重启验收](../tests/test_model_provider_authority.py)

从格式 79 起，严格租约固定发放时实际使用的 `model.serve` 委托、上游委托、根授权和 Model 修订。Core 在重取未结算的上下文、接受决策和接受失败报告前重新核对原授权链。原授权失效后，新授予同一 Provider 的权限只适用于后续租约。失去授权或升级前缺少授权绑定的在途租约保留原预扣与并发占用，直到按既有期限结算为 `Unknown`。已结算回执仍按当前访问权限查询。

`ReferencesOnly` 给模型上一步的结果引用，`LocalToolResults` 允许将授权结果正文交给指定本机 Provider。两种模式都核对当前权限与大小。Session 固定 Skill、工具和 Model 修订。退役依赖会阻止后续派发。重复决策返回原回执，租约被取代后旧决策被拒绝。SDK 遇到已领取但结论不明的模型调用时要求保留决策或等待新租约。工具提交、会话检查点和预算更新使用同一事务。

Provider 可用 Rust SDK 的 `model_turn_durable`，或运行 `psiosctl model-step SESSION --key KEY --decision-dir DIR`。为每个并发工作进程预建独占的服务账户目录（模式 0700），使用支持目录锁、原子重命名和 fsync 的本机文件系统。SDK 在发出决策前将原节点、主体、会话、键、租约和结构化决策写入私有文件并落盘。文件至多 72 KiB，可能包含私有工具输入。重启后用原目录和参数重发已保存的决策，Core 可返回原 Task 回执。同一目录有未决记录时，其他请求会得到 `Conflict`。

格式 95 的严格租约公布迟到证据能力及原 Model 引用。已有决策无法再按原租约结算时，SDK 可调用 `Model.ObserveLateDecision` 递交原会话、租约和精确决策。Core 只接受已经持久结算为 `Unknown` 的严格租约与仍获原授权的 Provider，保存一条决策摘要、字节数及可选用量摘要。会话所有者可经 `Session.History` 核对。相同报告可重放，摘要不同会返回冲突。

Core 记录 Provider 的迟到证据，原 `Unknown`、工具任务和记账继续沿用各自权威记录。传输或写入中断时，SDK 保留原本地决策。收到对应的持久回执或明确终止答复后释放暂存。旧日志沿用原投递格式。[迟到证据验收](../tests/test_model_late_decision.py)

严格 Provider 的领取回执可声明 `decision_rejection_policy: "UndeclaredToolV1"`，Core 在签发该租约时把能力与原 Session 修订一起保存。Provider 可在 `Model.Respond` 中传回该值，使 Core 在工具未由固定 Skill 声明时结算该决策。回执的 Session 是 `Failed / Known`，附 `rejection.code: "ToolNotDeclared"`、决策摘要、字节数及可用的用量引用。Core 不创建 Task、审批或 Operation。同一请求可重放，改变决策或经报告的用量会返回 `IdempotencyConflict`。未传该值的请求仍收到 `PermissionDenied`，可在原租约期限内更正决策。升级前签发的租约沿用原领取回执。

Rust SDK 的 `SessionModel::accepts_undeclared_tool_rejection` 默认关闭。启用后，只有领取回执声明该版本时才会发送该字段。SDK 暂存使用私有格式 5，包含协商后的原请求。收到匹配的拒绝回执即可清理暂存并继续领取。在升级前建立的格式 1 至 4 暂存继续按原规则投递。旧 Core 无法解析该请求字段时，新 SDK 保留格式 5 的原文件。降级前先结算格式 5 的未决投递，或保留原目录，之后用支持它的 Core 和 SDK 恢复。Core 数据库格式沿用现有严格 `Responded` 账本。[真实 SDK 与进程故障验收](../crates/psios-core/tests/model_rejection.rs)

如果推理已经开始、决策尚未持久化，重启返回 `OutcomeUnknown`。损坏记录、授权被撤销及冲突决策会留待核实。需保留这类记录时，停用该工作进程并隔离其私有目录；新键仍受原租约、预算和权限约束。[进程故障验收](../tests/test_runtime.py)


本机 CPU 模型可通过 UID 绑定的 Unix socket 生成结构化盘点决策，复用同一 Provider 身份、租约和决策暂存目录。[本机推理配置与范围](local-inference.md)

新领取回执在模型上下文外携带 `lease_timing`，其中包含实际分配的时间窗口和对应 Linux 时钟域的截止值。Rust SDK 在 `SessionModel::decide_for_lease` 提供租约 ID、轮次、到期时间和当前剩余额度，普通领取与 Provider 队列共用这一入口。SDK 在同一时钟域使用原始 boottime 截止值，在其他时钟域使用 UTC 到期时间与原始窗口中的较短值。本机适配器按剩余额度限制连接和响应等待，并预留 100 ms 提交回执。请求未发出时可报告 `NotStarted`，发送后超时按 `Unknown` 结算。Core 继续执行自己的期限和授权校验。 [真实 Unix 推理传输验收](../tests/test_model_provider_outcome.py)

Core 将新租约的时间元数据随使用量预留一并持久化。同一领取键在租约有效时会返回原时间窗口和上下文，重启后也可按原账目重放。

Provider 可使用 `Model.LeaseStatus` 或 SDK 的 `ModelInvocation::status` 查询已签发的确切 Session、Model 修订和租约。响应只有 `Active` 或 `Ended`。Core 复核当前消费者委托、Provider 授权、会话与工作流状态和截止时间。本机适配器等待推理时定期检查租约，取消或撤权后结束本地连接。已经发送的推理仍按 `Unknown` 记录，并继续遵守原来的结果隔离规则。

`Session.History` 返回模型轮次、子任务和尝试关系。`Session.Cancel` 关闭后续派发并取消已链接的任务。`Session.Extend` 通过 CAS 增加本次会话的轮次上限，根授权额度独立生效。对于可安全重试的失败子任务，`Session.RetryTask` 使用会话 ID、会话预期修订、原 `task_id` 和新 `key`，沿用同一模型决策，在原任务和会话历史中追加记录。接受新的重试前复核主体的 32 个活动会话上限。通过 Provider 队列执行的会话同时遵守固定模型的 128 个活动会话上限。已接受的请求在授权仍有效时可用同一键重获原回执。未知写入继续占用原目标，等待核实。

格式 127 统一 `Session.Open` 与 `agent.session.start` 的消费者目标范围。若原委托设置了 `tool_targets`，其中须同时包含固定的 Skill 和主 Model，Provider 仍须具备覆盖该 Model 的 `model.serve`。领取、模型响应、恢复和工具执行均复核会话的原委托。升级后，缺少任一目标的旧队列会话会在被扫描时进入 `ModelAuthorizationDenied`，所有者仍可查询历史或取消。需要继续执行时，可使用完整的委托新建会话。 [Unix API 验收](../tests/test_model_queue.py)

### 在安全点暂停会话

`Session.Pause` 接受 `id`、`expected_resource_version` 和 `reason`。CLI 使用 `psiosctl session-pause ID --revision VERSION --reason REASON`。可领取的会话立即进入 `Paused`，其 `pause_reason` 为 `OperatorPaused`。正等待工具任务的会话先保存 `operator_pause.after_task_id`，原 Task 按现有审批和授权流程结算。若原任务成功，会话在下一轮前暂停。失败或取消仍使用原任务终态，结果未知时继续等待原操作对账。

在途模型调用须按现有租约规则结算。暂停期间的会话不产生新模型租约，原预算、结果和附加输入仍按其当前保留规则处理。使用 `psiosctl session-resume ID --revision VERSION` 复核原委托和上下文，再恢复领取。领取时继续执行节点的全局准入限制。轮次额度耗尽时可先用 `Session.Extend` 增加上限，此操作保留暂停。[安全点验收](../tests/test_agent_safe_point_pause.py)

### 有界的多轮工具上下文

`Session.Open` 和 `agent.session.start` 可指定 `tool_context_window`，保留最近 1 至 4 个已确认成功的工具轮次。原来的 `last_tool` 继续提供最新工具结果，`previous_tools` 从旧到新排列其余轮次，包含固定的 Task 修订、制品引用和实际尝试号。省略该字段继续使用单条上下文。普通调用、自动队列和原有的显式模型交接共用同一窗口。

`LocalToolResults` 会保留活动窗口内的原制品，轮转或会话终结后允许按常规策略回收。每次领取复核原工具计划的权限范围和制品可用性，并在读取前检查剩余的模型上下文额度。失败后的 `Session.RetryTask` 会在重新准入时移除具有正式回收回执的历史制品，并重新核验余下制品。遇到缺失或损坏时，原会话保持失败状态。

`ReferencesOnly` 提供原结果引用，结果中的可用性固定为任务原修订所记录的状态。通过 `Artifact.Get` 在独立授权下查询当前可用性或访问正文。

格式 65 把领取时的 Session 修订、ModelUsage 与规范化上下文摘要一起提交。领取结果和 `Session.History` 提供 `context_digest`。重发领取请求时从原修订重新生成上下文，校验摘要并复核当前权限。内容缺失、检查点损坏或上下文超限会以明确原因暂停自动队列中的该会话，其他会话可继续领取。原调用额度和工具回执仍可核对。[Unix API 与进程故障验收](../tests/test_agent_tool_context.py) · [格式 64 原地升级](../tests/agent_context_upgrade.py)

格式 107 也为省略 `tool_context_window` 的普通领取固定签发时的 Task 修订及内部上下文摘要。领取及历史的默认 JSON 结构保持原样。若后续回收制品，`ReferencesOnly` 重放保留原租约的元数据视图，制品接口仍报告当前可用性。`LocalToolResults` 重放须重新通过授权并核验现有正文。对于升级前尚未结算的默认领取，若 Task 已改变修订或原视图无法核实，重放返回 `OutcomeUnknown`，此前已保留的模型决策仍可通过原租约结算。 [原地升级验收](../tests/default_context_upgrade.py)

### 显式授权私有 JSON 输入

直接调用 `Session.Open` 可提交 `input_artifacts`，每项由唯一 `name`、完整资源引用 `reference` 和 `kind: "Json"` 组成。支持至多四个会话所有者发布的 `application/json` 制品，每份原始数据至多 16 KiB。Core 在会话建立和 Provider 领取时校验 JSON、原摘要及合并后上下文预算。会话表和历史查询保存引用及上下文摘要。

原始会话委托需具备 `session.context.read`、`artifact.read`、正常的会话与工具操作权限。它的 `tool_targets` 必须明确包含所选模型、每个制品和节点，以及工具所需的作用域。该模型的注册 Provider 必须在领取请求中显式提交 `accepts_session_inputs: true`，随后通过 `context.inputs` 接收 `name`、`reference`、`kind` 和 `value`。SDK 的 `SessionModel::accepts_session_inputs` 可启用此协商。Core 会在重放和提交新决策前复核原委托。活动会话固定这些精确制品，终态释放。重试会话中的失败 Task 时，会在同一受理事务内重新验证并固定原制品。格式 100 支持直接领取，格式 104 扩展至 Provider 队列并阻止旧 Core 打开升级后的状态。私有输入要求固定模型，通用本机盘点适配器继续使用默认能力声明。 [Unix API 验收](../tests/test_agent_session_inputs.py)

格式 106 允许 `agent.session.start` 在工具输入中使用同样的 `input_artifacts`。启动 Task 的原始委托同时覆盖此工具、Skill 和私有输入所需的作用域。实际受理后，启动 Task 保留其精确制品，直到失败、取消或移交给 Session。成功时，Core 在同一个持久事务内创建 Session、移交保留关系并发布只含 `session_ref` 的启动回执。

Schedule 和 Workflow 可以固定同样的制品引用，并通过各自已有的 Task 受理路径启动会话。保留从每次实际受理开始。若在该次受理之前撤回制品，该次启动会失败。Workflow 的输入映射继续只接受现有的受支持文本参数。 [启动、撤权与恢复验收](../tests/test_brokered_session_inputs.py)

### 预授权本机模型交接

`Session.Open` 和 `agent.session.start` 可用 `fallback_model_refs` 固定至多三个备选 Model 修订。会话选用 `ReferencesOnly`，使用的委托须包含 `session.failover` 并在目标范围纳入主模型及每个备选模型。登记的模型须共享本机认证接口和模态。交接时再次检查所选模型修订、Provider 授权和当前上下文大小。

调用 `Session.UseFallback` 时提交原会话 ID、当前修订和备选 Model 引用。Rust SDK 使用 `Client::use_session_fallback`，CLI 使用 `psiosctl session-use-fallback ID --revision VERSION MODEL_REF_FILE`。Core 在一个事务中关闭原会话并创建后继，保留已确认的 Task 引用和结果保留关系。交接沿用原共享预算，已使用的轮次也计入上限。原 Provider 的未结算租约立即失去提交权限。该次调用的预扣仍计入共享预算，原 Provider 的并发名额占用至租期结束。等待 Task 或含未知效果的会话先走原 Task 结算流程。此接口使用显式交接，备选 Provider 按原会话的队列设置接手。[双 Provider、任务和重启验收](../tests/test_model_fallback.py) · [格式 46 升级](../tests/model_fallback_upgrade.py)

严格租约已收到原 Provider 有授权且可核验的 `NotStarted` 回执时，所有者可直接交接至预授权备选模型。原 Model 之后退役仍可核验该历史回执。Core 验证当前会话委托、备选模型的 Provider 权限、剩余预算与原无效果回执，保留原轮次和已确认工具结果。后继会话在实际领取新租约时计费。原租约尚在途或结果为 `Unknown` 时仍拒绝交接。[严格失败回执及备选模型验收](../tests/test_model_fallback.py)

### 持久的 Provider 队列

调用 `Session.Open` 时显式提供 `"provider_queue":true`，允许选定 Model 的 Provider 自动领取该会话。Provider 可用独立凭据和上述 `model.json` 运行以下命令。省略 `--once` 时，进程持续等待并处理可领取的轮次。

```bash
install -d -m 0700 "$workdir/worker-decisions"
providerctl model-worker "$workdir/model.json" --decision-dir "$workdir/worker-decisions"
```

API 的 `Model.ClaimNext` 接受 `model_ref` 和 `key`。`Model.WaitNext` 另接受 1 至 10000 毫秒的 `timeout_ms`。返回 `available:false` 表示暂无可领取轮次。领取按进入就绪状态的顺序执行，等待工具或人工审批期间跳过该会话。每个 Model 最多接纳 128 个自动领取会话，沿用单次租约并发限制、会话轮数和共同根预算。撤权、依赖变更或额度耗尽的会话进入带原因的暂停状态，其他会话仍可领取。

这两个队列接口都接受可选的 `accepts_session_inputs`。未声明支持的 Provider 会跳过附有私有输入的会话，并可继续领取随后就绪的普通会话。重复领取键始终绑定原轮次。未声明支持时，重放仍在途的私有轮次会得到 `UnsupportedCapability`，已结算轮次仍可返回原回执。

使用 `LocalToolResults` 的会话在固定工具制品不可读取时暂停，记录 `ResultUnavailable` 或 `ResultEvicted`。自动领取继续服务其他会话。恢复原制品后，会话所有者用 `id` 和当前 `expected_resource_version` 调用 `Session.Resume`。服务重新检查原工具输出、Skill、Model 和委托，随后将会话放回就绪队列。模型调用额度在新轮次实际获发租约时扣除，原工具执行和回执仍可查。[缺失上下文验收](../tests/test_model_queue.py)

系统在签发租约和计入预算的同一事务中保存 Model 与领取键映射。领取请求重放只能恢复该轮次。只有已落盘的决策可重发，丢失决策的轮次等到原租约失效后才可申请新的额度。Rust SDK 的 `model_next_durable` 先尝试交付已保存的决策，再领取新轮次。若原会话授权已撤销，`Model.ClaimStatus` 通过 Model、领取键和租约 ID 返回该次交付的 `Pending`、`Settled` 或 `Fenced`。该方法只返回状态，任务回执和上下文仍受原会话授权约束。每个工作进程需要独占的暂存目录。[竞争与崩溃验收](../tests/test_model_queue.py)

## 定向通知与验证

接收方以 `mail.receive` 调用 `Mailbox.Bind`，指定 `name` 和 `sender_principal_id`。指定的发送方以 `mail.send` 调用 `Signal.Send`，提供 `mailbox_id, key, topic` 和可选 `resource_ref`。当前 topic 为 ObservationReady、ReviewRequested、WorkAvailable。附带的资源必须已经对双方可见。接收方用 `Mailbox.Read` 读取，用 `Mailbox.Ack` 的 `previous, through` 确认；崩溃后可重读。会话创建可带 `trigger_signal_ref`，同一信号最多触发一个会话。每个信箱最多有 256 条未确认通知，历史上限 16384 条。

格式 110 的 `ReviewRequested` 可指向精确的 `InReview` WorkItem 修订。已提交证据的 Contributor 通过受限信箱发送，接收方须独立拥有当前 Reviewer 角色及原 Task 结果读取权限。Core 固定请求时的 Project 名册修订。名册变化会使此前请求失效，包括移除后重新加入。后续投递重新检查访问权和工作项状态。失效通知只保留原序号用于确认。普通资源和事件查询不会返回审核请求或回执。

独立评审者可在 `WorkItem.Review` 中提供 `request_signal_ref`。Core 在同一事务提交当前工作项的条件审核和固定回执。双方可在现有授权下调用 `Signal.ReviewReceipt` 查询原回执。信箱确认有独立的读取水位，审核回执由 Project 权威生成。CLI 提供 `work-item request-review`、`work-item review --request-signal FILE` 和 `work-item review-receipt`。[权限、崩溃与原子回执验收](../tests/test_work_item_review_signals.py)

常驻 Agent 可用 `psiosctl inbox MAILBOX --wait-ms 5000 --limit 16` 等待投递。省略 `--wait-ms` 即刻读取。API `Mailbox.Wait` 接受 `id`、1 至 10000 毫秒的 `timeout_ms` 和可选的 `limit`（1 至 128）。返回值包含 `items`、`acked`、`lifecycle` 和 `mailbox_ref`。处理完一页再用该页的 `acked` 作 `previous`，最后一项的 `signal.seq` 作 `through` 确认。接收端在确认前退出，可在重启后重新投递同一页。空的 Open 页代表超时，Closed 信箱可以继续读取积压的信号，直到返回空页。阻塞期间的撤权会终止本次等待。等待名额已满时返回 `QueueFull`。

CI 同时执行原有契约、故障与 API 测试，以及持久会话、授权链、真实 systemd、从旧二进制升级和两个 OCI 镜像间的切换回退。在线多卷应用一致性、异机备份、多节点 fencing、整机断电、跨主机 Provider 及云模型账单仍需对应驱动与环境。[平台测试](../tests/test_platform.py) · [Agent 测试](../tests/test_runtime.py) · [systemd 故障测试](../tests/services.py) · [升级测试](../tests/upgrade.py) · [OCI 测试](../tests/oci.py)
