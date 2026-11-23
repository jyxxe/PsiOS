#!/usr/bin/env python3
"""Build the human-readable indexes, complete Markdown, offline HTML and ZIP.

Does not execute PsiOS or source archives. Default mode derives documents from
docs/ and the current registries. --seal updates hashes; --zip validates first.
Pandoc is required only for regenerating HTML, not for reading or validating it.
"""
from __future__ import annotations
import argparse
from collections import Counter
import hashlib
import html
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import zipfile
from urllib.parse import urlsplit, unquote

from validate_package import requirements, strict_load

ROOT=Path(__file__).resolve().parents[1]
DATE='2026-11-22'
LINK=re.compile(r'(?<!!)\[([^\]\n]+)\]\(([^)\n]+)\)')


def write(path,text):
    p=ROOT/path;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(text,encoding='utf-8')


def write_json(path,data):write(path,json.dumps(data,ensure_ascii=False,indent=2)+'\n')


def file_list():
    return sorted(p for p in ROOT.rglob('*') if p.is_file() and '__pycache__' not in p.parts and p.suffix!='.pyc')


def md_cell(text):return str(text).replace('|','\\|').replace('\n',' ')


def normalize_tables():
    for p in ROOT.rglob('*.md'):
        if 'archive' in p.relative_to(ROOT).parts or p.name=='PsiOS-v1-complete.md':continue
        lines=p.read_text(encoding='utf-8').splitlines(keepends=True)
        fixed=[]
        for line in lines:
            if line.startswith('|'):
                line=re.sub(r'`([^`]+)`',lambda m:'`'+re.sub(r'(?<!\\)\|',r'\\|',m.group(1))+'`',line)
            fixed.append(line)
        result=''.join(fixed)
        if result!=p.read_text(encoding='utf-8'):p.write_text(result,encoding='utf-8')


