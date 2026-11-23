# 来源、核验与引用边界

原始项目输入完整保存在archive；来源清单记录原上传名、ID、字节数和SHA-256。外部资料只用一手规范或官方文档支持紧邻机制，不为PsiOS提供实现或认证证明。核验日：2026-11-22。

带latest/main/current的网址可能变化；实际依赖冻结需另记录版本/tag/commit。历史归档与当前规范发生冲突时，先按00章查迁移，不从旧源恢复宽松行为。

| 编号 | 官方资料 | 支持的范围 |
|---|---|---|
| SEC-S01 | [RFC 9700 — Best Current Practice for OAuth 2.0 Security](https://www.rfc-editor.org/rfc/rfc9700.html) | OAuth授权中的受众限制、code/PKCE及refresh token保护机制；不证明PsiOS已采用或实现 |
| SEC-S02 | [RFC 8785 — JSON Canonicalization Scheme](https://www.rfc-editor.org/rfc/rfc8785.html) | 确定性JSON规范化、拒绝重复键、数值/Unicode/数组顺序约束；v1选定JCS摘要语义，具体实现库仍待ADR验证；JCS数值/Unicode/排序及确定摘要语义；不等于完整PsiOS安全验证 |
| SEC-S03 | [OWASP Session Management Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html) | Web会话Cookie、登录/权限变化时ID轮换、服务端过期和注销机制 |
| SEC-S04 | [OWASP Cross-Site Request Forgery Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html) | CSRF防护、Origin/SameSite边界及安全GET；具体PsiOS策略由本包提出 |
| SEC-S05 | [OWASP Cross Site Scripting Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross_Site_Scripting_Prevention_Cheat_Sheet.html) | 上下文输出编码、HTML消毒以及CSP仅作为纵深防御 |
| SRC-STA-01 | [Linux kernel 6.1 FUSE documentation](https://docs.kernel.org/6.1/filesystems/fuse.html) | default_permissions、allow_other与FUSE连接/挂载边界；不证明PsiOS已实现授权。 |
| SRC-STA-02 | [Linux kernel 5.15 Fuse I/O modes](https://docs.kernel.org/5.15/filesystems/fuse-io.html) | writeback缓存及所有修改经FUSE内核路径的假设；当前内核特性需绑定实际版本复测。 |
| SRC-STA-03 | [PostgreSQL file system level backup](https://www.postgresql.org/docs/current/backup-file.html) | 数据库备份需要按实际停止/快照与WAL范围协议协调；不代表选用该数据库。 |
| SRC-STA-04 | [RFC 8259 JSON](https://www.rfc-editor.org/rfc/rfc8259) | JSON值类型、数字与互操作基础；PsiOS额外拒重复键和精确比较策略为本设计定义。 |
| SRC-STA-05 | [OCI Runtime Specification configuration](https://github.com/opencontainers/runtime-spec/blob/main/config.md) | OCI配置、root、mounts及process等机制；具体权限效果需实际Runtime验证。 |
| SRC-STA-06 | [OCI Runtime Specification Linux configuration](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md) | namespace/cgroup等Linux隔离配置机制；不保证任意威胁模型沙箱。 |
| SRC-STA-07 | [Flatpak Sandbox Permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html) | 静态权限与Portal授权路径不同；音频Socket可能开放多类音频能力。 |
| SRC-STA-08 | [XDG Desktop Portal ScreenCast interface](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html) | 恢复令牌单次使用；pipewire-serial为接口v6新增；组合RemoteDesktop持久化不得由ScreenCast单独管理。 |
| SRC-STA-09 | [XDG Desktop Portal RemoteDesktop interface](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) | 组合会话生命周期与持久化由RemoteDesktop管理；EIS建立后不再使用Notify输入。 |
| SRC-ORC-001 | [MCP 2026-07-28 Versioning and Negotiation](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning) | 现代 MCP 采用逐请求版本上下文；服务器必须实现 server/discover，客户端可以先发现或直接请求；与 legacy 初始化时代区分。 |
| SRC-ORC-002 | [MCP 2025-11-25 Lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) | 历史 MCP 通过 initialize/initialized 生命周期协商版本与能力；不能把旧会话假设直接套用现代请求。 |
| SRC-ORC-003 | [MCP 2026-07-28 Transports](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports) | 现代 HTTP 元信息与正文映射须一致，传输流/请求中止语义不构成远端业务效果已回滚的证明。 |
| SRC-ORC-004 | [MCP 2026-07-28 Authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization) | HTTP OAuth 与 STDIO 凭据路径区分；资源/令牌受众、发行方与授权响应 issuer 验证按规范和元数据执行；不得把可选条件写成所有响应强制 iss。 |
| SRC-ORC-005 | [MCP Tasks Extension 2026-07-28](https://tasks.extensions.modelcontextprotocol.io/specification/2026-07-28/tasks) | 现代任务方法包含 tasks/get、tasks/update、tasks/cancel；completed 可以包含 isError:true 的工具结果；访问任务继续授权，TTL 到期可能清理记录。 |
| SRC-ORC-006 | [Agent2Agent Protocol Specification](https://a2a-protocol.org/latest/specification/) | A2A 覆盖 Agent 发现、消息与任务互操作；Send Message 的 messageId 幂等属于 MAY，不能由协议名推断发送端到端 exactly-once。 |
| SRC-ORC-007 | [Agent Client Protocol v1 Overview](https://agentclientprotocol.com/protocol/v1/overview) | ACP-Client 是客户端与 Agent 的会话交互协议；核验时官方文档区分 v1 Latest 与 v2 Preview，实际采用仍需锁定版本。 |
| SRC-ORC-008 | [Agent Communication Protocol Welcome and A2A Migration Notice](https://agentcommunicationprotocol.dev/introduction/welcome) | Agent Communication Protocol 与 Agent Client Protocol 是不同协议；前者官方说明已并入 Linux Foundation 的 A2A。 |
| SRC-ORC-009 | [AG-UI Introduction](https://docs.ag-ui.com/introduction) | AG-UI 负责 Agent 与用户前端之间的交互事件；PsiOS 将其定位为 UI 事件适配，审批和效果权威仍由自己的安全/Operation 契约定义。 |
| SRC-ORC-010 | [OpenAPI Specification v3.2.1](https://spec.openapis.org/oas/v3.2.1.html) | OpenAPI 是 HTTP API 的接口描述规范，可用于工具封装；描述文件本身不等于实现已验证、安全授权或幂等保证。 |
| SRC-CORE-002 | [etcd v3.6 Disaster recovery](https://etcd.io/docs/v3.6/op-guide/recovery/) | 旧快照revision回退和watch缓存恢复；不能代替资源端fencing |
| SRC-CORE-003 | [bootc filesystem](https://github.com/bootc-dev/bootc/blob/main/docs/src/bootc-filesystem.7.md) | /etc与/var持久与更新语义不同；具体后端尚待选择 |
| SRC-CORE-004 | [bootc rollback](https://bootc.dev/bootc/man/bootc-rollback.8.html) | rollback启动部署与/etc处理、自动更新可能再次推进；需绑定目标版本核验 |
| SRC-CORE-005 | [Linux man-pages 6.11](https://www.kernel.org/pub/linux/docs/man-pages/book/man-pages-6.11.pdf) | CLOCK_MONOTONIC与CLOCK_BOOTTIME对挂起计时的差别 |
| SRC-CORE-006 | [Linux cgroup v2](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html) | memory.high压力节流不等同memory.max硬限制 |
| SRC-CORE-007 | [The Update Framework specification](https://theupdateframework.github.io/specification/latest/index.html) | 更新信任和委托机制参考；不表示已经采用 |
| SRC-CORE-008 | [SLSA v1.2 Provenance](https://slsa.dev/spec/v1.2/provenance) | 构建来源与制品证据模型；不声称达到认证等级 |

## 原始输入与代码边界

[原始输入清单](source-inventory.json) · [历史源说明](archive/README.md) · [XeLab静态清点](../migration/xelab-reuse-boundary.md)。

此包未获取当前Forgejo HEAD，未安装或执行归档代码。保存源文件是为了可追溯，不把历史文件的计划、测试数字或脚本自动提升为当前权威。
