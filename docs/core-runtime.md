# 单节点宿主与 Agent Runtime

本页接续[本机 Core 启动指南](core-local.md)。服务只接受本机 Unix socket 上经过认证的请求。Task、Run、Operation、模型步骤、接收通知和用户操作都保留各自的主体和资源引用，可以通过同一套 Resource 与 Event 接口追查。

## 从 PR #1 升级

停服并按本机恢复要求保存 state 和 witness。新版首次打开旧库时核对数据库、见证和密钥，在一次带见证的事务中升级至格式 5。旧格式 1 还会迁移到多工作线程的资源和预算结构；已有授权和预算原样保留。格式 3 登记了网络接口观测，格式 4 增加备份核验，二者均需显式授权。格式 5 给模型租约记录 Linux 启动身份和经过时间期限。升级前已经结算的模型回执可继续查询。旧库中尚未结算的调用缺少启动证明，Provider 需要使用新的领取键并消耗新一轮预算。旧二进制拒绝打开超出自身支持范围的格式。离线 `psiosd verify` 报告是否需要升级，`check` 或启动服务会执行升级。已有 Agent 的权限、预算、凭据、取消墓碑、审批和任务身份按原记录保留。涉及绑定卷的新派发会检查卷版本。若旧应用启动计划缺少这个前提，需要提交新计划。

## 宿主与调度

- `psiosctl node` 返回本机权威分配的 Node 身份、Linux boot ID、工作线程数、各阶段队列数量和调度模式。`node.inventory`、`node.processes` 和 `node.devices` 是独立授权的 Task 工具。进程查询支持 `after_pid` 和 `limit`。boot ID、PID 和 start ticks 共同标识一次进程实例。块设备来自 sysfs，结果和进程页都是调用时的观测。
- 配置中的 `workers` 取 1 至 16，默认 4。工作线程共享事务权威，在数据库锁外等待宿主调用。调度器在有积压的主体之间轮转。同一卷、应用或服务上的冲突写入继续受持久占用保护。所有者用 `Node.Configure` 提交 Node 的 `expected_resource_version` 和 `scheduling`，取值为 `Cordoned` 或 `Enabled`。暂停领取期间可接受任务，在途工作可结算，超时及撤权仍会释放预留。
- `Task.History` 使用 `id` 和可选 `limit`。下一页带回 `through_version` 和服务器返回的 `next_after_version`（请求字段为 `after_version`），读取同一历史区间。`Task.Archive` 使用任务 ID 和修订，将已结算的终态移出调度表，保留幂等回执、历史、Run 和结果。当前仍需为结果及审计历史预留磁盘空间。
- `Event.Wait` 复用 Event.Poll 的授权游标，支持 1 至 10000 毫秒的 `timeout_ms` 和可选 `limit`。`psiosctl watch CURSOR --wait-ms 5000` 可直接使用。提交后通知订阅者，等待时释放数据库锁，撤权和超时重新检查。事件和信箱共用每个守护进程的 8 个等待名额。

### 网络接口与地址

`node.interfaces` 通过 Task 返回本地网络命名空间中接口的 ifindex、名称、MTU、链路类型、运行状态、标志和已分配的 IPv4/IPv6 地址。地址含前缀长度和 scope，点对点配置另有 peer。结果同时记录 Linux boot ID、网络命名空间 inode 和观测时间。网卡驱动与地址由现有 Linux 网络管理器配置。此接口仅观测，DNS、路由、远端连通性及配置事务尚待实现。

详细拓扑需要显式授权。以下命令接续本机指南中的 `ctl` 函数。为其他 Agent 颁发时替换 principal_id 并使用其凭据执行 Task。

```bash
node=$(ctl node | jq -er .reference.resource_id)
grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",actions:["task.submit","task.read","node.interfaces"],
  tool_targets:[$node],ttl_seconds:300,units:8}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
task=$(echo '{"limit":32}' | ctl submit node.interfaces --delegation "$grant" --key interface-page-1 | jq -er .task_id)
ctl wait "$task" | jq '{reference:.result.artifact_refs[0]}' | ctl call Artifact.Read
```