def regenerate_indexes():
    tests=strict_load(ROOT/'registries/test-cases.json')
    coverage={}
    for t in tests:
        for q in t['requirement_ids']:coverage.setdefault(q,[]).append(t['id'])
    reqs=requirements()
    for q in reqs:
        q.update(design_status='Specified',implementation_status='NotVerified',
                 planned_test_ids=coverage.get(q['id'],[]),
                 verification_plan_status='Planned' if coverage.get(q['id']) else 'NeedsDedicatedPlan')
    write_json('registries/requirements.json',{'document_version':'1.0.0',
        'authority':'Current definitions in docs/; this is a generated location and coverage index.',
        'coverage_is_not_execution':True,'requirements':reqs})
    out=['# v1 规范索引与验证计划\n',
         '本索引从当前17份分章生成。每个ID只有一个定义，表中的测试是待运行验证计划；存在映射不表示已经通过。完整要求以目标分章为准。\n',
         '| 要求ID | 定义入口 | 待运行测试 |\n|---|---|---|']
    for q in reqs:
        heading=q['heading'].replace('**','')
        title=heading.split('。',1)[0]
        if len(title)>110:title=title[:110]+'…'
        out.append(f"| {q['id']} | [{md_cell(title)}](../{q['document']}) | {', '.join(q['planned_test_ids']) or '需要专门计划'} |")
    write('registries/requirements.md','\n'.join(out)+'\n')
    findings=strict_load(ROOT/'registries/findings.json')['findings']
    sev=Counter(f['severity'] for f in findings);kinds=Counter(f['kind'] for f in findings)
    out=['# v0.x → v1 问题与修正台账\n',
         f'共{len(findings)}项独立登记：P0 {sev["P0"]}项、P1 {sev["P1"]}项、P2 {sev["P2"]}项。它们包括原文错误、含义歧义及需要补成可执行合同的设计缺口，不是已确认的源码漏洞数量。\n',
         '每条`CorrectedInV1`仅说明本包文字或定义已修正/边界已明确；实现均为`NotVerified`，相关产品验收均需按acceptance执行。缺失历史报告无法由文字修订补回，台账保留其证据缺口。\n',
         '| ID | 优先级 | 类型 | 问题 | 修正位置 |\n|---|---|---|---|---|']
    kind_cn={'error':'错误','ambiguity':'歧义','gap':'缺口','inconsistency':'不一致','underspecified':'定义不足'}
    for f in findings:
        target=f['target'].split(' §',1)[0].split('；',1)[0].split('、',1)[0]
        brief=f['finding'][:105]+('…' if len(f['finding'])>105 else '')
        out.append(f"| {f['id']} | {f['severity']} | {kind_cn.get(f['kind'],f['kind'])} | {md_cell(brief)} | [{md_cell(f['target'])}](../{target}) |")
    out.append('\n## 逐项证据与处理\n')
    for f in findings:
        target=f['target'].split(' §',1)[0].split('；',1)[0].split('、',1)[0]
        out += [f"### {f['id']}｜{f['severity']}｜{kind_cn.get(f['kind'],f['kind'])}\n",
                '**原文依据：** '+f['source']+'\n', '**问题：** '+f['finding']+'\n',
                '**v1修正：** '+f['correction']+'\n',
                f"**位置：** [{f['target']}](../{target})\n",
                '**验证方向：** '+f['verification']+'\n',
                '**状态：** CorrectedInV1（规范层）；NotVerified（实现层）。\n']
    write('migration/findings.md','\n'.join(out))
    references=strict_load(ROOT/'sources/references.json')['references']
    out=['# 来源、核验与引用边界\n',
         '原始项目输入完整保存在archive；来源清单记录原上传名、ID、字节数和SHA-256。外部资料只用一手规范或官方文档支持紧邻机制，不为PsiOS提供实现或认证证明。核验日：2026-11-22。\n',
         '带latest/main/current的网址可能变化；实际依赖冻结需另记录版本/tag/commit。历史归档与当前规范发生冲突时，先按00章查迁移，不从旧源恢复宽松行为。\n',
         '| 编号 | 官方资料 | 支持的范围 |\n|---|---|---|']
    for r in references:
        out.append(f"| {r['id']} | [{md_cell(r['title'])}]({r['url']}) | {md_cell('；'.join(r['claims']))} |")
    out += ['\n## 原始输入与代码边界\n',
            '[原始输入清单](source-inventory.json) · [历史源说明](archive/README.md) · [第三方依赖治理](../adr/ADR-017.md)。\n',
            '此包未获取当前Forgejo HEAD，未安装或执行归档代码。保存源文件是为了可追溯，不把历史文件的计划、测试数字或脚本自动提升为当前权威。\n']
    write('sources/README.md','\n'.join(out))
    write('registries/README.md', '''# 机器登记表

所有登记表的状态均按00章区分。当前规范定位来自docs，不允许手改requirements.json创造新的规范。

| 文件 | 用途与维护方式 |
|---|---|
| [requirements.json](requirements.json) / [可读索引](requirements.md) | v1唯一ID、定义位置、摘要、验证计划；由build_package.py生成 |
| [findings.json](findings.json) | 本次逐项缺陷与修正证据；只关闭规范问题，不自动关闭工程风险 |
| [legacy-requirements.json](legacy-requirements.json) | 所有旧要求/测试的原句、来源、范围展开及新去向；由migration工具生成 |
| [adrs.json](adrs.json) | 25份待决ADR、候选、原范围与门槛；接受须连同正文、实现与证据修改 |
| [work-items.json](work-items.json) | 26个稳定工程工作项、依赖和旧编号来源；未创建外部工单 |
| [test-cases.json](test-cases.json) | 产品验证计划、夹具、注入、预期及证据；本版全部NotRun |

外部参考资料另在[sources/references.json](../sources/references.json)，Schema清单在[schemas/catalog.json](../schemas/catalog.json)，样例清单在[examples/manifest.json](../examples/manifest.json)。不能将不同表的同名status跨层比较。
''')
    return reqs,tests,findings,references,sev,kinds


