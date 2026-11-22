# 正反示例与摘要向量

所有内容均为虚构夹具。时间是固定示例时间，资源引用不对应可访问系统，许可例子也不是有效令牌。`manifest.json`是样例清单，标明目标Schema、预期Valid/Invalid及拒绝层次；`digest-vectors.json`保留规范化字节与预期摘要。

`positive/`包含：完整任务提交/接受/查询、已取消但效果未知的Operation、结果已回收但历史成功仍成立的Task、丢失联系且效果未知的Run、聊天发送与大整数序号回执、最小工作流、派发意图、Dataset、卷、撤权及恢复等。Dataset的`extra_data`数组和null都合法，score=0与未评分null不同。

`negative/`包含：客户端伪造task_id或sender、遗漏发送ID、摘要改变、用JSON number表示序号、前导零/溢出的代次、Succeeded+Unknown、审批已绑定却无Operation、未认证许可结构、重复标签、空向量、缺评分Profile、只读挂载获得写权、失联目标却声称撤权Complete、隔离恢复开自动写、删除历史未知却公开分享、工作流重号/环路、回执与派发不匹配等。

| validation | 如何判断 |
|---|---|
| Schema | 该对象必须被目标Schema拒绝；解析器或引用加载故障不能冒充此结果 |
| Digest | 结构先通过，再因本合同payload与摘要不一致拒绝 |
| Semantic | 结构先通过，再因列明的局部跨字段/图约束拒绝；不宣称覆盖远程授权或事务 |
| SchemaAndLocalSemantics | 正例同时通过结构、已实现的文档级关系检查及适用摘要检查 |

运行`python tools/validate_package.py`读取这些样例。它不执行Task、工作流、撤权、恢复或归档源码。当前真实产品验收仍全部NotRun，详见acceptance目录。