使用结果的 `next_after_ifindex` 作为下一次 Task 的 `after_ifindex`，并为新页提供新的幂等键。单页上限 128 个接口，底层输出上限 192 KiB。超限或驱动不可用时返回错误。各页反映各次采样时的配置；接口热插拔后 index 可以复用。打包单元允许读取 netlink，服务以普通专用账户运行。

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
启动流程会在核验容器配置后、请求 Docker 启动前，将引擎 ID、容器 ID、原 Operation 和应用、卷修订存入带见证的检查点。若启动回执丢失，先运行新的 `application.health`，再向 `Operation.Reconcile` 提交该 ArtifactRef。只有同一引擎确认原容器正在运行、镜像与隔离配置匹配，才会一次性登记运行实例、挂载卷并结算原任务。此过程不会重复启动容器。容器只完成创建、已经退出、引擎不可达，或中断发生在检查点写入之前时，原操作继续保留占用。旧版产生的未知启动也需要另行核实。

## 身份与权限

`Grant.Issue` 可提供 `tool_targets`，把工具限制在明确的资源 ID 集合中。对已绑定卷的应用启停，授权需覆盖应用和卷。通用资源读取仍按独立权限和资源所有者裁决。设置 `allow_delegation=true` 和 `max_delegation_depth` 可启用最多 4 层转授。子委托可以收窄动作、目标、期限和剩余层数，共享根预算。沿用旧配置的 Grant 保持原有的单层状态委托范围。每次派发检查完整祖先链及其主体。

`Secret.Rotate` 接受秘密 ID、预期修订和新值。新密文使用新随机 nonce，并将秘密身份及密钥版本纳入认证数据。轮换前接受且尚未派发的旧计划会失效。旧库中的密文可以继续使用并原地轮换。撤销的秘密不能通过轮换重新激活。

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

## 定向通知与验证

接收方以 `mail.receive` 调用 `Mailbox.Bind`，指定 `name` 和 `sender_principal_id`。指定的发送方以 `mail.send` 调用 `Signal.Send`，提供 `mailbox_id, key, topic` 和可选 `resource_ref`。当前 topic 为 ObservationReady、ReviewRequested、WorkAvailable。附带的资源必须已经对双方可见。接收方用 `Mailbox.Read` 读取，用 `Mailbox.Ack` 的 `previous, through` 确认；崩溃后可重读。会话创建可带 `trigger_signal_ref`，同一信号最多触发一个会话。每个信箱最多有 256 条未确认通知，历史上限 16384 条。

常驻 Agent 可用 `psiosctl inbox MAILBOX --wait-ms 5000 --limit 16` 等待投递。省略 `--wait-ms` 即刻读取。API `Mailbox.Wait` 接受 `id`、1 至 10000 毫秒的 `timeout_ms` 和可选的 `limit`（1 至 128）。返回值包含 `items`、`acked`、`lifecycle` 和 `mailbox_ref`。处理完一页再用该页的 `acked` 作 `previous`，最后一项的 `signal.seq` 作 `through` 确认。接收端在确认前退出，可在重启后重新投递同一页。空的 Open 页代表超时，Closed 信箱可以继续读取积压的信号，直到返回空页。阻塞期间的撤权会终止本次等待。等待名额已满时返回 `QueueFull`。

CI 同时执行原有契约、故障与 API 测试，以及持久会话、授权链、真实 systemd、从旧二进制升级和两个 OCI 镜像间的切换回退。在线多卷应用一致性、异机备份、多节点 fencing、整机断电、跨主机 Provider 及云模型账单仍需对应驱动与环境。[平台测试](../tests/test_platform.py) · [Agent 测试](../tests/test_runtime.py) · [systemd 故障测试](../tests/services.py) · [升级测试](../tests/upgrade.py) · [OCI 测试](../tests/oci.py)