def create_readme(stats):
    reqs,tests,findings,refs,sev,kinds=stats
    cat=strict_load(ROOT/'schemas/catalog.json')['contracts']
    ex=strict_load(ROOT/'examples/manifest.json')['examples']
    legacy=strict_load(ROOT/'registries/legacy-requirements.json')['summary']
    docs=sorted((ROOT/'docs').glob('*.md'))
    titles={p:p.read_text(encoding='utf-8').splitlines()[0].lstrip('# ') for p in docs}
    lines=['# PsiOS v1｜完整规范与工程交付基线\n',
           '**文档版本1.0.0 · 2026-11-22 · 设计基线，待工程验证**\n',
           '本包整合v0.1～v0.6、Project导入材料、完整Space规划与风险台账，统一当前规则并保留历史来源。推荐先用[离线阅读器](html/index.html)浏览，或阅读[完整Markdown合订本](PsiOS-v1-complete.md)。精确文件目录见[DIRECTORY.md](DIRECTORY.md)。\n',
           'v1是文档版本；当前没有宣称软件已发布、最新仓库已审计、硬件已通过或技术选型已经批准。\n',
           '## 本包包含什么\n',
           '| 内容 | 实际数量/范围 |\n|---|---|',
           f'| 独立规范章节 | {len(docs)}章，含架构、共同合同、安全、恢复、存储、应用、Agent、Chat、Workflow、集群、发布、路线和词典 |',
           f'| v1规范条款 | {len(reqs)}条，唯一ID与验证计划可追踪 |',
           f'| 问题修正记录 | {len(findings)}项：P0={sev["P0"]}、P1={sev["P1"]}、P2={sev["P2"]}；均为规范层修正 |',
           f'| 机器合同 | {len(cat)}份JSON Schema；关键请求严格拒绝未知字段 |',
           f'| 正反样例 | {sum(x["expected"]=="Valid" for x in ex)}个正例、{sum(x["expected"]=="Invalid" for x in ex)}个反例，另有摘要向量 |',
           f'| 产品验证计划 | {len(tests)}个具体场景，全部NotRun；文档/治理与真实运行验证区分 |',
           '| ADR / 工作项 / 操作手册 | 25份Proposed ADR、26个WI工作项、6份可执行顺序明确的runbook |',
           f'| 历史迁移 | {legacy["unique_requirement_ids"]}个旧要求、{legacy["unique_test_ids"]}个旧测试均有文档去向 |',
           f'| 来源 | 15份原始输入逐字节归档；{len(refs)}个去重一手资料入口；按 PsiOS 原生接口与组件准入规则核验来源 |',
           '\n映射率只表示有明确去向，Schema检查只验证结构和列明的局部关系；两者都不能当作产品通过率。\n',
           '## 建议阅读顺序\n',
           '1. **确定边界：** 00阅读与权威 → 01产品架构 → 02共同合同。\n',
           '2. **开发具体模块：** 对应领域章节，同时阅读04安全、05恢复和16接口词典。\n',
           '3. **安排工程：** 14路线 → adr → work-items → acceptance；先完成可恢复的M1/M2闭环。\n',
           '4. **审查改动：** migration/findings → 旧要求映射 → 来源；不将历史源作为并列当前规范。\n',
           '## 全部分章\n',
           '| 文件 | 内容 |\n|---|---|']
    for p in docs:lines.append(f'| [{p.name}]({p.relative_to(ROOT).as_posix()}) | {md_cell(titles[p])} |')
    lines+=['\n## 关键修正\n',
        '- 明确SecurityState、Task、Workflow、Conversation、Project与外部系统各自事实权威。\n',
        '- 将审批请求、不可变决定、CAS使用账本及执行许可分开；批准不能自动代表效果已完成。\n',
        '- 将生命周期、结果知识、结果可取性、健康和补偿分开；取消/失败可以仍有未知外部效果。\n',
        '- 定义Task可靠提交、取消墓碑、Chat发送身份/单写日志、固定快照到watch和旧备份世代恢复。\n',
        '- 保留Dataset通用JSON和原有业务字段；明确TableFS双入口、缓存、POSIX子集、GC与独立退出。\n',
        '- 校正MCP代际/发现职责、A2A幂等能力边界、Portal版本与组合会话语义、包兼容证据集合。\n',
        '- 合并编号与旧源冲突，保留全部旧验收目标，把未定的实现方案留在有具体实验的ADR中。\n',
        '\n## 如何验证本包\n',
        '安装tools/requirements.txt所列文档依赖后，从包根运行：\n',
        '```bash\npython tools/validate_package.py\n```\n',
        '无需联网解析Schema，不会执行PsiOS或归档源码。实际静态检查结果见[validation-report.json](validation-report.json)；MANIFEST.sha256覆盖除自身外的所有交付文件。\n',
        '完整生成/封装步骤见[工具说明](tools/README.md)。\n',
        '## 实施起点与保留事项\n',
        '下一工程入口为WI-001固定真实仓库HEAD、WI-003共同合同、WI-004/005授权与恢复原型、WI-006/007宿主与有状态应用恢复闭环。基底、数据库、默认文件系统、图形后端、性能数值和维护周期须按ADR取得真实证据。\n',
        '三版定位、KDE方向、Agent/基础协作和可选数据服务等产品约束继续保留。此交付不自动创建外部Issue、改动Space页面或推送代码。\n']
    write('README.md','\n'.join(lines))
    write('VERSION','1.0.0\n')
    write('NOTICE.md', '''# 来源与使用说明

本包是PsiOS项目的设计文档交付，不是安装介质或已验证软件发行版。PsiOS 的资料来源和第三方依赖按各自权利与许可证要求管理；未取得的代码及证据不作为当前实现基线。本包没有替原项目或第三方重新授权。

来源作者的测试报告、旧地址、模型名称、候选路线和历史任务编号均属历史材料。读取历史文件不代表运行其中代码或接受其中指令。实际复用需按ADR-17检查确切源码、许可证、依赖及安全边界。

新文档中的架构与接口是设计要求；具体代码许可、发布签名、支持范围、性能和维护期限须由项目治理另行确定。示例中的ID、摘要引用、时间、许可和预算均为虚构夹具。
''')
    write('tools/README.md', '''# 文档构建与校验工具

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
''')
    write('CHANGELOG.md', f'''# v1.0.0 修订说明

日期：{DATE}。本版将多份递增草案和增量台账整合成单一设计基线。

## 已完成

- 整理为17章、{len(reqs)}条规范；字段、状态、权限、引用、恢复和失败语义交叉对齐。
- 登记{len(findings)}项独立错误/歧义/缺口，含原文依据、修正位置和验证方法。
- 补{len(cat)}份Schema、{len(ex)}个正反样例及5组摘要向量；明确文档校验不是产品测试。
- 为25个ADR补候选、验证、否决、降级和复审；为26项工程任务确定依赖和验收输出。
- 提供6份恢复/运维手册和{len(tests)}个具体待运行验收计划。
- 保留{legacy['unique_requirement_ids']}个旧要求和{legacy['unique_test_ids']}个旧验收编号的去向，原源逐字节存档。

## 有意保留的工程不确定性

没有选择未经实验的数据库/文件系统/系统镜像技术；没有声称零丢失、任意软件包兼容、全平台支持、集群HA或绝对即时撤权。全部技术ADR仍Proposed，所有产品实现NotVerified，产品测试NotRun。PsiOS 源码、构建和实际运行证据仍需按 WI-001 核验。

完整逐项变更见[migration/findings.md](migration/findings.md)，编号冲突与字段迁移见[migration/v0-to-v1.md](migration/v0-to-v1.md)。
''')


