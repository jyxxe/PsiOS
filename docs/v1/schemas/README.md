# JSON Schema 合同

采用JSON Schema Draft 2020-12。`catalog.json`列出全部合同；`common.schema.json`定义共同类型。`https://schemas.psios.example/v1/`是保留的示例命名空间，只用于离线引用解析，**不是已上线服务**。校验工具把本目录文件注册为本地资源，不访问该地址。

## 如何使用

从包根运行`python tools/validate_package.py`，可检查Schema语法及全部正反示例。将某个合同用于SDK/服务之前，还须冻结ADR-05传输绑定、实现章节中的语义验证、通过实际合同测试；不能把这个文档工具接到生产网络当授权服务。

| 分类 | 文件 | 关键约束 |
|---|---|---|
| 基础 | common、resource-envelope、error | 不可变引用、opaque版本、无损计数、统一错误；通用spec不等于可写API |
| 效果与审批 | operation-plan、operation、effect-record、approval-request、approval-decision、approval-use、execution-permit | 计划摘要、单元CAS、效果知识、受众与代次；普通JSON不自证可信 |
| 执行 | task-submit、task-accepted、task-lookup、task、run、cancel-origin | 请求/回执分离、四分查询、Task与Run分工、先到取消保护 |
| 交流 | chat-send、chat-accepted、message | 发送ID、恢复世代、持久序号、初始摘要与修改后内容版本分开 |
| 编排 | workflow-definition、workflow-run、dispatch-intent、event | 有界AST、明确步骤依赖、可靠派发、事件不是授权 |
| 数据 | dataset-record、volume、volume-attachment | 七个业务字段、可选任意JSON、卷身份/挂载/访问分开 |
| 能力与恢复 | capability-descriptor、revocation-result、recovery-status、recovery-set-manifest | 逐资源强制、信任与效果屏障、恢复集一致性及真实故障范围 |
| 应用与列表 | application-manifest、application-release-spec、snapshot-page | 元数据固定引用完整执行/权限/资源/来源规范；固定分页快照与watch起点 |

上表文件均加`.schema.json`后缀。带`Ref`字段必须解析到已登记类型，不允许以任意JSON blob替换授权、预算或允许效果边界。`ResourceRef`的digest与引用资源实际版本应匹配；夹具中的资源未提供真实系统对象，示例通过不表示引用可解析。

## 摘要Profile

| 合同 | 排除自身字段 | 域前缀（后接LF） |
|---|---|---|
| task-submit | request_digest | psios/task-submit/1 |
| chat-send | content_digest | psios/chat-send/1 |
| operation-plan | plan_digest | psios/operation-plan/1 |
| workflow-definition | definition_digest | psios/workflow-definition/1 |
| recovery-set-manifest | manifest_digest | psios/recovery-set-manifest/1 |

规范为UTF-8域前缀+LF+RFC8785 JCS(payload)，再SHA-256。`tools/build_contracts.py`中的摘要函数**仅适用于本包ASCII键、字符串、布尔、null和安全整数的夹具子集**，会拒绝浮点等更广输入，不能用作完整JCS库。生产实现必须验证完整RFC8785数值/UTF-16键排序/Unicode错误向量。原始内容制品按字节摘要；Dataset任意精度JSON不强制先转double。

## 仍需运行时验证

所有权与身份、请求受众、签名/密钥、字段级权限、引用存在及保留、条件写原子性、ApprovalUse跨记录唯一性、幂等窗口新鲜度、Workflow输入类型与引用子图有界性、真实资源fencing、外部结果、Profile资源预算、数字精度和期限。结构校验不能证明这些性质。

本目录的Schema为v1设计合同，支持向后兼容的展示扩展必须明确命名空间与限额；安全请求默认拒绝未知字段。需要新增字段时先变更合同及版本、负例与迁移，再生成SDK，不靠宽松解析掩盖版本冲突。
