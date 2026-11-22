# XeLab 历史归档清点与复用边界

**结论：这个 ZIP 是 66 个可选择模块的历史导出集合，不是带完整 Git 历史的当前 PsiOS 仓库。** 本次检查没有运行归档代码，没有访问其来源仓库，也没有执行测试；所有复用决定仍为 `PendingCurrentRepositoryAudit`，工程状态 `NotVerified`。

## 1. 可复核输入与方法

- 原上传：`xelab-package.zip`，`source_file_id=file_000000001c2c8211adadf0ee6c507526`。
- 归档：[xelab-package-history.zip](../sources/archive/xelab-package-history.zip)，大小 **3,174,454 字节**。
- SHA-256：`e6d1b43ed5644e8f7f8e7eeaf72495e7471887918b5afb012970ad32c910b599`。
- 原字节不修改；读取 ZIP 目录、每个模块 README、metadata.json，以及 Cargo/package/许可证文本和现有历史报告。没有将包解压为可执行工作区、安装依赖或执行脚本。
- 完整静态记录：[xelab-archive-inspection.json](xelab-archive-inspection.json)；来源身份：[source-inventory.json](../sources/source-inventory.json)。

## 2. 实际目录和模块结构

| 层级 | 实际观察 | 含义 |
|---|---|---|
| 外层 ZIP | 207 个目录项；入口 `packaged/` | 外层没有可直接视为完整工作树的顶层源码仓库 |
| `packaged/xelab/` | 28 个模块，每项有 `readme.md` + `code.zip` | 历史 XeLab 的独立/互补模块 |
| `packaged/xenonlab-old/` | 38 个模块，每项有 `readme.md` + `code.zip` | 另一个历史代码族，不能合并成同一当前 HEAD |
| 66 个内层 ZIP | 合计 1,110 个目录项、13,339,846 字节解压条目大小 | 可组合模块与独立 Web 项目混合，需要按模块规则集成 |
| Git 历史 | 外层和66个内层目录未出现 `.git/` | 不能验证 commit 对象、分支拓扑或当前 HEAD；元数据里的SHA只是来源陈述 |
| 自带来源清单 | `packaged/metadata.json`、`metadata.csv` | 可用于定位旧文件/来源；不是受信构建证明 |
| 历史报告 | `packaged/reports/xenonlab-old-review.md` 与两份TSV | 是作者此前检查的陈述，不是本次执行结果 |

所有66个内层ZIP的实际SHA-256均与自带metadata声明一致。这只证明“此次归档字节与此次归档的清单一致”，不能独立证明来自特定远端Git提交、未包含漏洞或能在PsiOS v1运行。对中央目录检查未观察到绝对路径、`..`路径分量或symlink条目；这也不是完整解压器/源码安全审计。

## 3. 已观察模块逐项清单