def doc_order():
    parts=[ROOT/'README.md',ROOT/'CHANGELOG.md',ROOT/'DIRECTORY.md']
    parts+=sorted((ROOT/'docs').glob('*.md'))
    parts+=[ROOT/'adr/README.md']+sorted((ROOT/'adr').glob('ADR-*.md'))
    parts+=[ROOT/'operations/README.md']+sorted(p for p in (ROOT/'operations').glob('*.md') if p.name!='README.md')
    parts+=[ROOT/'acceptance/README.md',ROOT/'acceptance/scenarios.md',ROOT/'acceptance/document-review.md',ROOT/'registries/requirements.md']
    parts+=[ROOT/'migration'/n for n in ['README.md','v0-to-v1.md','id-conflicts.md','findings.md',
        'legacy-requirements.md','legacy-tests.md']]
    parts+=[ROOT/'schemas/README.md',ROOT/'examples/README.md',ROOT/'sources/README.md',ROOT/'tools/README.md',ROOT/'NOTICE.md']
    return [p for p in parts if p.exists()]


def directory():
    planned={p.relative_to(ROOT).as_posix() for p in file_list()}
    planned.update(['DIRECTORY.md','PsiOS-v1-complete.md','html/index.html','validation-report.json','MANIFEST.sha256'])
    tree={}
    for name in sorted(planned):
        branch=tree
        for part in name.split('/'):
            branch=branch.setdefault(part,{})
    lines=['PsiOS-v1/']
    def walk(branch,indent):
        for name,children in branch.items():
            lines.append('  '*indent+name+('/' if children else ''))
            walk(children,indent+1)
    walk(tree,1)
    write('DIRECTORY.md','''# 完整目录与职责

这是一份可离线审查、可追溯的文档包。当前规范在docs，辅助登记表与机器合同按各目录README维护；sources/archive只作历史证据。下面列出每个交付文件，未包括运行Python临时产生的缓存。

| 目录 | 内容和使用方式 |
|---|---|
| docs/ | 00–16共17章当前规范，先读00/01/02 |
| schemas/ | 共同类型和关键JSON合同，catalog.json给精确索引 |
| examples/ | positive、negative、预期结果manifest、摘要向量 |
| registries/ | 新旧规范、修正、ADR、工作项和验收的机器登记 |
| adr/ | 25份待决技术选型，每份保留原编号与范围 |
| acceptance/ | 待执行场景与实际证据模板，所有产品结果NotRun |
| operations/ | 6种失败/恢复/退出路径的操作顺序与停止条件 |
| migration/ | 原文错误、编号冲突、旧要求/测试去向及组件依赖治理 |
| sources/ | 输入清单、外部一手资料、逐字节原始归档 |
| tools/ | 文档生成、校验、封装工具，不是产品服务 |
| html/ | 无CDN、无网络依赖的完整阅读器 |

## 每个文件

```text
'''+ '\n'.join(lines)+'\n```\n\nMANIFEST.sha256逐项记录除自身外所有交付文件的SHA-256；ZIP内部统一使用安全相对路径。\n')


