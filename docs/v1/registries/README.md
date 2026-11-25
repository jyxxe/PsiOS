# 机器登记表

所有登记表的状态均按00章区分。当前规范定位来自docs，不允许手改requirements.json创造新的规范。

| 文件 | 用途与维护方式 |
|---|---|
| [requirements.json](requirements.json) / [可读索引](requirements.md) | v1唯一ID、定义位置、摘要、验证计划；由build_package.py生成 |
| [findings.json](findings.json) | 本次逐项缺陷与修正证据；只关闭规范问题，不自动关闭工程风险 |
| [legacy-requirements.json](legacy-requirements.json) | 所有旧要求/测试的原句、来源、范围展开及新去向；由migration工具生成 |
| [adrs.json](adrs.json) | 25份待决ADR、候选、原范围与门槛；接受须连同正文、实现与证据修改 |
| [work-items.json](work-items.json) | 28个稳定工程工作项、依赖和旧编号来源；未创建外部工单 |
| [test-cases.json](test-cases.json) | 产品验证计划、夹具、注入、预期及证据；本版全部NotRun |

外部参考资料另在[sources/references.json](../sources/references.json)，Schema清单在[schemas/catalog.json](../schemas/catalog.json)，样例清单在[examples/manifest.json](../examples/manifest.json)。不能将不同表的同名status跨层比较。
