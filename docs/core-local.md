# PsiOS 单机 Core

`psiosd` 把资源、任务、授权账本和事件保存在同一个 SQLite 数据库中。`psiosctl`、Rust SDK 和原生 Agent 都通过 Unix socket 访问这个权威服务。这个实现使用 v1.0.0 契约，运行于 Linux。OCI 测试需要可访问的本机 Docker Engine。基础管理、存储和 Agent 状态任务可以独立运行。

节点调度、受管 systemd 服务、分块卷数据、应用版本切换和持久 Agent 会话见[宿主与 Agent Runtime 指南](core-runtime.md)。

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

`kill -TERM "$daemon_pid"` 请求关闭。再次使用同一目录启动会恢复队列、保留原任务身份和已接受回执。系统服务安装时，把二进制放入 `/usr/local/bin`，以专用服务账户初始化状态目录，并将配置安装在 `/etc/psios/core.json`。服务单元与配置示例使用 `psios` 用户、`/run/psios/api.sock`、`/var/lib/psios` 和独立的 `/var/lib/psios.witness`。[systemd 单元](../contrib/systemd/psiosd.service) · [配置示例](../contrib/systemd/core.json)

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
| Approval.Decide、Run.Get、Operation.Get、Operation.Reconcile | 决策绑定已显示的计划摘要、审批请求版本和单次使用单元。Reconcile 使用 Operation 的 id 和 expected_resource_version。可核实文本写入、分块追加、受管服务启停和 OCI 容器删除后的停止结果。 |
| Artifact.Read、Secret.Put、Secret.Revoke、Audit.List | Artifact.Read 使用 reference。Secret.Put 接受 name/value，仅返回 SecretRef。secret.mac 可使用秘密计算 HMAC。Audit.List 支持 after 和 limit。 |
| Tool.Call、Capability.Discover | 即时入口支持 system.status。执行其他注册工具通过 Task.Submit，服务端承担审批、预算、派发和回执保存。 |
| StoragePool.Get、StoragePool.Configure、Application.Configure | 本地池实时报告容量。所有者用 CAS 开关新卷分配。应用停止后可用 CAS 修改非秘密环境配置。 |

服务嵌入仓库中的 JSON Schema，无需在线获取 Schema。CLI 的 `validate task-submit FILE` 可检查结构和提交摘要。摘要使用带用途前缀的 JCS 和 SHA-256。已启用的写接口会进一步检查引用类型、当前授权、版本与真实资源身份。[v1 Schema](v1/schemas/README.md) · [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785)

## 应用、卷与审批

| Task 工具 | input |
| --- | --- |
| system.status | 空对象 |
| secret.mac | secret_id、message |
| volume.create | name |
| volume.read、volume.write | id、expected_resource_version、单个文件名 name；write 另需 UTF-8 content，最大 64 KiB |
| volume.delete、volume.activate、backup.create、backup.restore、backup.delete | id、expected_resource_version。restore 的目标是 Backup，产生新 Volume，读取核实后通过 activate 开放写入。 |
| application.install | 本地 OCI 镜像配置摘要 image_id、name、release_version、license_expression、argv、environment、cpu_millicores、memory_max_bytes、pids_max、ephemeral_max_bytes，可选 volume 指向卷 id 和 expected_resource_version |
| application.start、application.stop、application.uninstall、application.health、application.logs | 应用 id、expected_resource_version |

volume.write、volume.delete、volume.activate、backup.delete、application.install、application.start 和 application.uninstall 会等待审批。读 Task 的 ApprovalRequired 条件可取得审批请求 ID，Resource.Get 读取该请求和 OperationPlan。检查 plan 的 allowed_effects、输入、目标版本、预算和期限后，向 Approval.Decide 提交审批请求的 id、expected_resource_version、plan_digest 和布尔值 approve。服务端在实际执行前再次验证这些条件。

单机 OCI 适配器接受已在本机引擎中的 Linux 镜像配置摘要，生成固定的 ApplicationManifest 与 ReleaseSpec。Deployment 固定 Release、配置和卷世代，RuntimeInstance 记录引擎容器身份及运行观测。启动从该 Release 解析 argv 和资源限制，在引擎创建容器后核验配置，再启动进程。隔离配置包括非 root UID、只读根、禁用网络、移除 capabilities、no-new-privileges、CPU/内存/PID 限制和有界 tmpfs。health 提供进程存活观测，logs 返回有界尾部。支持显式 /data 绑定，拒绝隐式镜像卷。安装来源记录为 UserSuppliedUnverified，组件清单覆盖所见镜像层。

应用停止后，backup.create 才能取得排他占用。后端保存文件内容、长度、模式、目录和摘要，支持服务 UID/GID 的普通文件及目录。遇到符号链接、硬链接文件、扩展属性或嵌套挂载会拒绝备份。单份备份上限 256 MiB、4096 个条目。恢复核验权威记录的清单摘要和每个文件的内容摘要，在暂存目录完成后发布新卷。故障域为同一本机文件系统，故障保证为单卷离线 crash_consistent。卸载应用保留卷，原卷与恢复卷具有不同身份及数据世代。

## 持久状态与恢复

任务元数据、输入保留、幂等回执、预算预留、Operation 及事件在同一个 SQLite 事务内提交。每个资源版本同时是一条有序持久事件。服务保存历史修订、操作记录和取消墓碑。每个主体最多保留 4096 个不同输入，总输入上限 64 MiB。队列另有独立上限。当前版本尚无归档与检索历史的 GC。

领取 Run 与签发宿主调用分属两个事务。派发事务先记录单用途 ExecutionPermit、Effect、Run 代次、审计及必要的资源占用。进程在领取后丢失，调度器可以在预算内建立新 Run。已派发的写操作丢失回执时，查询会返回 Unknown，相关资源保持占用。对 volume.write，可用 Operation.Reconcile 在原卷身份和占用下按已批准输入核实文件内容。服务附加证据并结算任务。分块追加按长度和范围内容核实，受管服务可用新观测对账。其他未知宿主效果保留待核实。取消中的调用收到成功回执时会同时显示 Cancelled 和 Known，并保留结果。

独立 witness 目录保存提交标记、密钥与本机所有者凭据。服务先写入本次事务的候选标记，提交数据库，再固定已确认标记，之后才能返回回执或调用宿主。启动时核对标记，可恢复中断提交的相邻两种状态。单独回滚数据库或替换密钥会触发 RecoveryRequired。运行 `psiosd check` 或 `psiosd recover-owner` 需要停服并持有可信状态目录。后者轮换本机所有者令牌。见证和数据库被一起回滚、整机丢失和跨节点接管需要独立信任锚及新的恢复流程。

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