def key(p):return 'doc-'+re.sub(r'[^A-Za-z0-9_-]','-',p.relative_to(ROOT).as_posix()).removesuffix('-md')


def rewrite_links(text,source,mode,included):
    def one(m):
        label,target=m.groups();u=urlsplit(target)
        if u.scheme or u.netloc or not u.path:return m.group(0)
        dest=(source.parent/unquote(u.path)).resolve()
        if not dest.is_relative_to(ROOT):return m.group(0)
        if mode=='html' and dest in included:
            # A file-level stable target avoids accidental collision of repeated
            # section headings across the full combined document.
            out='#psios-file-target-'+key(dest)
        else:
            out=('../' if mode=='html' else '')+dest.relative_to(ROOT).as_posix()
            if u.fragment:out+='#'+u.fragment
        return '['+label+']('+out+')'
    return LINK.sub(one,text)


def compile_readers():
    parts=doc_order();included=set(parts)
    combined=['# PsiOS v1 完整文档合订本\n',
        '文档版本1.0.0 · 2026-11-22。由当前分章及配套ADR、运维、验收和迁移资料自动生成；字段Schema和原始来源另见同包对应目录。勿独立修改此派生本。\n',
        '链接指向同包原始分章或证据文件，避免不同章节同名锚点混淆。\n']
    body=[];nav=[]
    pandoc=shutil.which('pandoc')
    if not pandoc:raise SystemExit('Pandoc is required to regenerate the offline HTML.')
    for p in parts:
        title=p.read_text(encoding='utf-8').splitlines()[0].lstrip('# ')
        raw=p.read_text(encoding='utf-8')
        combined+=['\n---\n',f'来源文件：`{p.relative_to(ROOT).as_posix()}`\n',rewrite_links(raw,p,'markdown',included)]
        rendered=subprocess.run([pandoc,'--from=gfm','--to=html5','--wrap=none','--id-prefix='+key(p)+'-'],
            input=rewrite_links(raw,p,'html',included),text=True,capture_output=True,check=True).stdout
        rendered=rendered.replace('href="#'+key(p)+'-psios-file-target-','href="#')
        rendered=rendered.replace('<table>','<div class="table-scroll"><table>').replace('</table>','</table></div>')
        body.append(f'<section class="document" id="{key(p)}" data-title="{html.escape(title,quote=True)}">'
                    f'<p class="source-path">{html.escape(p.relative_to(ROOT).as_posix())}</p>{rendered}</section>')
        nav.append(f'<a href="#{key(p)}">{html.escape(title)}</a>')
    write('PsiOS-v1-complete.md','\n'.join(combined)+'\n')
    style='''
    :root{color-scheme:light;--ink:#172b3a;--muted:#526777;--line:#d9e3e8;--accent:#087f8c;--paper:#fff;--nav:#102e3d}
    *{box-sizing:border-box}body{margin:0;font-family:Inter,"Noto Sans CJK SC","PingFang SC","Microsoft YaHei",system-ui,sans-serif;color:var(--ink);background:#edf3f5;line-height:1.75}
    aside{position:fixed;inset:0 auto 0 0;width:285px;background:var(--nav);color:#d6e8ee;overflow:auto;padding:26px 20px}aside h1{font-size:27px;line-height:1.2;margin:0 0 8px;color:white}aside p{font-size:12px;opacity:.85}aside a{display:block;color:#cde0e6;text-decoration:none;font-size:12px;line-height:1.5;padding:7px 9px;border-radius:4px}aside a:hover,aside a:focus{background:#1f485b;color:#fff}
    .topbar{position:sticky;top:0;z-index:3;background:#f8fbfcf5;border-bottom:1px solid var(--line);padding:14px 30px;display:flex;gap:16px;align-items:center;margin-left:285px;backdrop-filter:blur(8px)}input{min-width:180px;flex:1;max-width:680px;border:1px solid #a9c1cb;border-radius:6px;padding:11px 14px;font:inherit;font-size:14px}button{padding:10px 15px;background:#fff;color:var(--ink);border:1px solid #a9c1cb;border-radius:6px;cursor:pointer}.result-count{font-size:12px;color:var(--muted)}
    main{margin-left:285px;padding:28px 34px 80px;max-width:1560px}.document{background:var(--paper);border:1px solid var(--line);border-radius:10px;padding:32px 40px;margin-bottom:26px;box-shadow:0 5px 16px #17384506;scroll-margin-top:100px;overflow-wrap:anywhere}.source-path{font-family:ui-monospace,monospace;font-size:11px;color:var(--muted);letter-spacing:.02em;margin:0 0 12px}h1{font-size:29px;line-height:1.35;margin:0 0 24px;letter-spacing:-.02em}h2{font-size:22px;margin:35px 0 15px;padding-top:12px;border-top:1px solid var(--line)}h3{font-size:17px;margin-top:27px;color:#123e50}p{margin:12px 0}a{color:#087884;text-underline-offset:3px}strong{font-weight:650}.table-scroll{overflow:auto;margin:17px 0 22px}table{border-collapse:collapse;font-size:13px;min-width:100%;background:white}th,td{border:1px solid var(--line);padding:10px 12px;vertical-align:top;text-align:left}th{background:#eaf4f6;font-weight:650;white-space:nowrap}tr:nth-child(even) td{background:#f8fbfc}code{font-family:ui-monospace,"SFMono-Regular",Consolas,monospace;font-size:.9em;background:#eef4f6;padding:.1em .3em;border-radius:3px}pre{overflow:auto;line-height:1.65;font-size:12px;background:#102e3d;color:#e3f0f4;padding:20px;border-radius:8px}pre code{background:none;padding:0;color:inherit}blockquote{border-left:4px solid #72a9b4;padding:1px 18px;margin:18px 0;color:var(--muted);background:#f5f9fa}li{margin:6px 0}.hidden{display:none!important}:focus-visible{outline:3px solid #f4b64b;outline-offset:3px}
    @media(max-width:900px){aside{position:relative;width:100%;max-height:260px;padding:18px}aside nav{columns:2}.topbar,main{margin-left:0}.topbar{padding:10px 16px;flex-wrap:wrap}main{padding:18px 12px}.document{padding:22px 18px}h1{font-size:24px}.result-count{width:100%}}
    @media print{aside,.topbar{display:none}main{margin:0;padding:0;max-width:none}.document{box-shadow:none;border:0;border-radius:0;padding:0;break-before:page}.source-path{color:#333}pre{white-space:pre-wrap;color:#000;background:#f3f3f3}table{font-size:10px}a{color:#000}}
    '''
    script='''
    const input=document.getElementById('query'),count=document.getElementById('count');
    const sections=[...document.querySelectorAll('section.document')];
    const searchable=sections.map(s=>s.textContent.toLocaleLowerCase());
    let timer;input.addEventListener('input',()=>{clearTimeout(timer);timer=setTimeout(()=>{
      const q=input.value.trim().toLocaleLowerCase();let shown=0;
      sections.forEach((s,i)=>{const keep=!q||searchable[i].includes(q);s.classList.toggle('hidden',!keep);if(keep)shown++;});
      count.textContent=q?`${shown} / ${sections.length} 份文档匹配`:`${sections.length} 份文档 · 离线可读`;
    },120);});
    document.querySelectorAll('aside a').forEach(a=>a.addEventListener('click',()=>{
      if(input.value){input.value='';sections.forEach(s=>s.classList.remove('hidden'));count.textContent=`${sections.length} 份文档 · 离线可读`;}
    }));document.getElementById('print').addEventListener('click',()=>window.print());
    count.textContent=`${sections.length} 份文档 · 离线可读`;
    '''
    page='<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">'
    page+='<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; style-src \'unsafe-inline\'; script-src \'unsafe-inline\'; img-src data:; base-uri \'none\'; form-action \'none\'">'
    page+='<title>PsiOS v1 · 完整规范</title><style>'+style+'</style></head><body>'
    page+='<aside aria-label="文档目录"><h1>PsiOS / v1</h1><p>完整规范 · 2026-11-22<br>设计基线，待工程验证</p><nav>'+'\n'.join(nav)+'</nav></aside>'
    page+='<header class="topbar"><input id="query" type="search" aria-label="搜索全文" placeholder="搜索规范、字段、ADR 或测试编号"><span id="count" class="result-count"></span><button id="print" type="button">打印</button></header>'
    page+='<main>'+'\n'.join(body)+'</main><script>'+script+'</script></body></html>'
    write('html/index.html',page)


