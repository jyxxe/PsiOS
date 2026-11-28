# 使用本机语言模型执行资产盘点

`psiosctl model-step` 的本机推理适配器通过 Unix socket 调用支持结构化 Chat Completions 的进程。当前配置支持 `system.status`、`node.inventory`、`node.devices`、`node.pci` 及结束或失败决策。盘点工具使用空对象作为输入。PCI 观察须由 Skill 明确允许，委托的 `tool_targets` 须包含当前节点以及会话采用的 Model 和 Skill。Core 负责签发租约、结算模型轮次、提交 Task 并保留结果。CLI 的普通节点和存储操作在推理进程停止时仍可使用。[会话及授权](core-runtime.md) · [PCI Task](core-runtime.md#pci-设备与驱动观测)

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
