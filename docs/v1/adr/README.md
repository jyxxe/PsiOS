# 架构决策记录（ADR）

本目录保留原ADR-01～25的身份。文件名ADR-001.md～ADR-025.md中的额外补零只用于排序，不产生新编号。**25项均为Proposed；工程NotVerified、验收NotRun。** 本包已经明确的安全与行为约束是候选方案必须满足的边界，不能据此声称某一数据库、运行时、协议版本组合或平台已通过选型。

## 如何阅读与批准

1. 先阅读共同契约和该ADR所链接模块，再查看候选、实验、否决与降级条件。
2. 指派实际负责人及独立复核角色，固定实验仓库SHA、依赖和制品。
3. 执行实验并保存失败证据；将选择理由、被排除方案、支持Profile、预算和剩余限制写入记录。
4. 经项目规定的决策流程批准后，才将Proposed改为Accepted；批准实现选型不自动把Implementation改为Passed。
5. 决策替换或拒绝分别登记Superseded/Rejected并保留历史链接。涉及共同契约改变时先处理规范冲突，不能绕过主规范。

本目录没有真实批准签名、负责人任命或本轮工程验收。表中的候选可行性须由所列实验确认，文档结构检查不能替代技术验证。

## 索引

| 规范ID | 主题 | 原范围与v1关系 | 状态 |
|---|---|---|---|
| [ADR-01](ADR-001.md) | OS 基底、构建工具与平台组合 | 保持原编号与范围 | Proposed |
| [ADR-02](ADR-002.md) | 启动器、更新后端与持久状态布局 | 保持原编号与范围 | Proposed |
| [ADR-03](ADR-003.md) | 默认文件系统、卷与备份后端 | 保持原编号与范围 | Proposed |
| [ADR-04](ADR-004.md) | 身份、授权、Secret 与安全启动 Profile | 保持原编号与范围 | Proposed |
| [ADR-05](ADR-005.md) | 系统 API、资源 Schema 与元数据实现 | 保持原编号与范围 | Proposed |
| [ADR-06](ADR-006.md) | OCI 执行后端与隔离等级 | 保持原编号与范围 | Proposed |
| [ADR-07](ADR-007.md) | 网络配置与策略执行后端 | 保持原编号与范围 | Proposed |
| [ADR-08](ADR-008.md) | 设备/GPU 和电源能力矩阵 | 保持原编号与范围 | Proposed |
| [ADR-09](ADR-009.md) | Agent 协议与第三方兼容配置 | 保持原编号与范围 | Proposed |
| [ADR-10](ADR-010.md) | KDE、Portal 与无头图形后端 | 保持原编号与范围 | Proposed |
| [ADR-11](ADR-011.md) | 任务/事件持久化与执行语义 | 保持原编号与范围 | Proposed |
| [ADR-12](ADR-012.md) | 集群控制面、准入、租约与恢复代次 | 保持原编号与范围 | Proposed |
| [ADR-13](ADR-013.md) | Dataset/Knowledge 引擎和查询实现 | 范围明确/拆分见正文 | Proposed |
| [ADR-14](ADR-014.md) | 观测与审计后端 | 保持原编号与范围 | Proposed |
| [ADR-15](ADR-015.md) | 发布信任、兼容窗口与维护政策 | 保持原编号与范围 | Proposed |
| [ADR-16](ADR-016.md) | 发行版资源和体验基线 | 保持原编号与范围 | Proposed |
| [ADR-17](ADR-017.md) | 第三方组件准入与依赖治理 | 保持原编号与范围 | Proposed |
| [ADR-18](ADR-018.md) | TableFS 文件/表双视图存储后端 | 保持原编号与范围 | Proposed |
| [ADR-19](ADR-019.md) | Universal Package Adapter 与发行版兼容 Profile | 保持原编号与范围 | Proposed |
| [ADR-20](ADR-020.md) | GUI Bridge：窗口、Viewer 与传输后端 | 范围明确/拆分见正文 | Proposed |
| [ADR-21](ADR-021.md) | Agent Chat 的 Room/DM、投递和唤醒后端 | 范围明确/拆分见正文 | Proposed |
| [ADR-22](ADR-022.md) | PsiFlow 编排、WorkflowRun 与版本化 DSL | 范围明确/拆分见正文 | Proposed |
| [ADR-23](ADR-023.md) | 项目管理及外部系统同步 | 范围明确/拆分见正文 | Proposed |
| [ADR-24](ADR-024.md) | 跨模块操作/审批与 Saga | 范围明确/拆分见正文 | Proposed |
| [ADR-25](ADR-025.md) | Workflow/Chat 持久存储、容量、保留与回收 | 范围明确/拆分见正文 | Proposed |

## 容易混淆的职责

- ADR-05拥有共同API/Schema与条件写实现；ADR-11拥有Task/Event事实；ADR-25裁决共享容量/保留/恢复基础实现，不创造第二权威。
- ADR-10裁决桌面会话、compositor/Portal和输入后端；ADR-20裁决浏览器Viewer、窗口呈现与媒体/输入传输。
- ADR-21始终是Agent Chat；ADR-22始终是PsiFlow。原追加表按22、21的行顺序列出，不表示交换编号。
- 原ADR-22的宽范围已由v0.6追加ADR-23/24/25分工：Project/外部字段同步归23，审批/Saga归24，物理存储/容量归25；本目录逐项保留沿革说明。
- ADR-13中v1已经明确的逻辑字段不再作为任意后端的自由默认；物理编码与查询执行仍需比较验证。

表中“依赖”表示本决策所需的前置契约或基础选择，构成无环关系；它不要求在所有关联团队完成实现后才开始实验。ADR-02/04仍需与ADR-15共同验证发布信任，ADR-06/08仍需共同验证设备隔离；这些往返验证属于协同关系，不登记为互相等待的前置依赖。

机器可读登记见 [adrs.json](../registries/adrs.json)。