def seal():
    rows=[]
    for p in file_list():
        if p.name=='MANIFEST.sha256':continue
        if p.is_symlink():raise SystemExit('Symlinks are not allowed in a portable document package: '+str(p))
        rows.append(hashlib.sha256(p.read_bytes()).hexdigest()+'  '+p.relative_to(ROOT).as_posix())
    write('MANIFEST.sha256','\n'.join(rows)+'\n')
    print(json.dumps({'sealed_files':len(rows),'manifest_excludes_itself':True}))


def make_zip(target):
    subprocess.run([sys.executable,str(ROOT/'tools/validate_package.py')],check=True)
    target=target.resolve()
    if target.is_relative_to(ROOT):raise SystemExit('ZIP output must be outside the package directory.')
    target.parent.mkdir(parents=True,exist_ok=True)
    with zipfile.ZipFile(target,'w',compression=zipfile.ZIP_DEFLATED,compresslevel=9) as z:
        for p in file_list():
            info=zipfile.ZipInfo('PsiOS-v1/'+p.relative_to(ROOT).as_posix(),date_time=(2026,11,22,0,0,0))
            info.compress_type=zipfile.ZIP_DEFLATED;info.external_attr=0o100644<<16
            z.writestr(info,p.read_bytes(),compress_type=zipfile.ZIP_DEFLATED,compresslevel=9)
    with zipfile.ZipFile(target) as z:
        bad=z.testzip()
        if bad:raise SystemExit('ZIP CRC failure: '+bad)
        names=z.namelist()
        if len(names)!=len(set(names)) or any(n.startswith('/') or '..' in Path(n).parts for n in names):
            raise SystemExit('Invalid ZIP paths or duplicate entries')
    print(json.dumps({'zip':str(target),'bytes':target.stat().st_size,'entries':len(names),
                      'sha256':hashlib.sha256(target.read_bytes()).hexdigest(),'crc':'Passed'},ensure_ascii=False))


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--seal',action='store_true');ap.add_argument('--zip',type=Path)
    args=ap.parse_args()
    if args.seal:seal();return
    if args.zip:make_zip(args.zip);return
    # A prior seal necessarily becomes obsolete when derived documents change.
    (ROOT/'MANIFEST.sha256').unlink(missing_ok=True)
    normalize_tables();stats=regenerate_indexes();create_readme(stats);directory();compile_readers()
    print(json.dumps({'docs':len(list((ROOT/'docs').glob('*.md'))),'requirements':len(stats[0]),
                      'test_plans':len(stats[1]),'findings':len(stats[2]),'references':len(stats[3]),
                      'complete_markdown_bytes':(ROOT/'PsiOS-v1-complete.md').stat().st_size,
                      'offline_html_bytes':(ROOT/'html/index.html').stat().st_size}))


if __name__=='__main__':main()
