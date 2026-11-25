# v1 规范索引与验证计划

本索引从当前17份分章生成。每个ID只有一个定义，表中的测试是待运行验证计划；存在映射不表示已经通过。完整要求以目标分章为准。

| 要求ID | 定义入口 | 待运行测试 |
|---|---|---|
| V1-GOV-001 | [V1-GOV-001｜同一发布包只有一套规范](../docs/00-reading-and-authority.md) | AT-COMP-001 |
| V1-GOV-002 | [V1-GOV-002｜证据不得跨层推断](../docs/00-reading-and-authority.md) | AT-COMP-002 |
| V1-GOV-003 | [V1-GOV-003｜实现和功能开关分开](../docs/00-reading-and-authority.md) | AT-COMP-003 |
| V1-GOV-004 | [V1-GOV-004｜旧编号不可无来源迁移](../docs/00-reading-and-authority.md) | AT-COMP-001 |
| V1-GOV-005 | [V1-GOV-005｜变更流程](../docs/00-reading-and-authority.md) | AT-COMP-001 |
| V1-BASE-001 | [V1-BASE-001｜三版同源](../docs/01-product-and-architecture.md) | AT-WFL-010 |
| V1-BASE-002 | [V1-BASE-002｜容器化边界](../docs/01-product-and-architecture.md) | AT-COMP-007 |
| V1-BASE-003 | [V1-BASE-003｜数据边界](../docs/01-product-and-architecture.md) | AT-COMP-014 |
| V1-BASE-004 | [V1-BASE-004｜协作边界](../docs/01-product-and-architecture.md) | AT-COMP-009 |
| V1-ARCH-001 | [V1-ARCH-001｜启动依赖为有向无环图](../docs/01-product-and-architecture.md) | AT-CORE-012 |
| V1-ARCH-002 | [V1-ARCH-002｜一个字段一个写入权威](../docs/01-product-and-architecture.md) | AT-COMP-009 |
| V1-ARCH-003 | [V1-ARCH-003｜Core 不是所有服务常驻的集合](../docs/01-product-and-architecture.md) | AT-CORE-012 |
| V1-ARCH-004 | [V1-ARCH-004｜服务失效隔离](../docs/01-product-and-architecture.md) | AT-WFL-010 |
| V1-ARCH-005 | [V1-ARCH-005｜复用需逐模块验收](../docs/01-product-and-architecture.md) | AT-COMP-004 |
| V1-API-001 | [V1-API-001｜身份、位置和权限分离](../docs/02-common-contracts.md) | AT-COMP-009 |
| V1-API-002 | [V1-API-002｜引用必须固定可验证版本](../docs/02-common-contracts.md) | AT-STA-009 |
| V1-API-003 | [V1-API-003｜条件写由资源权威原子检查](../docs/02-common-contracts.md) | AT-STA-006, AT-CORE-008 |
| V1-API-004 | [V1-API-004｜依赖条件须可强制](../docs/02-common-contracts.md) | AT-WFL-008, AT-CORE-013 |
| V1-API-005 | [V1-API-005｜声明式控制权显式移交](../docs/02-common-contracts.md) | AT-CORE-017 |
| V1-API-006 | [V1-API-006｜跨语言编码固定](../docs/02-common-contracts.md) | SEC-V1-15, AT-CORE-019 |
| V1-API-007 | [V1-API-007｜规范化摘要有域隔离](../docs/02-common-contracts.md) | SEC-V1-15, AT-CORE-019 |
| V1-API-008 | [V1-API-008｜版本彼此独立](../docs/02-common-contracts.md) | AT-CORE-019 |
| V1-API-009 | [V1-API-009｜接受、执行、结果和补偿独立](../docs/02-common-contracts.md) | AT-TSK-006, AT-WFL-006, AT-CORE-020 |
| V1-API-010 | [V1-API-010｜幂等命名空间包含安全域](../docs/02-common-contracts.md) | AT-CORE-001 |
| V1-API-011 | [V1-API-011｜保留期结束不可证明未执行](../docs/02-common-contracts.md) | REC-V1-06, AT-TSK-003 |
| V1-API-012 | [V1-API-012｜配额准入与必要状态一起提交](../docs/02-common-contracts.md) | AT-TSK-001, AT-CORE-007 |
| V1-API-013 | [V1-API-013｜取消是独立可查询操作](../docs/02-common-contracts.md) | AT-TSK-004 |
| V1-API-014 | [V1-API-014｜快照到事件无缺口](../docs/02-common-contracts.md) | AT-STA-007, AT-STA-010, AT-CHAT-005, AT-CORE-006 |
| V1-API-015 | [V1-API-015｜通知不是授权凭证](../docs/02-common-contracts.md) | AT-CORE-006 |
| V1-API-016 | [V1-API-016｜结构有效不等于请求可执行](../docs/02-common-contracts.md) | AT-CORE-019 |
| V1-API-017 | [V1-API-017｜路径不是权限](../docs/02-common-contracts.md) | SEC-V1-12, AT-AGT-007 |
| V1-API-018 | [V1-API-018｜边界有明确上限](../docs/02-common-contracts.md) | AT-CORE-007 |
| V1-FND-001 | [V1-FND-001｜可重建的宿主输入](../docs/03-host-updates-network-devices.md) | AT-COMP-005 |
| V1-FND-002 | [V1-FND-002｜三个就绪状态](../docs/03-host-updates-network-devices.md) | AT-CORE-012 |
| V1-FND-003 | [V1-FND-003｜逻辑分域不假定物理分盘](../docs/03-host-updates-network-devices.md) | AT-COMP-006 |
| V1-FND-004 | [V1-FND-004｜磁盘操作按稳定身份核对](../docs/03-host-updates-network-devices.md) | AT-STA-001, AT-CORE-013 |
| V1-FND-005 | [V1-FND-005｜救援独立且受控](../docs/03-host-updates-network-devices.md) | SEC-V1-14, AT-CORE-012 |
| V1-UPD-001 | [V1-UPD-001｜更新状态持久可恢复](../docs/03-host-updates-network-devices.md) | AT-CORE-009 |
| V1-UPD-002 | [V1-UPD-002｜回退与重试闭环](../docs/03-host-updates-network-devices.md) | AT-CORE-009 |
| V1-UPD-003 | [V1-UPD-003｜三类迁移分开](../docs/03-host-updates-network-devices.md) | REC-V1-12, AT-STA-015, AT-CORE-009 |
| V1-UPD-004 | [V1-UPD-004｜更新信任独立](../docs/03-host-updates-network-devices.md) | REC-V1-12, AT-CORE-009 |
| V1-UPD-005 | [V1-UPD-005｜HostExtension受完整更新治理](../docs/03-host-updates-network-devices.md) | AT-COMP-005 |
| V1-FND-006 | [V1-FND-006｜计时语义按用途选择](../docs/03-host-updates-network-devices.md) | REC-V1-13, AT-CORE-014 |
| V1-FND-007 | [V1-FND-007｜启动链与解密Profile](../docs/03-host-updates-network-devices.md) | AT-COMP-005 |
| V1-NET-001 | [V1-NET-001｜一个接口一个配置权威](../docs/03-host-updates-network-devices.md) | AT-CORE-008 |
| V1-NET-002 | [V1-NET-002｜管理连接变更有确认与回退](../docs/03-host-updates-network-devices.md) | AT-CORE-008 |
| V1-NET-003 | [V1-NET-003｜实际出站路径受控](../docs/03-host-updates-network-devices.md) | AT-STA-017, AT-STA-024, AT-CORE-015 |
| V1-NET-004 | [V1-NET-004｜断联不扩权](../docs/03-host-updates-network-devices.md) | AT-COMP-017 |
| V1-DEV-001 | [V1-DEV-001｜设备身份不靠路径](../docs/03-host-updates-network-devices.md) | AT-CORE-013 |
| V1-DEV-002 | [V1-DEV-002｜承诺与强制能力分开](../docs/03-host-updates-network-devices.md) | AT-STA-026, AT-CORE-016 |
| V1-DEV-003 | [V1-DEV-003｜公平性和本机所有权](../docs/03-host-updates-network-devices.md) | AT-CORE-016 |
| V1-DEV-004 | [V1-DEV-004｜电源纳入执行协调](../docs/03-host-updates-network-devices.md) | AT-CORE-002 |
| V1-UX-001 | [V1-UX-001｜无需模型也能完成初始配置](../docs/03-host-updates-network-devices.md) | AT-COMP-011 |
| V1-UX-002 | [V1-UX-002｜会话与可访问性](../docs/03-host-updates-network-devices.md) | AT-COMP-011 |
| V1-SEC-001 | [V1-SEC-001　主体、系统和数据边界](../docs/04-security-identity.md) | SEC-V1-01, REC-V1-08 |
| V1-SEC-002 | [V1-SEC-002　安全对象字典](../docs/04-security-identity.md) | AT-COMP-009 |
| V1-SEC-003 | [V1-SEC-003　认证建立身份，资源策略决定权限](../docs/04-security-identity.md) | SEC-V1-01, SEC-V1-02, SEC-V1-12 |
| V1-SEC-004 | [V1-SEC-004　权限交集与委托链](../docs/04-security-identity.md) | SEC-V1-02 |
| V1-SEC-005 | [V1-SEC-005　不可变计划的可审阅内容](../docs/04-security-identity.md) | SEC-V1-06 |
| V1-SEC-006 | [V1-SEC-006　摘要与解释必须一致](../docs/04-security-identity.md) | SEC-V1-06, SEC-V1-15, AT-CORE-019 |
| V1-SEC-007 | [V1-SEC-007　审批状态与单次消费](../docs/04-security-identity.md) | SEC-V1-03, SEC-V1-04, REC-V1-04, AT-CORE-004 |
| V1-SEC-008 | [V1-SEC-008　ExecutionPermit 的边界和审计](../docs/04-security-identity.md) | SEC-V1-03, AT-CORE-004, AT-CORE-014 |
| V1-SEC-009 | [V1-SEC-009　审批、撤销与资源变更的竞争](../docs/04-security-identity.md) | SEC-V1-05, SEC-V1-06, AT-WFL-004, AT-CORE-004 |
| V1-SEC-010 | [V1-SEC-010　宿主操作必须类型化](../docs/04-security-identity.md) | AT-COMP-007 |
| V1-SEC-011 | [V1-SEC-011　文件与本地 IPC 的实际对象验证](../docs/04-security-identity.md) | SEC-V1-07, AT-STA-017 |
| V1-SEC-012 | [V1-SEC-012　数据外发与有权代理误用](../docs/04-security-identity.md) | SEC-V1-08, AT-AGT-003, AT-CORE-015 |
| V1-SEC-013 | [V1-SEC-013　Secret 生命周期及恢复](../docs/04-security-identity.md) | AT-COMP-010 |
| V1-SEC-014 | [V1-SEC-014　Shell、CLI 与 GUI 的共同会话边界](../docs/04-security-identity.md) | SEC-V1-09, SEC-V1-11, SEC-V1-12 |
| V1-SEC-015 | [V1-SEC-015　跨站请求、脚本与代理](../docs/04-security-identity.md) | SEC-V1-09, SEC-V1-10, AT-STA-024 |
| V1-SEC-016 | [V1-SEC-016　新鲜认证、恢复与反滥用](../docs/04-security-identity.md) | SEC-V1-11, REC-V1-02 |
| V1-SEC-017 | [V1-SEC-017　能力集合与报告结果分离](../docs/04-security-identity.md) | SEC-V1-05, SEC-V1-13 |
| V1-SEC-018 | [V1-SEC-018　撤销工作流及准确响应](../docs/04-security-identity.md) | SEC-V1-05, SEC-V1-11, AT-STA-026 |
| V1-SEC-019 | [V1-SEC-019　撤权与文件、设备、输入的特殊边界](../docs/04-security-identity.md) | SEC-V1-13, AT-STA-020 |
| V1-SEC-020 | [V1-SEC-020　必要审计与秘密最小化](../docs/04-security-identity.md) | SEC-V1-14 |
| V1-SEC-021 | [V1-SEC-021　限额、删除与密钥独立](../docs/04-security-identity.md) | SEC-V1-14 |
| V1-SEC-022 | [V1-SEC-022　安全验收与失败注入](../docs/04-security-identity.md) | AT-COMP-002 |
| V1-REC-001 | [V1-REC-001　恢复状态不得合并成一个“成功”](../docs/05-recovery-consistency.md) | AT-COMP-015 |
| V1-REC-002 | [V1-REC-002　安全事实只有一个持久权威](../docs/05-recovery-consistency.md) | AT-COMP-006 |
| V1-REC-003 | [V1-REC-003　恢复协调器不创造第二事实来源](../docs/05-recovery-consistency.md) | AT-COMP-015 |
| V1-REC-004 | [V1-REC-004　旧备份不能证明后续撤权](../docs/05-recovery-consistency.md) | REC-V1-01, REC-V1-02 |
| V1-REC-005 | [V1-REC-005　三种模式的精确定义](../docs/05-recovery-consistency.md) | REC-V1-01 |
| V1-REC-006 | [V1-REC-006　隔离模式的可用管理路径](../docs/05-recovery-consistency.md) | REC-V1-01, REC-V1-02 |
| V1-REC-007 | [V1-REC-007　重新登记不是历史恢复](../docs/05-recovery-consistency.md) | REC-V1-03 |
| V1-REC-008 | [V1-REC-008　RecoverySetManifest](../docs/05-recovery-consistency.md) | REC-V1-04, REC-V1-15, AT-STA-003, AT-WFL-009 |
| V1-REC-009 | [V1-REC-009　结果、附件与意图的恢复关系](../docs/05-recovery-consistency.md) | REC-V1-07, REC-V1-08 |
| V1-REC-010 | [V1-REC-010　恢复后的旧任务默认不产生新效果](../docs/05-recovery-consistency.md) | REC-V1-05, AT-CORE-003 |
| V1-REC-011 | [V1-REC-011　对账与恢复决策矩阵](../docs/05-recovery-consistency.md) | REC-V1-04, REC-V1-05, AT-WFL-009, AT-CORE-003 |
| V1-REC-012 | [V1-REC-012　幂等历史、墓碑与灾难恢复窗口](../docs/05-recovery-consistency.md) | REC-V1-06, AT-TSK-003 |
| V1-REC-013 | [V1-REC-013　恢复世代与资源栅栏](../docs/05-recovery-consistency.md) | REC-V1-09, AT-STA-012, AT-CHAT-004, AT-CORE-005 |
| V1-REC-014 | [V1-REC-014　失联节点和旧写入者](../docs/05-recovery-consistency.md) | REC-V1-09 |
| V1-REC-015 | [V1-REC-015　Conversation 已确认历史的损失边界](../docs/05-recovery-consistency.md) | REC-V1-10, AT-CHAT-010 |
| V1-REC-016 | [V1-REC-016　明确状态转换](../docs/05-recovery-consistency.md) | REC-V1-11, REC-V1-14 |
| V1-REC-017 | [V1-REC-017　激活门槛](../docs/05-recovery-consistency.md) | REC-V1-03, REC-V1-07, REC-V1-14, AT-STA-003 |
| V1-REC-018 | [V1-REC-018　新目标、原位恢复与回退](../docs/05-recovery-consistency.md) | REC-V1-11, REC-V1-12, REC-V1-14 |
| V1-REC-019 | [V1-REC-019　时间条件与离线边界](../docs/05-recovery-consistency.md) | REC-V1-13, AT-CORE-014 |
| V1-REC-020 | [V1-REC-020　容量与保留链](../docs/05-recovery-consistency.md) | REC-V1-11 |
| V1-REC-021 | [V1-REC-021　可测量目标](../docs/05-recovery-consistency.md) | REC-V1-15 |
| V1-REC-023 | [V1-REC-023　删除与保留状态不能因恢复自动倒退](../docs/05-recovery-consistency.md) | REC-V1-16, AT-CORE-018 |
| V1-REC-022 | [V1-REC-022　恢复验收矩阵](../docs/05-recovery-consistency.md) | AT-COMP-002 |
| V1-STO-001 | [V1-STO-001：三级职责及可选性](../docs/06-storage-data-tablefs.md) | AT-COMP-014 |
| V1-STO-002 | [V1-STO-002：对象身份、版本和恢复后的身份](../docs/06-storage-data-tablefs.md) | REC-V1-08, AT-STA-001 |
| V1-STO-003 | [V1-STO-003：能力与访问模式](../docs/06-storage-data-tablefs.md) | AT-STA-004 |
| V1-STO-004 | [V1-STO-004：生命周期、挂载与故障状态](../docs/06-storage-data-tablefs.md) | AT-STA-001 |
| V1-STO-005 | [V1-STO-005：容量与可操作性](../docs/06-storage-data-tablefs.md) | AT-COMP-015 |
| V1-STO-006 | [V1-STO-006：成功响应与故障模型](../docs/06-storage-data-tablefs.md) | AT-STA-002, AT-CORE-010 |
| V1-STO-007 | [V1-STO-007：一致性范围与协调](../docs/06-storage-data-tablefs.md) | AT-COMP-015 |
| V1-STO-008 | [V1-STO-008：BackupManifest 与恢复动作](../docs/06-storage-data-tablefs.md) | REC-V1-15, AT-STA-003 |
| V1-STO-009 | [V1-STO-009：RPO/RTO、保留和密钥](../docs/06-storage-data-tablefs.md) | REC-V1-15 |
| V1-DAT-001 | [V1-DAT-001：主记录字段及元数据](../docs/06-storage-data-tablefs.md) | AT-STA-005 |
| V1-DAT-002 | [V1-DAT-002：逻辑记录与大内容](../docs/06-storage-data-tablefs.md) | AT-STA-005 |
| V1-DAT-003 | [V1-DAT-003：JSON 精确查询](../docs/06-storage-data-tablefs.md) | AT-STA-005 |
| V1-DAT-004 | [V1-DAT-004：版本、视图与向量](../docs/06-storage-data-tablefs.md) | AT-STA-006 |
| V1-DAT-005 | [V1-DAT-005：查询、权限与删除传播](../docs/06-storage-data-tablefs.md) | SEC-V1-01, REC-V1-16, AT-STA-007, AT-CORE-018 |
| V1-DAT-006 | [V1-DAT-006：导出、迁移和服务退出](../docs/06-storage-data-tablefs.md) | AT-STA-011 |
| V1-TFS-001 | [V1-TFS-001：权威模型](../docs/06-storage-data-tablefs.md) | AT-COMP-014 |
| V1-TFS-002 | [V1-TFS-002：路径、名称和遍历](../docs/06-storage-data-tablefs.md) | SEC-V1-07, AT-STA-010 |
| V1-TFS-003 | [V1-TFS-003：原子内容发布与持久边界](../docs/06-storage-data-tablefs.md) | AT-STA-002, AT-CORE-010 |
| V1-TFS-004 | [V1-TFS-004：缓存、多入口写入和 mmap](../docs/06-storage-data-tablefs.md) | AT-STA-008 |
| V1-TFS-005 | [V1-TFS-005：权限、打开句柄与撤权](../docs/06-storage-data-tablefs.md) | AT-STA-007 |
| V1-TFS-006 | [V1-TFS-006：快照、并发 GC 与恢复导出](../docs/06-storage-data-tablefs.md) | AT-STA-009, AT-STA-011, AT-STA-018 |
| V1-TFS-007 | [V1-TFS-007：Dataset 映射、智能目录与索引](../docs/06-storage-data-tablefs.md) | AT-STA-010 |
| V1-TFS-008 | [V1-TFS-008：可行性门槛和集群范围](../docs/06-storage-data-tablefs.md) | AT-STA-012 |
| V1-TFS-009 | [V1-TFS-009：POSIX逐项能力与错误契约](../docs/06-storage-data-tablefs.md) | AT-STA-008 |
| V1-APP-001 | [V1-APP-001：统一身份、制品与部署](../docs/07-applications-packages-gui.md) | AT-COMP-016 |
| V1-APP-002 | [V1-APP-002：清单与生效权限](../docs/07-applications-packages-gui.md) | AT-STA-013, AT-STA-015 |
| V1-APP-003 | [V1-APP-003：隔离与运行后端](../docs/07-applications-packages-gui.md) | AT-STA-013 |
| V1-APP-004 | [V1-APP-004：生命周期分解与可观察状态](../docs/07-applications-packages-gui.md) | AT-COMP-016 |
| V1-APP-005 | [V1-APP-005：安装、启动、停止和数据绑定](../docs/07-applications-packages-gui.md) | AT-STA-014 |
| V1-APP-006 | [V1-APP-006：升级、恢复与互斥操作](../docs/07-applications-packages-gui.md) | AT-STA-014, AT-STA-015 |
| V1-APP-007 | [V1-APP-007：卸载、共享依赖与维护](../docs/07-applications-packages-gui.md) | AT-STA-015 |
| V1-APP-008 | [V1-APP-008：格式、目标环境和兼容等级](../docs/07-applications-packages-gui.md) | AT-STA-016 |
| V1-APP-009 | [V1-APP-009：导入安全与完整状态](../docs/07-applications-packages-gui.md) | SEC-V1-07, AT-STA-017 |
| V1-APP-010 | [V1-APP-010：隔离构建、来源锁与安全重建](../docs/07-applications-packages-gui.md) | AT-STA-018 |
| V1-APP-011 | [V1-APP-011：图形与特权兼容 Profile](../docs/07-applications-packages-gui.md) | AT-STA-016 |
| V1-GUI-001 | [V1-GUI-001：入口和对象边界](../docs/07-applications-packages-gui.md) | AT-STA-019 |
| V1-GUI-002 | [V1-GUI-002：权限分项和实际范围](../docs/07-applications-packages-gui.md) | AT-STA-019, AT-STA-025 |
| V1-GUI-003 | [V1-GUI-003：输入协议、去重与人类接管](../docs/07-applications-packages-gui.md) | SEC-V1-13, AT-STA-020 |
| V1-GUI-004 | [V1-GUI-004：定位、布局和连接恢复](../docs/07-applications-packages-gui.md) | AT-STA-021 |
| V1-GUI-005 | [V1-GUI-005：Portal协议和能力版本](../docs/07-applications-packages-gui.md) | AT-STA-022 |
| V1-GUI-006 | [V1-GUI-006：一次性恢复令牌和崩溃窗口](../docs/07-applications-packages-gui.md) | AT-STA-023 |
| V1-GUI-007 | [V1-GUI-007：传输与资源预算](../docs/07-applications-packages-gui.md) | AT-STA-026 |
| V1-GUI-008 | [V1-GUI-008：NativeWeb的浏览器安全边界](../docs/07-applications-packages-gui.md) | SEC-V1-10, AT-STA-024, AT-CORE-015 |
| V1-GUI-009 | [V1-GUI-009：秘密、隐私和数据去向](../docs/07-applications-packages-gui.md) | AT-STA-025 |
| V1-GUI-010 | [V1-GUI-010：第三方与节点边界](../docs/07-applications-packages-gui.md) | AT-STA-021 |
| V1-SHL-001 | [V1-SHL-001：桌面优先的产品定位](../docs/07-applications-packages-gui.md) | AT-SHL-001, AT-SHL-012 |
| V1-SHL-002 | [V1-SHL-002：菜单栏、Dock 与应用入口](../docs/07-applications-packages-gui.md) | AT-SHL-001, AT-SHL-002, AT-SHL-003 |
| V1-SHL-003 | [V1-SHL-003：多窗口与独立生命周期](../docs/07-applications-packages-gui.md) | AT-SHL-002, AT-SHL-003, AT-SHL-006, AT-SHL-011, AT-SHL-013 |
| V1-SHL-004 | [V1-SHL-004：工作区与目标上下文](../docs/07-applications-packages-gui.md) | AT-SHL-005, AT-SHL-014 |
| V1-SHL-005 | [V1-SHL-005：布局恢复与多设备](../docs/07-applications-packages-gui.md) | AT-SHL-004, AT-SHL-005, AT-SHL-009, AT-SHL-011, AT-SHL-014 |
| V1-SHL-006 | [V1-SHL-006：第一方 Web 与图形应用接入](../docs/07-applications-packages-gui.md) | AT-SHL-010, AT-SHL-011, AT-SHL-012 |
| V1-SHL-007 | [V1-SHL-007：对象协作与 Agent 上下文](../docs/07-applications-packages-gui.md) | AT-SHL-007, AT-SHL-008 |
| V1-SHL-008 | [V1-SHL-008：Shell 视图契约与隔离](../docs/07-applications-packages-gui.md) | AT-SHL-003, AT-SHL-004, AT-SHL-007, AT-SHL-009, AT-SHL-010 |
| V1-SHL-009 | [V1-SHL-009：任务、通知与真实状态](../docs/07-applications-packages-gui.md) | AT-SHL-006, AT-SHL-008, AT-SHL-009 |
| V1-SHL-010 | [V1-SHL-010：一致视觉与资源预算](../docs/07-applications-packages-gui.md) | AT-SHL-015 |
| V1-SHL-011 | [V1-SHL-011：浏览器、无障碍与窄屏](../docs/07-applications-packages-gui.md) | AT-SHL-013, AT-SHL-014 |
| V1-SHL-012 | [V1-SHL-012：三版一致与阶段交付](../docs/07-applications-packages-gui.md) | AT-SHL-001, AT-SHL-012, AT-SHL-015 |
| V1-APP-012 | [V1-APP-012：产物与应用的统一映射](../docs/07-applications-packages-gui.md) | AT-AIF-001, AT-AIF-008, AT-AIF-010 |
| V1-APP-013 | [V1-APP-013：可验证的交付描述](../docs/07-applications-packages-gui.md) | AT-AIF-001, AT-AIF-002, AT-AIF-009 |
| V1-APP-014 | [V1-APP-014：从提交到安装的受控流程](../docs/07-applications-packages-gui.md) | AT-AIF-001, AT-AIF-003, AT-AIF-005, AT-AIF-006, AT-AIF-007 |
| V1-APP-015 | [V1-APP-015：预览、构建与运行边界](../docs/07-applications-packages-gui.md) | AT-AIF-004, AT-AIF-005, AT-AIF-009 |
| V1-APP-016 | [V1-APP-016：版本、保留与日常管理](../docs/07-applications-packages-gui.md) | AT-AIF-007, AT-AIF-008, AT-AIF-010 |
| V1-TSK-001 | [V1-TSK-001（必须）：`task_id`、`run_id`、`operation_id`、`event_id`、`artifact_id` 由各自权威分配且不得复用](../docs/08-tasks-events.md) | AT-COMP-009 |
| V1-TSK-002 | [V1-TSK-002（必须）：创建请求不携带尚未分配的 `task_id` 或 `run_id`](../docs/08-tasks-events.md) | AT-TSK-001 |
| V1-TSK-003 | [V1-TSK-003（必须）：在一个本地事务或经过验证的等效协议中完成：提交去重判定、输入清单引用与保留关系、任务元数据容量预留、Task 记录、必要审计意图和 outbox](../docs/08-tasks-events.md) | AT-TSK-001, AT-TSK-008, AT-CORE-007 |
| V1-TSK-004 | [V1-TSK-004（必须）：`NotFound` 的普通资源查询不等价于 `AbsentWithinWindow`](../docs/08-tasks-events.md) | AT-TSK-002, AT-TSK-003 |
| V1-TSK-005 | [V1-TSK-005（必须）：Workflow 在自己的权威事务中写入 `DispatchIntent + outbox`](../docs/08-tasks-events.md) | SEC-V1-04, REC-V1-04, AT-TSK-002, AT-CORE-001 |
| V1-TSK-006 | [V1-TSK-006（必须）：取消不能只删除 Workflow outbox](../docs/08-tasks-events.md) | AT-TSK-004, AT-WFL-005, AT-CORE-002 |
| V1-TSK-007 | [V1-TSK-007（必须）：所有任务查询同时返回生命周期 `lifecycle`、结果知识 `outcome_knowledge`、条件列表、结果引用状态和当前修订](../docs/08-tasks-events.md) | AT-TSK-006, AT-WFL-003, AT-CORE-003 |
| V1-TSK-008 | [V1-TSK-008（必须）：工具至少声明 `ReadOnly / IdempotentWrite / CompensatableWrite / NonRetryableEffect`，并声明幂等保证的范围、有效期、结果…](../docs/08-tasks-events.md) | AT-TSK-006, AT-AGT-001, AT-AGT-006, AT-WFL-006 |
| V1-TSK-009 | [V1-TSK-009（必须）：Run 结果提交包含 Task 修订、Run ID、领取代次、恢复世代、输入/执行版本摘要和结果引用](../docs/08-tasks-events.md) | AT-TSK-005 |
| V1-TSK-010 | [V1-TSK-010（必须）：大结果先上传到隔离暂存位置，验证大小、内容摘要和安全域，再提交不可变 Artifact 引用](../docs/08-tasks-events.md) | REC-V1-07, AT-CORE-010 |
| V1-TSK-011 | [V1-TSK-011（必须）：事件信封包含 `event_id`、`event_type`、`schema_version`、来源权威、资源引用/修订、发生时间与接收时间、关联/因果 ID、必要 Task/Run ID、…](../docs/08-tasks-events.md) | AT-TSK-007, AT-CORE-001 |
| V1-TSK-012 | [V1-TSK-012（必须）：列表快照返回可连续订阅的权威游标；订阅中途失效返回 `CursorExpired`，要求重新列举并从新游标恢复](../docs/08-tasks-events.md) | AT-TSK-007 |
| V1-TSK-013 | [V1-TSK-013（必须）：定义消息大小、并发、积压字节、重试次数/总时限与退避](../docs/08-tasks-events.md) | AT-TSK-007, AT-TSK-008 |
| V1-TSK-014 | [V1-TSK-014（必须）：父子任务共享可核算的预算树](../docs/08-tasks-events.md) | SEC-V1-02, AT-TSK-009 |
| V1-TSK-015 | [V1-TSK-015（必须）：Schedule 定义包括稳定 ID、修订、IANA 时区、当地触发规则、DST 重复/缺失时刻处理、错过触发的 Skip/Coalesce/BoundedCatchUp、重叠策略、截止时间…](../docs/08-tasks-events.md) | AT-TSK-009 |
| V1-TSK-016 | [V1-TSK-016（必须）：关机先停止新接受/领取、持久化在途状态、使执行者到无写入安全点或由资源端 fencing，再释放会触发替代调度的租约](../docs/08-tasks-events.md) | AT-CORE-002 |
| V1-AGT-001 | [V1-AGT-001（必须）：Task 进入统一 Broker 后，由调度器选择匹配能力的 Runtime/Adapter](../docs/09-agents-models-protocols.md) | AT-COMP-008 |
| V1-AGT-002 | [V1-AGT-002（必须）：每个 Run 记录以下执行清单，并在持久恢复时验证：](../docs/09-agents-models-protocols.md) | AT-AGT-001, AT-AGT-002, AT-AGT-008 |
| V1-AGT-003 | [V1-AGT-003（必须）：运行日志保存目标、计划摘要、工具调用、返回值引用、执行证据和用户可见说明](../docs/09-agents-models-protocols.md) | AT-COMP-008 |
| V1-AGT-004 | [V1-AGT-004（必须）：Skill 清单将说明文本、工具依赖、可执行脚本、来源/签名、所申请能力、参数 Schema、版本与许可证分开记录](../docs/09-agents-models-protocols.md) | AT-AGT-002 |
| V1-AGT-005 | [V1-AGT-005（必须）：网页、README、Issue、聊天、截图 OCR、工具结果和检索片段携带来源与安全域，按不可信数据处理](../docs/09-agents-models-protocols.md) | SEC-V1-08 |
| V1-AGT-006 | [V1-AGT-006（必须）：模型登记记录文本/图像/音频等输入输出模态、工具调用格式、上下文与输出上限、并发、流式与取消支持、部署位置、数据保留/出口约束，以及价格/额度信息的来源时间](../docs/09-agents-models-protocols.md) | AT-AGT-003, AT-AGT-008, AT-CORE-015 |
| V1-AGT-007 | [V1-AGT-007（必须）：父 Task/Workflow 的预算采用预留、计量和结算；重试、压缩、自动聊天、子 Agent 以及模型切换都计入共同根预算](../docs/09-agents-models-protocols.md) | AT-CHAT-009 |
| V1-AGT-008 | [V1-AGT-008（必须）：兼容声明是“具体实现 + 版本/提交 + 传输 + 认证 + Profile + 方法/限制 + 通过用例”的元组，不能写成“兼容所有第三方 Agent”](../docs/09-agents-models-protocols.md) | AT-AGT-004 |
| V1-AGT-009 | [V1-AGT-009（必须）：适配器将协议时代、传输、认证和业务能力分别登记](../docs/09-agents-models-protocols.md) | AT-AGT-004, AT-AGT-007 |
| V1-AGT-010 | [V1-AGT-010（必须）：HTTP OAuth 路径与 STDIO 进程凭据路径分开实现](../docs/09-agents-models-protocols.md) | AT-AGT-004 |
| V1-AGT-011 | [V1-AGT-011（必须）：持久映射键为 `(adapter_id, endpoint_identity, remote_tenant_scope, external_task_id)`，绑定本地 Task/Run/O…](../docs/09-agents-models-protocols.md) | AT-AGT-005 |
| V1-AGT-012 | [V1-AGT-012（必须）：A2A 发送是否幂等依据具体服务器契约和实测，不仅依据 `messageId`](../docs/09-agents-models-protocols.md) | AT-AGT-006 |
| V1-AGT-013 | [V1-AGT-013：共享合同与能力发现](../docs/09-agents-models-protocols.md) | AT-AIF-001, AT-AIF-002, AT-AIF-005, AT-AIF-010 |
| V1-AGT-014 | [V1-AGT-014：提交、安装与回执合同](../docs/09-agents-models-protocols.md) | AT-AIF-001, AT-AIF-002, AT-AIF-003, AT-AIF-006, AT-AIF-009 |
| V1-AGT-015 | [V1-AGT-015：对话入口、授权与中断恢复](../docs/09-agents-models-protocols.md) | AT-AIF-003, AT-AIF-004, AT-AIF-005, AT-AIF-006, AT-AIF-010 |
| V1-CHAT-001 | [V1-CHAT-001（必须）：会话生命周期为 `Active / Archived / Deleting / Deleted`](../docs/10-conversations.md) | AT-CHAT-006 |
| V1-CHAT-002 | [V1-CHAT-002（必须）：成员生命周期为 `Invited / Active / Left / Removed / Expired`](../docs/10-conversations.md) | AT-CHAT-003, AT-CHAT-006, AT-CHAT-008 |
| V1-CHAT-003 | [V1-CHAT-003（必须）：成员变化和消息提交必须有可比较的顺序](../docs/10-conversations.md) | AT-CHAT-003 |
| V1-CHAT-004 | [V1-CHAT-004（必须）：服务器接受回执至少包含 `message_id`、`conversation_id`、`client_send_id`、`conversation_seq`、`writer_epoch`、…](../docs/10-conversations.md) | AT-CHAT-001, AT-CHAT-007 |
| V1-CHAT-005 | [V1-CHAT-005（必须）：唯一去重键为 `(security_domain_id, conversation_id, authenticated_sender, client_send_id)`；同键同摘要返回原消…](../docs/10-conversations.md) | REC-V1-10, AT-CHAT-001, AT-CHAT-002 |
| V1-CHAT-006 | [V1-CHAT-006（必须）：每个 Conversation 在任一时刻只有一个可提交的 Writer](../docs/10-conversations.md) | AT-CHAT-004 |
| V1-CHAT-007 | [V1-CHAT-007（必须）：持久性声明写明 `durability_profile` 的故障范围：例如已 fsync 的本机崩溃恢复、复制后节点故障或独立备份的整盘灾难恢复](../docs/10-conversations.md) | REC-V1-10, AT-CHAT-010 |
| V1-CHAT-008 | [V1-CHAT-008（必须）：明确分开以下事实：](../docs/10-conversations.md) | AT-CHAT-005, AT-CHAT-008 |
| V1-CHAT-009 | [V1-CHAT-009（必须）：普通群消息默认记录并按订阅通知，不唤醒所有 Agent](../docs/10-conversations.md) | AT-CHAT-009, AT-CORE-001 |
| V1-CHAT-010 | [V1-CHAT-010（必须）：循环检测不能只靠模型理解](../docs/10-conversations.md) | AT-CHAT-009 |
| V1-CHAT-011 | [V1-CHAT-011（必须）：附件先在隔离区域上传、验证大小/类型/校验和与访问权限，再将不可变引用纳入消息](../docs/10-conversations.md) | SEC-V1-10, AT-CHAT-007, AT-CORE-010 |
| V1-CHAT-012 | [V1-CHAT-012（必须）：消息修改生成内容修订和审计关系，任务已引用版本保持可验证；撤回生成墓碑并阻止后续普通读取，不能承诺收回已经交给外部模型/远端桥接的副本](../docs/10-conversations.md) | REC-V1-16, AT-CHAT-002, AT-CORE-018 |
| V1-CHAT-013 | [V1-CHAT-013（必须）：外部 Agent/聊天适配器登记外部身份与 Principal 的绑定、协议全名/版本、端点身份、数据流向、投递/撤回支持和已通过用例](../docs/10-conversations.md) | AT-COMP-012 |
| V1-CHAT-014 | [V1-CHAT-014（必须）：Shell、CLI、SDK/API 提供会话/DM 列表、线程、提及、查询、发送、成员管理、任务关联、归档、导出及暂停自动回复](../docs/10-conversations.md) | AT-COMP-012 |
| V1-WFL-001 | [V1-WFL-001（必须）：所有对象具有稳定 ID、安全域、所有者、Schema 版本、创建/更新责任主体和资源修订](../docs/11-workflows-projects.md) | AT-WFL-010 |
| V1-WFL-002 | [V1-WFL-002（必须）：Definition 以版本化 AST 为语义权威](../docs/11-workflows-projects.md) | AT-WFL-001 |
| V1-WFL-003 | [V1-WFL-003（必须）：编译/登记时检测重复步骤 ID、悬空依赖、非法回边、永远不能满足的 Join、输入输出类型不匹配、无界循环/展开、未声明权限、没有失败路径的审批、无法解释的补偿和不可满足预算](../docs/11-workflows-projects.md) | AT-WFL-002 |
| V1-WFL-004 | [V1-WFL-004（必须）：Workflow 在本地权威事务中写步骤派发状态、DispatchIntent、预算预留引用和 outbox](../docs/11-workflows-projects.md) | SEC-V1-04, AT-TSK-002, AT-WFL-001, AT-WFL-005 |
| V1-WFL-005 | [V1-WFL-005（必须）：WorkflowRun 的生命周期为 `Draft / Ready / Running / WaitingApproval / WaitingInput / Blocked / Cancel…](../docs/11-workflows-projects.md) | AT-AGT-005, AT-WFL-002, AT-WFL-003 |
| V1-WFL-006 | [V1-WFL-006（必须）：并行“先成功者获胜”不能假装消除了其他已运行分支的效果](../docs/11-workflows-projects.md) | AT-WFL-003 |
| V1-WFL-007 | [V1-WFL-007（必须）：ApprovalGate 创建或引用 Security 权威的 ApprovalRequest](../docs/11-workflows-projects.md) | SEC-V1-03, SEC-V1-08, AT-WFL-004, AT-CORE-004 |
| V1-WFL-008 | [V1-WFL-008（必须）：暂停阻止释放新步骤；取消持久关闭取消域并逐一处理派发中/已接受 Task，采用 Task 章节的 CancelOrigin 墓碑与对账协议](../docs/11-workflows-projects.md) | AT-WFL-005, AT-CORE-003 |
| V1-WFL-009 | [V1-WFL-009（必须）：补偿计划在原步骤被接受前登记其可补偿范围、执行者权限、输入依赖和失败处理](../docs/11-workflows-projects.md) | AT-WFL-006 |
| V1-WFL-010 | [V1-WFL-010（必须）：运行固定 Definition 版本、DSL 解释器语义版本、输入快照和相关 Skill/工具版本](../docs/11-workflows-projects.md) | AT-WFL-001, AT-WFL-009 |
| V1-WFL-011 | [V1-WFL-011（必须）：WorkItem 保存标题/描述、项目与里程碑、优先级、父子/阻塞关系、指派、验收条件、目标资产、文档、Task/WorkflowRun、外部 Issue 和交付证据](../docs/11-workflows-projects.md) | AT-COMP-013 |
| V1-WFL-012 | [V1-WFL-012（必须）：ExternalLink 记录 `provider`、端点/实例身份、外部租户/仓库、对象种类和稳定外部 ID、本地资源引用、字段权威表、修订类型、最后权威读取版本/时间、同步状态、事件去重…](../docs/11-workflows-projects.md) | AT-WFL-007 |
| V1-WFL-013 | [V1-WFL-013（必须）：外部 `revision` 不一定是可排序整数；Git SHA、ETag、随机版本 ID 不按字符串大小或时间戳比较新旧](../docs/11-workflows-projects.md) | REC-V1-05, AT-WFL-007 |
| V1-WFL-014 | [V1-WFL-014（必须）：参考流程为 Plan、Implement、CI、IndependentReview、ProtectedMerge、Deploy/Verify、Close；每一关绑定实际资源和版本](../docs/11-workflows-projects.md) | AT-WFL-004, AT-WFL-008, AT-CORE-011 |
| V1-CLU-001 | [V1-CLU-001｜加入集群不移交整机](../docs/12-cluster-coordination.md) | AT-CORE-017 |
| V1-CLU-002 | [V1-CLU-002｜身份引导一次、正式身份可轮换](../docs/12-cluster-coordination.md) | AT-CORE-017 |
| V1-CLU-003 | [V1-CLU-003｜成员状态机](../docs/12-cluster-coordination.md) | AT-COMP-017 |
| V1-CLU-004 | [V1-CLU-004｜任务领取不是资源写锁](../docs/12-cluster-coordination.md) | AT-TSK-005, AT-CORE-005 |
| V1-CLU-005 | [V1-CLU-005｜token具有资源级次序](../docs/12-cluster-coordination.md) | REC-V1-09, AT-STA-004, AT-STA-012, AT-STA-014, AT-CORE-005 |
| V1-CLU-006 | [V1-CLU-006｜资源端拒旧不等于取消外部动作](../docs/12-cluster-coordination.md) | AT-CORE-003 |
| V1-CLU-007 | [V1-CLU-007｜幂等不替代授权](../docs/12-cluster-coordination.md) | REC-V1-13, AT-CORE-014 |
| V1-CLU-008 | [V1-CLU-008｜旧快照恢复先隔离后对账](../docs/12-cluster-coordination.md) | AT-CORE-005 |
| V1-CLU-009 | [V1-CLU-009｜恢复后所有控制订阅失效](../docs/12-cluster-coordination.md) | AT-CORE-006 |
| V1-CLU-010 | [V1-CLU-010｜单Controller不宣称HA](../docs/12-cluster-coordination.md) | AT-COMP-017 |
| V1-CLU-011 | [V1-CLU-011｜实际准入二次确认](../docs/12-cluster-coordination.md) | AT-CORE-016 |
| V1-CLU-012 | [V1-CLU-012｜升级前排空，能力重协商](../docs/12-cluster-coordination.md) | AT-COMP-017 |
| V1-OPS-001 | [V1-OPS-001｜配置来源可见](../docs/13-operations-release-governance.md) | AT-CORE-020 |
| V1-OPS-002 | [V1-OPS-002｜期望配置提交与实际应用分开](../docs/13-operations-release-governance.md) | AT-CORE-020 |
| V1-OPS-003 | [V1-OPS-003｜Drain有可执行顺序](../docs/13-operations-release-governance.md) | AT-CORE-002, AT-CORE-020 |
| V1-OPS-004 | [V1-OPS-004｜压力和容量覆盖隐藏元数据](../docs/13-operations-release-governance.md) | AT-TSK-008, AT-CORE-007, AT-CORE-016 |
| V1-OPS-005 | [V1-OPS-005｜退役不止删除节点](../docs/13-operations-release-governance.md) | AT-CORE-017 |
| V1-OBS-001 | [V1-OBS-001｜最低诊断常驻宿主](../docs/13-operations-release-governance.md) | AT-CORE-012 |
| V1-OBS-002 | [V1-OBS-002｜四类数据独立](../docs/13-operations-release-governance.md) | AT-COMP-018 |
| V1-OBS-003 | [V1-OBS-003｜审计先意图后结果](../docs/13-operations-release-governance.md) | AT-COMP-018 |
| V1-OBS-004 | [V1-OBS-004｜隐私、保留和外发](../docs/13-operations-release-governance.md) | AT-CORE-018 |
| V1-OBS-005 | [V1-OBS-005｜告警有可操作原因](../docs/13-operations-release-governance.md) | AT-COMP-018 |
| V1-REL-001 | [V1-REL-001｜精确输入与制品绑定](../docs/13-operations-release-governance.md) | AT-STA-018, AT-CORE-011 |
| V1-REL-002 | [V1-REL-002｜审查、构建、签名、发布分离](../docs/13-operations-release-governance.md) | AT-CORE-011 |
| V1-REL-003 | [V1-REL-003｜更新信任与应急](../docs/13-operations-release-governance.md) | AT-COMP-019 |
| V1-REL-004 | [V1-REL-004｜门禁按真实攻击面](../docs/13-operations-release-governance.md) | AT-COMP-002, AT-COMP-005, AT-COMP-007 |
| V1-REL-005 | [V1-REL-005｜发布声明逐功能](../docs/13-operations-release-governance.md) | AT-CORE-011 |
| V1-REL-006 | [V1-REL-006｜维护政策在正式发布前冻结](../docs/13-operations-release-governance.md) | AT-COMP-003 |
| V1-DEVX-001 | [V1-DEVX-001｜开发者契约可检查](../docs/13-operations-release-governance.md) | AT-COMP-016 |
| V1-GOV-006 | [V1-GOV-006｜维护责任到模块](../docs/13-operations-release-governance.md) | AT-COMP-004 |
| V1-GOV-007 | [V1-GOV-007｜复用来源不自动可信](../docs/13-operations-release-governance.md) | AT-COMP-004 |
| V1-GOV-008 | [V1-GOV-008｜复杂度预算](../docs/13-operations-release-governance.md) | AT-COMP-003 |
| V1-PLAN-001 | [V1-PLAN-001｜先限定支持范围，再执行验收](../docs/14-roadmap-and-decisions.md) | AT-COMP-003 |
| V1-PLAN-002 | [V1-PLAN-002｜按可恢复闭环交付](../docs/14-roadmap-and-decisions.md) | AT-COMP-021 |
| V1-PLAN-003 | [V1-PLAN-003｜选择必须可反驳](../docs/14-roadmap-and-decisions.md) | AT-COMP-003 |
| V1-PLAN-004 | [V1-PLAN-004｜失败有具体后果](../docs/14-roadmap-and-decisions.md) | AT-COMP-003 |
| V1-TEST-001 | [V1-TEST-001｜文档检查不能冒充产品验收](../docs/15-acceptance-and-evidence.md) | AT-COMP-002 |
| V1-TEST-002 | [V1-TEST-002｜证据绑定确切对象](../docs/15-acceptance-and-evidence.md) | AT-COMP-002 |
| V1-TEST-003 | [V1-TEST-003｜重点验证不能从局部成功推导的性质](../docs/15-acceptance-and-evidence.md) | AT-COMP-002 |
| V1-TEST-004 | [V1-TEST-004｜阈值必须预先冻结](../docs/15-acceptance-and-evidence.md) | AT-COMP-003 |
| V1-TEST-005 | [V1-TEST-005｜覆盖可查但不伪造一对一](../docs/15-acceptance-and-evidence.md) | AT-COMP-002 |
| V1-IF-001 | [V1-IF-001｜请求、权威记录和展示投影分开](../docs/16-interface-catalog-and-glossary.md) | AT-COMP-009 |
| V1-IF-002 | [V1-IF-002｜计数字符串不能偷偷转number](../docs/16-interface-catalog-and-glossary.md) | AT-CORE-019 |
| V1-IF-003 | [V1-IF-003｜同名状态始终限定所属对象](../docs/16-interface-catalog-and-glossary.md) | AT-COMP-015, AT-COMP-016 |
| V1-IF-004 | [V1-IF-004｜结构合同不放行不可信数据](../docs/16-interface-catalog-and-glossary.md) | AT-COMP-020 |
