# 文档构建与校验工具

这些脚本只维护本包文档及数据，不是PsiOS产品实现，不执行sources/archive中的代码。Python 3.10或更新版本；校验依赖见requirements.txt。重建HTML另需系统Pandoc；交付的html/index.html可直接离线阅读。

## 工具与写入范围

| 工具 | 行为 |
|---|---|
| build_contracts.py | 按版本化定义生成schemas和正反样例；摘要帮助函数仅覆盖夹具子集 |
| build_package.py | 生成需求索引、修正台账、来源索引、README、完整Markdown与离线HTML |
| validate_package.py | 默认只读检查；--report只写指定文档验证报告 |
| build_package.py --seal | 最后生成MANIFEST.sha256，明确排除其自身 |
| build_package.py --zip PATH | 先完整校验，再创建单一PsiOS-v1顶层目录的ZIP并检查CRC/路径 |
| ../migration/rebuild_legacy_registry.py | 从已归档原文重建旧编号及定位映射；输入路径见脚本说明 |

## 修改后重建

```bash
python tools/build_contracts.py
python tools/build_package.py
python tools/validate_package.py --report validation-report.json
python tools/build_package.py --seal
python tools/validate_package.py
python tools/build_package.py --zip ../PsiOS-v1.0.0-docs-2026-11-22.zip
```

修改Schema生成器后应检查是否有退役样例，不能留未列入manifest的旧反例。不要手改生成的合订本、HTML或需求索引形成第二权威。原始archive保持字节不变。

## 实际检查范围

严格JSON读取、Schema语法/本地ref、正反例与拒绝层次、无损计数范围、夹具摘要向量、局部关系/依赖图、规范ID、链接、注册表定位、产品计划仍NotRun、原文定义切片与归档SHA、最终交付清单。未覆盖的签名真实性、分布式并发、真实fencing、断电、硬件、协议互通和性能在报告中明确列为NotProven。

历史映射中的原文切片对应 2026-11-22 封存交付。仓库检出会离线读取并校验同级 releases 目录的五个分段。单独分发修订版时，使用 `python tools/validate_package.py --original-delivery /path/to/PsiOS-v1.0.0-docs-2026-11-22.zip` 复核历史切片。当前分章和来源副本分别按当前清单核验。