| 代码族 | 模块目录 | 内层条目数 | 名称/用途线索（来自原元数据） |
|---|---|---:|---|
| xelab | `application-sqlite-schema` | 3 | 应用 SQLite 约束 |
| xelab | `artifact-preflight-candidate` | 3 | 制品预检候选 |
| xelab | `contract-tooling` | 6 | 协议与数据契约校验工具 |
| xelab | `cpp-plugin-sdk` | 10 | C++ 插件 SDK |
| xelab | `event-consumer-candidate` | 5 | 事件消费者候选 |
| xelab | `export-workspace` | 3 | Rust 工作区清单与许可 |
| xelab | `host-diagnostics` | 7 | 宿主诊断与溯源工具 |
| xelab | `object-storage-preset` | 53 | 对象存储预设候选 |
| xelab | `postgres-service` | 243 | PostgreSQL 事务与平台服务适配 |
| xelab | `protocols` | 267 | OpenAPI 与 Protobuf 契约 |
| xelab | `psios-broker-core` | 2 | psios-broker-core |
| xelab | `psios-common` | 2 | psios-common |
| xelab | `psios-event-core` | 2 | psios-event-core |
| xelab | `psios-host-io` | 2 | psios-host-io |
| xelab | `psios-plugin-contracts` | 6 | psios-plugin-contracts |
| xelab | `psios-runtime` | 8 | psios-runtime |
| xelab | `psios-scheduler` | 5 | psios-scheduler |
| xelab | `psios-security` | 7 | psios-security |
| xelab | `psios-storage` | 12 | psios-storage |
| xelab | `psios-task-engine` | 2 | psios-task-engine |
| xelab | `psios-workflow` | 5 | psios-workflow |
| xelab | `rust-plugin-sdk` | 8 | Rust 插件 SDK |
| xelab | `rust-readonly-client-candidate` | 6 | 只读 HTTPS 客户端候选 |
| xelab | `sdk-conformance-harness` | 2 | Rust/C++ SDK 互操作测试 |
| xelab | `shell` | 22 | Shell 浏览器组件与静态服务 |
| xelab | `signed-artifact-candidate` | 10 | 离线签名制品候选 |
| xelab | `test-fixtures` | 59 | 互操作/契约验证夹具 |
| xelab | `web-search-application` | 6 | Web 搜索应用原语 |
| xenonlab-old | `account-authority` | 6 | 账户继承、OIDC 与会话 |
| xenonlab-old | `archive-snapshot` | 4 | 文件归档快照 |
| xenonlab-old | `capability-registry` | 21 | 能力注册、描述快照与生命周期 |
| xenonlab-old | `capability-state` | 3 | 能力状态快照与子能力权限判定 |
| xenonlab-old | `capability-versions` | 3 | 能力版本协商 |
| xenonlab-old | `data-primitives` | 2 | SQLite 事务与 API 错误原语 |
| xenonlab-old | `deploy-admission` | 5 | 审批与部署租约适配器 |
| xenonlab-old | `desktop-api` | 4 | 桌面状态与插件投影 API |
| xenonlab-old | `desktop-snapshot` | 8 | 桌面账户与资源备份 |
| xenonlab-old | `embedding-workers` | 18 | 嵌入提供者、预算与 CPU worker |
| xenonlab-old | `lean-verification` | 7 | Lean 证明校验服务与定理元数据 |
| xenonlab-old | `library-doctor` | 3 | .xl 目录与对象一致性检查 |
| xenonlab-old | `metrics-registry` | 3 | Prometheus 指标注册器 |
| xenonlab-old | `miniv4-model-source` | 17 | MiniV4 模型与 tokenizer 参考源码 |
| xenonlab-old | `model-cache` | 3 | 模型快照缓存与水位管理 |
| xenonlab-old | `oeis-import` | 4 | OEIS 数据包构建与导入 |
| xenonlab-old | `openapi-contracts` | 12 | 早期服务 OpenAPI 契约 |
| xenonlab-old | `ops-reporting` | 6 | 备份与宿主健康报告 |
| xenonlab-old | `platform-assembly` | 2 | 服务装配与跨模块测试入口 |
| xenonlab-old | `platform-core` | 6 | FastAPI 请求基础与授权适配 |
| xenonlab-old | `plugin-trust-policy` | 22 | 服务插件声明、签名、溯源与执行策略 |
| xenonlab-old | `pool-telemetry` | 3 | 资源池签名遥测 |
| xenonlab-old | `provider-gateway` | 7 | Chat Provider 与受控工具路由 |
| xenonlab-old | `python-integration-kit` | 6 | Python 依赖锁与共享夹具 |
| xenonlab-old | `query-dsl` | 2 | 查询 DSL 与 AST 解释器 |
| xenonlab-old | `retrieval` | 16 | 检索、分块与质量重排 |
| xenonlab-old | `service-db-backup` | 4 | PostgreSQL 与 Forgejo 备份适配器 |
| xenonlab-old | `sqlite-job-store` | 4 | SQLite 批处理和运行状态账本 |
| xenonlab-old | `sqlite-text-index` | 2 | SQLite FTS5 中日韩文本索引 |
| xenonlab-old | `vector-scan-prototype` | 5 | Rust 精确向量扫描原型 |
| xenonlab-old | `web-compact-tree` | 3 | 紧凑屏树形侧栏布局 |
| xenonlab-old | `web-frame-boundary` | 3 | iframe 消息与沙箱边界 |
| xenonlab-old | `web-library-workbench` | 32 | 知识库工作台数据处理与 React hooks |
| xenonlab-old | `web-session-policy` | 20 | 浏览器会话、请求代次与 OIDC 适配 |
| xenonlab-old | `web-shell-models` | 46 | 桌面交互与运维视图状态 |
| xenonlab-old | `work-queue` | 4 | 工作租约 API 与规则分诊 |
| xenonlab-old | `xl-library` | 22 | .xl 文档数据库服务适配层 |
| xenonlab-old | `xl-snapshot` | 6 | XeLibrary 快照与恢复 |

