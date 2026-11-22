#!/usr/bin/env python3
"""Read-only static validation of the PsiOS v1 documentation package.

Optional --report writes a documentation report. No archived code, workflow,
external action, hardware test or product test is executed by this tool.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import re
import sys
from collections import Counter
from datetime import datetime, timezone
from decimal import Decimal
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

try:
    from jsonschema import Draft202012Validator, FormatChecker
    from referencing import Registry, Resource
except ImportError:
    raise SystemExit('Install the documentation-only dependencies in tools/requirements.txt first.')

ROOT = Path(__file__).resolve().parents[1]
BASE = 'https://schemas.psios.example/v1/'
U64_MAX = 18446744073709551615
COUNTERS = {'spec_generation','observed_generation','attempt_generation','conversation_seq','writer_epoch',
    'membership_revision','authorization_epoch','security_recovery_epoch','claim_epoch','epoch','source_seq',
    'size_bytes','capacity_bytes','minimum_bytes','required_restore_bytes','rpo_seconds','deadline_seconds',
    'max_concurrency','max_iterations','max_items','max_attempts','backoff_seconds','dimensions',
    'stop_grace_seconds','start_deadline_seconds','max_restarts','cpu_millicores','memory_max_bytes','pids_max','ephemeral_max_bytes'}
DIGEST_FIELDS = {'task-submit':'request_digest','chat-send':'content_digest','operation-plan':'plan_digest',
                 'workflow-definition':'definition_digest','recovery-set-manifest':'manifest_digest'}
REQ_DEF = re.compile(r'^(?:#{1,6}\s+|\*\*)(V1-[A-Z]+-\d{3})([^\n]*)', re.M)
LINK = re.compile(r'(?<!!)\[[^\]\n]+\]\(([^)\n]+)\)')


def strict_load(path):
    def pairs(items):
        result={}
        for k,v in items:
            if k in result: raise ValueError('Duplicate JSON key: '+k)
            result[k]=v
        return result
    def bad_constant(s): raise ValueError('Non-finite JSON value: '+s)
    value=json.loads(path.read_text(encoding='utf-8'),object_pairs_hook=pairs,parse_constant=bad_constant,parse_float=Decimal)
    def unicode_check(v):
        if isinstance(v,str): v.encode('utf-8',errors='strict')
        elif isinstance(v,list):
            for x in v: unicode_check(x)
        elif isinstance(v,dict):
            for k,x in v.items():unicode_check(k);unicode_check(x)
    unicode_check(value)
    return value


def fixture_jcs(value):
    def check(v):
        if isinstance(v,dict):
            if any(not isinstance(k,str) or not k.isascii() for k in v):
                raise ValueError('Fixture canonicalizer only accepts ASCII keys; use a full RFC8785 implementation.')
            for x in v.values():check(x)
        elif isinstance(v,list):
            for x in v:check(x)
        elif isinstance(v,bool) or v is None:pass
        elif isinstance(v,int):
            if abs(v)>2**53-1:raise ValueError('Fixture numeric value is not JCS-safe.')
        elif isinstance(v,str):v.encode('utf-8',errors='strict')
        else:raise ValueError('Fixture canonicalizer does not implement arbitrary number serialization.')
    check(value)
    return json.dumps(value,ensure_ascii=False,sort_keys=True,separators=(',',':'),allow_nan=False).encode()


def digest_errors(contract,value):
    if contract not in DIGEST_FIELDS:return []
    field=DIGEST_FIELDS[contract]
    if field not in value:return ['missing digest field']
    payload={k:v for k,v in value.items() if k!=field}
    try: computed='sha256:'+hashlib.sha256(('psios/'+contract+'/1\n').encode()+fixture_jcs(payload)).hexdigest()
    except ValueError as e:return [str(e)]
    return [] if computed==value[field] else [field+' does not match the complete business payload']


def semantic_errors(contract,value):
    errors=[]
    def walk(v,path=''):
        if isinstance(v,dict):
            for k,x in v.items():
                p=path+'/'+k
                if k in COUNTERS and isinstance(x,str) and x.isdigit() and int(x)>U64_MAX:
                    errors.append(p+' exceeds u64')
                walk(x,p)
        elif isinstance(v,list):
            for i,x in enumerate(v):walk(x,path+'/'+str(i))
    walk(value)
    if contract=='resource-envelope':
        if int(value['status']['observed_generation'])>int(value['metadata']['spec_generation']):
            errors.append('observed_generation exceeds spec_generation')
    if contract in ['operation','task','run','workflow-run','resource-envelope']:
        conditions=value.get('conditions',value.get('status',{}).get('conditions',[]))
        kinds=[x['type'] for x in conditions]
        if len(kinds)!=len(set(kinds)):errors.append('condition types must be unique in one view')
    if contract=='dataset-record':
        rec=value['record'];meta=value['metadata']
        if 'vector' in rec:
            ep=meta.get('embedding_profile',{})
            if int(ep.get('dimensions','0'))!=len(rec['vector']):errors.append('vector dimension differs from profile')
            if ep.get('source_content_version')!=meta['content_version']:errors.append('vector is stale for current content')
        if rec.get('score') is not None and meta.get('score_source_content_version')!=meta['content_version']:
            errors.append('score is stale for current content')
    if contract=='revocation-result':
        for t in value['targets']:
            if t['enforcement_status']=='Enforced' and not t['evidence_refs']:
                errors.append('Enforced target lacks evidence references')
        if value['status']=='Complete' and any(t['required'] and t['enforcement_status']!='Enforced' for t in value['targets']):
            errors.append('Complete includes a required target not Enforced')
    if contract=='task-lookup' and value['status']=='Found':
        for k in ['origin_ref','idempotency_key','request_digest']:
            if value[k]!=value['accepted'][k]:errors.append('Found receipt differs at '+k)
    if contract=='dispatch-intent':
        req=value['submit_request'];origin=req['origin_ref']
        if origin['origin_type']!='WorkflowRun':errors.append('workflow dispatch requires a WorkflowRun origin')
        for left,right in [('workflow_run_id','origin_id'),('step_key','step_key'),('attempt_generation','attempt_generation')]:
            if value[left]!=origin.get(right):errors.append('dispatch origin differs at '+right)
        if 'accepted' in value:
            for k in ['origin_ref','idempotency_key','request_digest']:
                if req[k]!=value['accepted'][k]:errors.append('dispatch acceptance differs at '+k)
        errors.extend('nested submit: '+x for x in digest_errors('task-submit',req))
    if contract=='execution-permit':
        issued=datetime.fromisoformat(value['issued_at'].replace('Z','+00:00'))
        expiry=datetime.fromisoformat(value['not_after'].replace('Z','+00:00'))
        if expiry<=issued:errors.append('permit validity interval is empty or reversed')
    if contract=='application-manifest':
        claims=[x['capability'] for x in value['compatibility_claims']]
        if len(claims)!=len(set(claims)):errors.append('duplicate compatibility capability evidence')
    if contract=='workflow-definition':
        nodes=value['nodes'];ids=[n['step_id'] for n in nodes];known=set(ids)
        if len(known)!=len(ids):errors.append('duplicate step IDs')
        graph={n['step_id']:n['depends_on'] for n in nodes}
        for n in nodes:
            refs=list(n['depends_on'])
            if n['kind']=='Parallel':refs+=n['children']
            if n['kind']=='Join':refs+=n['predecessors']
            if n['kind']=='Condition':
                refs+=n['default_activate']
                for b in n['branches']:refs+=b['activate']
            for x in refs:
                if x not in known:errors.append('dangling step reference '+x)
            if n['kind']=='Join' and n['strategy']=='AnySuccess' and n['loser_policy']!='ReconcileBeforeRelease':
                errors.append('AnySuccess requires loser reconciliation policy')
        visiting=set();done=set()
        def visit(k):
            if k in visiting:raise ValueError('cyclic base dependency graph')
            if k in done:return
            visiting.add(k)
            for dep in graph.get(k,[]):
                if dep in graph:visit(dep)
            visiting.remove(k);done.add(k)
        try:
            for k in graph:visit(k)
        except ValueError as e:errors.append(str(e))
    return errors


def requirements():
    rows=[]
    for p in sorted((ROOT/'docs').glob('*.md')):
        text=p.read_text(encoding='utf-8')
        matches=list(REQ_DEF.finditer(text))
        for i,m in enumerate(matches):
            end=matches[i+1].start() if i+1<len(matches) else len(text)
            block=text[m.start():end].strip()
            # Text remains in the source; registry stores the requirement paragraph
            # and location, not a second independently editable normative body.
            first=block.split('\n\n',1)[0]
            rows.append({'id':m.group(1),'document':p.relative_to(ROOT).as_posix(),
                         'line':text.count('\n',0,m.start())+1,
                         'heading':re.sub(r'^[#*\s]+|[*]+$','',m.group(0)),
                         'summary':first})
    return rows


class ReaderInspector(HTMLParser):
    def __init__(self):
        super().__init__();self.ids=[];self.anchors=[];self.remote_resources=[];self.sections=0
    def handle_starttag(self,tag,attrs):
        a=dict(attrs)
        if 'id' in a:self.ids.append(a['id'])
        if tag=='a' and a.get('href','').startswith('#'):self.anchors.append(unquote(a['href'][1:]))
        if tag in ['script','img','link','iframe','video','audio']:
            u=a.get('src',a.get('href',''))
            if u.startswith(('http:','https:','//')):self.remote_resources.append(u)
        if tag=='section' and a.get('class')=='document':self.sections+=1


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--contracts-only',action='store_true')
    parser.add_argument('--report',type=Path)
    args=parser.parse_args()
    failures=[];checks=Counter();results=[]
    def fail(scope,message):failures.append({'scope':scope,'message':message})
    schemas={}
    for p in sorted((ROOT/'schemas').glob('*.schema.json')):
        try:
            s=strict_load(p);Draft202012Validator.check_schema(s);schemas[s['$id']]=s;checks['schema_definitions']+=1
        except Exception as e:fail(str(p.relative_to(ROOT)),str(e))
    def no_network(uri):raise ValueError('Unregistered schema reference; network resolution is disabled: '+uri)
    registry=Registry(retrieve=no_network)
    for uri,s in schemas.items():registry=registry.with_resource(uri,Resource.from_contents(s))
    for uri,s in schemas.items():
        def refs(v):
            if isinstance(v,dict):
                if '$ref' in v:
                    target=v['$ref'].split('#',1)[0]
                    if target and target not in schemas:fail(uri,'Missing local reference '+target)
                    checks['schema_references']+=1
                for x in v.values():refs(x)
            elif isinstance(v,list):
                for x in v:refs(x)
        refs(s)
    manifest=strict_load(ROOT/'examples/manifest.json')
    for item in manifest['examples']:
        path=ROOT/'examples'/item['path'];contract=item['schema'].removesuffix('.schema.json')
        try:
            value=strict_load(path)
            validator=Draft202012Validator(schemas[BASE+item['schema']],registry=registry,format_checker=FormatChecker())
            structural=[e.message for e in validator.iter_errors(value)]
            semantic=[] if structural else semantic_errors(contract,value)
            digests=[] if structural else digest_errors(contract,value)
            if item['expected']=='Valid':ok=not(structural or semantic or digests)
            elif item['validation']=='Schema':ok=bool(structural)
            elif item['validation']=='Semantic':ok=not structural and bool(semantic)
            elif item['validation']=='Digest':ok=not(structural or semantic) and bool(digests)
            else:ok=False
            if not ok:fail(item['id'],'Unexpected example outcome: '+str({'schema':structural[:3],'semantic':semantic,'digest':digests}))
            results.append({'id':item['id'],'expected':item['expected'],'rejection_layer':item['validation'],
                            'result':'Passed' if ok else 'Failed','scope':'DocumentationFixtureOnly'})
            checks['positive_examples' if item['expected']=='Valid' else 'negative_examples']+=1
        except Exception as e:fail(item['id'],'Validation failed to execute: '+repr(e))
    vectors=strict_load(ROOT/'examples/digest-vectors.json')
    for vector in vectors['vectors']:
        try:
            data=fixture_jcs(vector['payload'])
            computed='sha256:'+hashlib.sha256(vector['domain_prefix'].encode()+data).hexdigest()
            if data.decode()!=vector['canonical_utf8'] or computed!=vector['expected_digest']:
                fail('digest-vector:'+vector['contract'],'Canonical bytes/digest mismatch')
            checks['digest_vectors']+=1
        except Exception as e:fail('digest-vector:'+vector['contract'],str(e))
    if not args.contracts_only:
        # Historical archives are preserved verbatim and intentionally not linted
        # as current v1 prose. Their hashes are checked below.
        for p in ROOT.rglob('*.json'):
            if 'archive' in p.relative_to(ROOT).parts or p.name=='validation-report.json':continue
            try:strict_load(p);checks['strict_json_files']+=1
            except Exception as e:fail(p.relative_to(ROOT).as_posix(),str(e))
        reqs=requirements();ids=[x['id'] for x in reqs];reqset=set(ids)
        for k,n in Counter(ids).items():
            if n>1:fail('requirements','Duplicate definition '+k)
        checks['normative_requirements']=len(reqs)
        expected=strict_load(ROOT/'registries/requirements.json')
        actual_ids={x['id'] for x in expected['requirements']}
        if reqset!=actual_ids:fail('requirements','Generated registry and docs differ')
        for p in ROOT.rglob('*.md'):
            relative=p.relative_to(ROOT)
            if 'archive' in relative.parts:continue
            txt=p.read_text(encoding='utf-8')
            for target in LINK.findall(txt):
                target=target.strip().split(' "',1)[0].strip('<>')
                u=urlsplit(target)
                if u.scheme or u.netloc or not u.path:continue
                dest=(p.parent/unquote(u.path)).resolve()
                if not dest.is_relative_to(ROOT):fail(relative.as_posix(),'Link escapes package: '+target)
                elif not dest.exists() and not (args.report and dest==args.report.resolve()):
                    fail(relative.as_posix(),'Missing linked file: '+target)
                checks['local_markdown_links']+=1
        tests=strict_load(ROOT/'registries/test-cases.json');test_ids=set()
        for t in tests:
            if t['id'] in test_ids:fail('tests','Duplicate '+t['id'])
            test_ids.add(t['id'])
            for field in ['preconditions','actions_or_fault','expected','required_evidence']:
                if not t.get(field):fail(t['id'],'Empty test field '+field)
            if t['status']!='NotRun' or t['implementation_status']!='NotVerified':
                fail(t['id'],'Documentation baseline must not claim product execution')
            for q in t['requirement_ids']:
                if q not in reqset:fail(t['id'],'Missing requirement '+q)
            if not(ROOT/t['source_document']).is_file():fail(t['id'],'Missing source document')
        checks['product_test_plans_not_run']=len(tests)
        coverage={}
        for t in tests:
            for q in t['requirement_ids']:coverage.setdefault(q,set()).add(t['id'])
        for q in expected['requirements']:
            if set(q['planned_test_ids'])!=coverage.get(q['id'],set()):fail(q['id'],'Generated test mapping is stale')
            if not coverage.get(q['id']):fail(q['id'],'No validation plan is mapped')
        checks['requirements_with_validation_plan']=sum(bool(coverage.get(q)) for q in reqset)
        declared_paths={str((ROOT/'examples'/x['path']).resolve()) for x in manifest['examples']}
        actual_paths={str(x.resolve()) for folder in ['positive','negative'] for x in (ROOT/'examples'/folder).glob('*.json')}
        if declared_paths!=actual_paths:fail('examples','Manifest differs from actual positive/negative fixture set')
        for row in strict_load(ROOT/'registries/adrs.json'):
            if row['status']!='Proposed' or row['selected_option'] is not None:fail(row['adr_id'],'Fabricated ADR acceptance')
            if not(ROOT/row['path']).is_file():fail(row['adr_id'],'Missing ADR document')
            checks['proposed_adrs']+=1
        witems=strict_load(ROOT/'registries/work-items.json')
        if isinstance(witems,dict):witems=witems['work_items']
        wids={x['id'] for x in witems}
        if len(wids)!=len(witems):fail('work-items','Duplicate work item ID')
        for w in witems:
            for dep in w.get('dependencies',[]):
                if dep not in wids:fail(w['id'],'Missing dependency '+dep)
            checks['work_items']+=1
        findings=strict_load(ROOT/'registries/findings.json')
        if isinstance(findings,dict):findings=findings['findings']
        fids=set()
        for f in findings:
            if f['id'] in fids:fail('findings','Duplicate '+f['id'])
            fids.add(f['id'])
            target=f['target'].split(' §',1)[0].split('；',1)[0].split('、',1)[0]
            if not(ROOT/target).exists():fail(f['id'],'Missing target '+target)
            if f['implementation_status']!='NotVerified':fail(f['id'],'Unverified code represented as verified')
        checks['document_findings']=len(findings)
        legacy=strict_load(ROOT/'registries/legacy-requirements.json')
        for category in ['requirements','tests']:
            for row in legacy[category]:
                if row['migration']['status']!='Mapped':fail(row['legacy_id'],'Unmapped legacy entry')
                for target in row['migration']['targets']:
                    if not(ROOT/target['document']).exists():fail(row['legacy_id'],'Missing migration target')
                    if target.get('requirement_id') and target['requirement_id'] not in reqset:
                        fail(row['legacy_id'],'Missing migrated requirement '+target['requirement_id'])
                    checks['legacy_mapping_targets']+=1
                for definition in row['source_definitions']:
                    lines=(ROOT/definition['source_file']).read_text(encoding='utf-8').splitlines()
                    actual='\n'.join(lines[definition['line_start']-1:definition['line_end']])
                    if actual!=definition['original_text']:fail(row['legacy_id'],'Original excerpt differs from preserved archive')
                    checks['legacy_definition_slices']+=1
        checks['legacy_requirements']=len(legacy['requirements']);checks['legacy_tests']=len(legacy['tests'])
        reader=ReaderInspector();reader.feed((ROOT/'html/index.html').read_text(encoding='utf-8'))
        missing=sorted(set(reader.anchors)-set(reader.ids))
        if missing:fail('html/index.html','Broken internal anchors: '+str(missing[:10])+f' (total {len(missing)})')
        if len(reader.ids)!=len(set(reader.ids)):fail('html/index.html','Duplicate HTML IDs')
        if reader.remote_resources:fail('html/index.html','Offline reader loads remote resources')
        checks['html_internal_anchors']=len(reader.anchors);checks['html_document_sections']=reader.sections
        inventory=strict_load(ROOT/'sources/source-inventory.json')
        entries=inventory if isinstance(inventory,list) else inventory.get('sources',inventory.get('files',[]))
        for entry in entries:
            rel=entry.get('archive_path') or entry.get('path') or entry.get('archived_path')
            if not rel:fail('source-inventory','Missing archive path in '+str(entry.get('source_id')));continue
            p=ROOT/rel
            if not p.is_file():fail(rel,'Archived input missing');continue
            blob=p.read_bytes()
            if hashlib.sha256(blob).hexdigest()!=entry['sha256']:fail(rel,'Archived input digest mismatch')
            expected_size=entry.get('size_bytes',entry.get('bytes'))
            if expected_size is not None and len(blob)!=int(expected_size):fail(rel,'Archived input size mismatch')
            checks['archived_sources']=checks['archived_sources']+1
        sums=ROOT/'MANIFEST.sha256'
        if sums.exists():
            for line in sums.read_text().splitlines():
                digest,rel=line.split('  ',1);p=ROOT/rel
                if not p.is_file() or hashlib.sha256(p.read_bytes()).hexdigest()!=digest:fail('MANIFEST.sha256','Mismatch '+rel)
                checks['manifest_files']+=1
    report={'document_version':'1.0.0','checked_at':datetime.now(timezone.utc).isoformat(),
            'scope':'DocumentationPackageStaticValidationOnly','result':'Passed' if not failures else 'Failed',
            'counts':dict(checks),'failures':failures,'example_results':results,
            'product_implementation_status':'NotVerified','product_test_status':'NotRun',
            'not_proven':['Authorization and signature authenticity','Database transactions and distributed races',
                'Hardware/process isolation and actual resource fencing','Power-loss durability and recovery',
                'External side effects or protocol interoperability','Performance and supported release profiles',
                'Full RFC8785 cross-language conformance (fixture subset only)']}
    if args.report:
        args.report.parent.mkdir(parents=True,exist_ok=True)
        args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
    print(json.dumps({k:report[k] for k in ['scope','result','counts','failures','product_test_status']},ensure_ascii=False,indent=2))
    return 0 if not failures else 1


if __name__=='__main__':sys.exit(main())
