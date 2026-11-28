# 使用本机语言模型执行资产盘点

`psiosctl model-step` 的本机推理适配器通过 Unix socket 调用支持结构化 Chat Completions 的进程。当前配置支持 `system.status`、`node.inventory`、`node.devices` 及结束或失败决策。三项盘点工具均使用空对象作为输入。Core 负责授予会话和工具权限、签发租约、结算模型轮次、提交 Task 并保留结果。CLI 的普通节点和存储操作在推理进程停止时仍可使用。[会话及授权](core-runtime.md)

在支持的 Linux 上，以模型服务账户启动可信的 llama.cpp 运行时，用 `--host /绝对路径/inference.sock` 绑定受保护目录中的 Unix socket。已验证的 CPU 组合使用 SmolLM2-135M-Instruct Q4_K_M、`--alias psios-small` 和至少 2048 token 上下文。不要将模型凭据、租约或 PsiOS 令牌加入提示。下载校验在启动前完成，推理进程可以在禁网环境运行。[验收所用版本、来源和摘要](../tests/fixtures/local-inference.json)

分别登记 Agent 和 Provider。给 Provider 授予 `model.serve`，给 Agent 授予 `session.run`、`task.submit`、`task.read` 及所需盘点工具。发布 Skill，登记绑定 Provider 主体的 Model，使用固化的两个资源引用打开 Session。将以下设置保存成 Provider 的本机配置。将 `model_ref` 完整替换为 `Model.Register` 的实际回执，将 `server_uid` 替换为运行推理服务的数字 UID。

```json
{
  "socket": "/run/psios-inference/inference.sock",
  "server_uid": 1000,
  "model_ref": {"resource_id": "MODEL_ID", "version": "MODEL_VERSION", "digest": "MODEL_DIGEST"},
  "model_alias": "psios-small",
  "max_tokens": 112,
  "timeout_ms": 30000
}
```

```sh
install -d -m 0700 ./provider-decisions
psiosctl --token-file ./provider.token model-step "$SESSION" --key "$TURN" \
  --local-model ./local-model.json --decision-dir ./provider-decisions
```

每次调用处理一个轮次。传输在发送上下文前核对内核提供的服务 UID 及绑定的 Model 修订；模型输出必须是本次 Skill 允许的结构化决策。模型返回尚未完成的输出、错误别名或超时会造成该次推理失败。重试同一键时使用 Core 回执或已持久化的决策；已经开始推理但缺少决策时，SDK 返回 `OutcomeUnknown`。Core 签发租约时扣减一轮调用额度。支持用量回执时，适配器从推理响应读取提示和生成的 Token 数，随原决策持久交付。通过回执的 `usage_ref` 可读取绑定预算、报告者和计数的 ModelUsage 修订。缺少报告时保留未知值；兼容旧 Core 时仅交付决策。Token 上限及总请求期限可配置，供应商账单和运行时权重证明仍待实现。此适配器不会选择其他端点或发起 TCP 请求。[真实推理与故障测试](../tests/local_inference.py)

打开会话时也可请求 `"provider_queue":true`。把 `Model.Register` 的完整回执保存为 `model-ref.json` 后，使用专用暂存目录运行独立工作进程。该进程会等待所选 Model 的会话并复用同一个推理端点。

```sh
install -d -m 0700 ./worker-decisions
psiosctl --token-file ./provider.token model-worker ./model-ref.json \
  --local-model ./local-model.json --decision-dir ./worker-decisions
```

## 在原生 amd64 磁盘中运行 CPU 推理

原生盘可选用独立的静态 llama.cpp 引擎。引擎、权重与 Core 分开保管，首次启动仍按原生盘流程进行。生成新盘时可用 `--home-mib 192` 为以下模型预留空间。已经部署的磁盘应核对实际可用空间。[原生磁盘容量](native-disk.md#生成新盘)

在可信的 amd64 Linux musl 构建环境中准备 GCC、G++、Linux 内核头文件、CMake、Ninja、GNU tar 及 binutils。提前取得锁定源码后，构建器可完全离线运行。它核对源归档，构建通用 amd64 CPU 引擎，检查 ELF 没有动态加载器和动态依赖，并输出校验清单和实际工具链版本。可用 `CMAKE_BUILD_PARALLEL_LEVEL` 控制并行编译数。

```sh
curl -fL -o llama-source.tar.gz \
  https://codeload.github.com/ggml-org/llama.cpp/tar.gz/f5e6915c28f10330885fcb9243a93ae98d3115ab
tools/build-local-inference llama-source.tar.gz psios-llama-server-amd64.tar.gz
mkdir engine
tar -xzf psios-llama-server-amd64.tar.gz -C engine
(cd engine && sha256sum -c SHA256SUMS)
```

从固定来源获取 SmolLM2 权重，在转入 PsiOS 前按 SHA-256 核验。把引擎和权重复制到普通操作账户拥有的私有目录。以该账户执行下面的命令，将打印的路径和账户 UID 用于 `--local-model` 配置。前台进程提供模型推理，普通 Core 管理命令可在另一会话执行。[模型修订、许可证和摘要](../tests/fixtures/local-inference.json)

```sh
umask 077
endpoint_dir=$(mktemp -d /tmp/psios-inference.XXXXXX)
printf 'socket=%s/inference.sock\nserver_uid=%s\n' "$endpoint_dir" "$(id -u)"
./llama-server -m ./SmolLM2-135M-Instruct-Q4_K_M.gguf \
  --host "$endpoint_dir/inference.sock" --alias psios-small \
  -t 2 -c 2048 -np 1 -ngl 0 --no-webui
```

当前原生验证覆盖 CPU、本机 Unix socket 及手动启停。自动服务部署、GPU 后端和远端模型分发仍待实现。验收脚本使用独立 OVMF 来宾和原生 Core 制品，两项输入通过只读磁盘转入，随后检查直接调用、队列调用和重启后的持久化结果。[原生推理验收](../tests/native_local_inference.py)