这里的用途线索是归档作者的命名和说明，不是本次对每个函数能力的确认。`psios-security`、`psios-runtime`、`psios-storage`、`psios-task-engine`等名称不意味着它们已经满足本包的安全、持久性、撤权或恢复契约。

## 4. 构建、依赖与许可证线索

`packaged/xelab/export-workspace/code.zip`实际只有`Cargo.lock`、`Cargo.toml`、`LICENSE`三个文件。工作区清单使用`crates/*`和`adapters/postgres-service`成员模式，版本为0.1.0、edition为2021，许可证字段MIT。相应LICENSE包含MIT文本；这不是PsiOS v1的软件版本或全包统一许可证。

其他模块的Cargo清单有互相引用的本地路径，例如runtime依赖common/host-io/plugin-contracts；只取一个代码ZIP未必能构建。Shell包含`web/package.json`；多个XenonLab-Old Web模块各有根`package.json`，不能随意在同一目录覆盖叠加。清单被读取但没有运行其中任何命令。

XenonLab-Old元数据/历史报告写明当时未发现项目级许可证，embedding供应商目录另带许可。对此这里只记录归档原有权利信息；复用与分发前需要按实际当前仓库、文件来源和依赖核验，不能把另一代码族的MIT文本套到全部文件。

## 5. 来源陈述不能证明当前HEAD

| 归档声明的来源 | 完整提交SHA | 本次状态 |
|---|---|---|
| `https://github.com/jyxxe/XeLab.git` | `5924c18bcdf08cb125c4f1d2172b9d8feae451dc` | HistoricalStatementOnly |
| `https://github.com/jyxxe/XeLab.git` | `ba79428354e2a518472e989acca85d7f70e1b535` | HistoricalStatementOnly |
| `https://github.com/jyxxe/XeLab.git` | `e28d70eeef63472d2a1dd4faaf153df8ad9274ca` | HistoricalStatementOnly |
| `https://github.com/jyxxe/XenonLab_Old` | `0f50d396052592624a8a8a4c1e36a9eb29f5bf6a` | HistoricalStatementOnly |
| `https://github.com/jyxxenon/XeLab` | `bf9f1a1e457ce8922a588e810e04d06ef5bb79ae` | HistoricalStatementOnly |
| `https://github.com/jyxxenon/XeLab` | `e499e6a964af99fdfd14022d1e419ddf34887cab` | HistoricalStatementOnly |
| `ssh://git@127.0.0.1:2222/xelab-admin/XeLab.git` | `b444a8ad56f8b3bbaf32279fdcc727658a7ec120` | HistoricalStatementOnly |

其中`127.0.0.1`是原导出环境的回环地址，不是可移植的项目远端配置；后续必须取得用户实际指定的Forgejo实例身份和仓库。不同来源/不同提交的文件可能被组合成一个导出模块，不能把任一SHA提升为全归档的唯一当前HEAD。

## 6. 历史测试与缺失链接

现有`xenonlab-old-review.md`报告了历史`1638 passed`、前端158项、不同组合包539/789等结果，并列出环境和跳过项。各README也使用“上一轮独立Rust工作区测试”等描述。这些数字属于**来源作者的历史报告**；本次没有复跑，不把它们抄成PsiOS v1的Passed、性能基线或安全验收。

本次发现 **29 处相对链接未命中外层归档条目**，主要指向不存在于交付ZIP中的`reports/verification.md`。完整源/目标清单在静态JSON；缺失报告意味着相应README所述证据无法仅靠这份ZIP完整复核。没有补造缺失报告，也没有把现存另一份报告当成同一证据。

## 7. 进入实际复用前的具体门槛

后续逐模块取得当前完整仓库SHA和文件来源后，才能给出reuse/adapt/rewrite/exclude决定。每项需比较v1契约、旧ABI/API/数据库结构、授权默认值、宿主权限、外部地址、供应链/许可证、迁移和故障测试。优先核对Task外部效果/去重、事件outbox/inbox、安全审批消费/撤权、存储持久与恢复fencing这些跨域边界。

只读解析器、类型/纯函数、受控工具、后端驱动和宿主特权组件的复用风险不同，应分别给出证据。即便历史单元测试通过，也不能推出打开FD撤权、断电持久性、跨节点分区、KDE Portal版本或不可信包脚本隔离已经满足PsiOS要求。

本次已经完成的是**归档静态清点、来源线索保留及摘要一致性核对**；没有完成当前源代码安全审计、构建验证或产品实现验收。
