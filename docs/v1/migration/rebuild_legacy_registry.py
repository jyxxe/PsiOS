#!/usr/bin/env python3
"""Rebuild source-backed legacy ID registers. Reads archived text, never executes it.

Run from any directory: python migration/rebuild_legacy_registry.py
The explicit migration map is reviewed documentation, not an inferred code test.
"""
from __future__ import annotations

import collections
import hashlib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INVENTORY = json.loads((ROOT / "sources/source-inventory.json").read_text())
REQ_FAMILIES = "BASE FND API SEC UPD NET DEV APP PKG CU GUI STO DAT TFS CLU AGT WFL CHAT PRJ WF OPS OBS REL UX EXT GOV EVT".split()
TEST_FAMILIES = "XAT WF-AT ACC-CHAT ACC-CLU ACC-DAT ACC-EVT ACC-OBS ACC-STO ACC-TFS ACC-WFL AT-DEV AT-FND AT-NET AT-REL AT-UPD".split()
ALL_FAMILIES = sorted(REQ_FAMILIES + TEST_FAMILIES + ["ADR", "P0", "P1", "P2", "R"], key=len, reverse=True)
PAT = re.compile(r"(?<![A-Za-z0-9-])(" + "|".join(ALL_FAMILIES) + r")-(T?)(\d{1,3})(?![A-Za-z0-9])")
SHORT = set("BASE FND UPD NET DEV REL ADR P0 P1 P2 R".split()) | set(TEST_FAMILIES)


def normalize(family, marker, number):
    return f"{family}-{marker}{int(number):0{2 if marker or family in SHORT else 3}d}"


def mentions(line):
    out = []
    for m in PAT.finditer(line):
        fam, mark, num = m.groups()
        out.append((normalize(fam, mark, num), m.start(), m.end(), None))
        tail = line[m.end():]
        # A range may repeat its prefix or abbreviate it, e.g. TFS-001～TFS-016.
        rr = re.match(r"\s*(?:～|~|–|—|至)\s*(?:(" + re.escape(fam) + r")-" + re.escape(mark) + r")?(\d{1,3})(?!\d)", tail)
        end = m.end()
        if rr:
            upper = int(rr.group(2)); lower = int(num)
            if lower <= upper <= 999:
                end += rr.end()
                out.extend((normalize(fam, mark, n), m.start(), end, line[m.start():end]) for n in range(lower, upper + 1))
        # Expand only adjacent, unambiguous slash shorthand: XAT-30～34/36～37.
        while True:
            sm = re.match(r"/\s*(\d{1,3})(?:\s*(?:～|~|–|—|至)\s*(\d{1,3}))?(?!\d)", line[end:])
            if not sm:
                break
            lo = int(sm.group(1)); hi = int(sm.group(2) or lo)
            if not (0 < lo <= hi <= 999):
                break
            end += sm.end()
            out.extend((normalize(fam, mark, n), m.start(), end, line[m.start():end]) for n in range(lo, hi + 1))
    unique = {}
    for item in out:
        key = item[0]
        if key not in unique or unique[key][3] is None and item[3] is not None:
            unique[key] = item
    return list(unique.values())


def kind(legacy_id):
    fam, num = legacy_id.rsplit("-", 1)
    if fam in TEST_FAMILIES or num.startswith("T"):
        return "test"
    if fam in REQ_FAMILIES:
        return "requirement"
    return {"ADR": "decision", "R": "risk", "P0": "work_item", "P1": "work_item", "P2": "work_item"}[fam]


def leading_id(line):
    s = re.sub(r"^(?:#{1,6}\s+|[-*]\s+|\d+\.\s+)", "", line.strip())
    if s.startswith("|"):
        s = s[1:].lstrip()
    s = s.lstrip("*` ")
    m = PAT.match(s)
    return normalize(*m.groups()) if m else None


def is_definition(line, legacy_id):
    if leading_id(line) == legacy_id:
        return True
    if kind(legacy_id) == "test" and re.search(r"(?:验收包括|至少验证|验收：|建议最低验收|额外验收：|最低验收)", line):
        return not re.search(r"交叉映射|新增位置|扩展原|详见", line)
    return False


def clean(s):
    s = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", s)
    return re.sub(r"[*`#]", "", s).strip()


def extract():
    records = {}
    for source in INVENTORY["sources"]:
        if source["kind"] not in ("ProjectUpload", "SpaceExport"):
            continue
        p = ROOT / source["archive_path"]
        assert hashlib.sha256(p.read_bytes()).hexdigest() == source["sha256"], p
        lines = p.read_text().splitlines()
        headings = []
        for i, line in enumerate(lines):
            h = re.match(r"^(#{1,6})\s+(.*)", line)
            if h:
                level = len(h.group(1)); headings = [x for x in headings if x[0] < level]
                headings.append((level, h.group(2)))
            for legacy_id, start, end, expanded in mentions(line):
                rec = records.setdefault(legacy_id, {"legacy_id": legacy_id, "category": kind(legacy_id), "family": legacy_id.rsplit("-", 1)[0], "source_definitions": [], "source_references": []})
                definition = is_definition(line, legacy_id)
                stop = i + 1
                # Keep the complete block of heading/bold requirements, including tables.
                if definition and kind(legacy_id) in ("requirement", "risk") and not line.lstrip().startswith("|"):
                    while stop < len(lines):
                        nextline = lines[stop]
                        if re.match(r"^#{1,6}\s", nextline) or leading_id(nextline) is not None or nextline.strip() == "---":
                            break
                        stop += 1
                    while stop > i + 1 and not lines[stop - 1].strip():
                        stop -= 1
                occurrence = {"source_id": source["source_id"], "source_file": source["archive_path"], "section": " › ".join(t for _, t in headings), "line_start": i + 1, "line_end": stop if definition else i + 1, "original_id_text": line[start:end], "original_text": "\n".join(lines[i:stop]) if definition else line, "expanded_from": expanded}
                if definition and kind(legacy_id) == "test" and not line.lstrip().startswith("|"):
                    after = [q.start() for q in PAT.finditer(line) if q.start() > start and kind(normalize(*q.groups())) == "test"]
                    a = start - 1 if start > 0 and line[start - 1] == "`" else start
                    b = min(after) if after else len(line)
                    if b < len(line) and line[b - 1] == "`":
                        b -= 1
                    occurrence["selected_excerpt"] = line[a:b].rstrip("；; ，,")
                rec["source_definitions" if definition else "source_references"].append(occurrence)
    priority = {"SRC-SPACE-V06": 0, "SRC-SPACE-RISK": 1, "SRC-IMPORT-V06": 2, "SRC-IMPORT-RISK": 3, "SRC-IMPORT-ROADMAP": 4, "SRC-LEGACY-V05": 5, "SRC-LEGACY-V04": 6, "SRC-LEGACY-V03": 7, "SRC-LEGACY-V02": 8, "SRC-LEGACY-V01": 9}
    for rec in records.values():
        defs = rec["source_definitions"]
        candidates = defs or rec["source_references"]
        rec["canonical_source"] = sorted(candidates, key=lambda x: (priority.get(x["source_id"], 99), x["line_start"]))[0]
        rec["definition_status"] = "Defined" if defs else "ReferenceOnly"
        rec["historical_definition_variant_count"] = len({x["original_text"] for x in defs})
    return records


DOCS = {}
NEW_REQ = {}
for p in sorted((ROOT / "docs").glob("*.md")):
    DOCS[p.name[:2]] = "docs/" + p.name
    heading = ""
    for line in p.read_text().splitlines():
        if re.match(r"^#{1,6}\s", line):
            heading = clean(line)
        if re.match(r"^(?:\*\*|#{1,6}\s+)V1-[A-Z]+-\d{3}", line):
            m = re.search(r"V1-[A-Z]+-\d{3}", line)
            NEW_REQ[m.group()] = {"document": "docs/" + p.name, "requirement_id": m.group(), "section": heading, "label": clean(line)[:180]}

M = {}


def put(old, new, disposition="consolidated_into", rationale=None):
    ids = new.split() if isinstance(new, str) else list(new)
    M[old] = {"status": "Mapped", "disposition": disposition, "targets": [dict(NEW_REQ[n]) for n in ids], "rationale": rationale or "按下列 v1 明确条款合并并补全；旧版的实现候选、保证范围和验收状态以 v1 修正为准。"}


def family(fam, mapping, disposition="consolidated_into"):
    for num, new in mapping.items():
        put(normalize(fam, "", num), new, disposition)


def section(old, doc, title, rationale, disposition="retained_context", new=""):
    put(old, new, disposition, rationale)
    M[old]["targets"].append({"document": DOCS[doc], "section": title})


def maps():
    family("BASE", {1:"V1-BASE-001 V1-ARCH-001",2:"V1-BASE-001",3:"V1-BASE-001",4:"V1-BASE-002",5:"V1-BASE-001 V1-GUI-001",6:"V1-BASE-003 V1-ARCH-004",7:"V1-BASE-003 V1-DAT-004",8:"V1-DAT-001 V1-DAT-002 V1-DAT-003",9:"V1-CLU-001 V1-CLU-010",10:"V1-ARCH-005 V1-GOV-007",13:"V1-STO-001 V1-TFS-001",14:"V1-TFS-007 V1-DAT-004",15:"V1-APP-008 V1-APP-009",16:"V1-GUI-001 V1-GUI-002",17:"V1-CHAT-001 V1-CHAT-002 V1-CHAT-009",18:"V1-BASE-004 V1-ARCH-002",19:"V1-CHAT-009 V1-CHAT-010 V1-AGT-007",20:"V1-ARCH-003 V1-ARCH-004 V1-CHAT-014"})
    section("BASE-11","00","5. 尚未决定的事项","OS、默认FS、OCI与更新后端继续作为待ADR验证的候选，归档不构成技术选型批准。",new="V1-GOV-005 V1-FND-001")
    section("BASE-12","01","1. 产品目标与固定约束","长期建设与证据阶段保留；文档版本不宣告日历工期或实现成熟度。",new="V1-GOV-002 V1-GOV-005")
    family("API",{1:"V1-API-001",2:"V1-SEC-002 V1-SEC-003 V1-API-017",3:"V1-ARCH-002 V1-API-009",4:"V1-API-003 V1-API-004",5:"V1-API-001",6:"V1-API-002 V1-STO-004 V1-APP-006",7:"V1-API-009 V1-API-010 V1-API-012 V1-API-013 V1-API-018",8:"V1-SEC-005 V1-SEC-006 V1-SEC-007 V1-SEC-009",9:"V1-API-010 V1-API-011",10:"V1-API-009 V1-TSK-007",12:"V1-API-014",13:"V1-API-005 V1-CLU-001",14:"V1-FND-005 V1-OBS-001",15:"V1-API-005",16:"V1-ARCH-001 V1-ARCH-004",17:"V1-API-008",18:"V1-REL-006",19:"V1-API-008 V1-API-018",20:"V1-DEVX-001 V1-API-016"})
    section("API-005","02","1. 名词和单一事实来源","StoragePool及Dataset记录身份的正式用语保留。","consolidated_into","V1-API-001")
    section("API-011","02","8. 错误约定","错误码、重试条件、结果未知与恢复原因在统一错误表展开。","consolidated_into","V1-API-009 V1-API-013")
    family("FND",{1:"V1-FND-001 V1-REL-005",2:"V1-FND-002 V1-ARCH-001",3:"V1-FND-003 V1-REC-002 V1-REC-008",4:"V1-FND-004 V1-REC-018",5:"V1-FND-005 V1-UX-001 V1-SEC-016",6:"V1-FND-006 V1-API-017",7:"V1-FND-007"})
    family("UPD",{1:"V1-UPD-001",2:"V1-UPD-002",3:"V1-UPD-003",4:"V1-REC-008 V1-REC-018 V1-UPD-003",5:"V1-UPD-001 V1-UPD-002 V1-FND-005 V1-REC-016",6:"V1-UPD-004 V1-REL-003"})
    family("SEC",{1:"V1-SEC-002 V1-SEC-003",2:"V1-SEC-003 V1-SEC-014 V1-SEC-016",3:"V1-SEC-002 V1-SEC-004 V1-SEC-005 V1-SEC-007 V1-SEC-008",4:"V1-SEC-005 V1-SEC-006 V1-SEC-007 V1-SEC-009",5:"V1-SEC-001 V1-SEC-010",6:"V1-SEC-010",7:"V1-SEC-003 V1-SEC-011 V1-API-017",8:"V1-SEC-017 V1-SEC-018 V1-SEC-019",9:"V1-SEC-003 V1-SEC-013 V1-SEC-016 V1-SEC-021",10:"V1-SEC-020 V1-SEC-021",11:"V1-SEC-001 V1-SEC-005 V1-SEC-010 V1-SEC-012 V1-SEC-015"})
    family("APP",{1:"V1-APP-001 V1-APP-002",2:"V1-APP-002",3:"V1-APP-003",4:"V1-APP-003",5:"V1-APP-004",6:"V1-APP-005",7:"V1-APP-006",8:"V1-APP-006 V1-APP-007",9:"V1-APP-007"})
    family("PKG",{1:"V1-APP-008",2:"V1-APP-009",3:"V1-APP-009 V1-APP-010",4:"V1-APP-010",5:"V1-APP-009 V1-APP-010",6:"V1-APP-001 V1-APP-008",7:"V1-APP-008 V1-APP-011",8:"V1-APP-011",9:"V1-APP-007 V1-APP-010",10:"V1-APP-007 V1-APP-008 V1-APP-010"})
    family("CU",{1:"V1-GUI-001 V1-GUI-002",2:"V1-GUI-002",3:"V1-GUI-001 V1-GUI-002",4:"V1-GUI-003",5:"V1-GUI-004",6:"V1-GUI-005 V1-GUI-006",7:"V1-GUI-009",8:"V1-GUI-005 V1-GUI-009"})
    family("GUI",{1:"V1-GUI-001",2:"V1-GUI-002",3:"V1-GUI-001 V1-GUI-002",4:"V1-GUI-002",5:"V1-GUI-002 V1-GUI-007",6:"V1-GUI-007",7:"V1-GUI-001 V1-GUI-002",8:"V1-GUI-003",9:"V1-GUI-004",10:"V1-GUI-004 V1-GUI-005",11:"V1-GUI-005 V1-GUI-010",12:"V1-GUI-007",13:"V1-GUI-009",14:"V1-GUI-008",15:"V1-GUI-010"})
    family("STO",{1:"V1-STO-002",2:"V1-STO-003",3:"V1-STO-004",4:"V1-STO-004",5:"V1-STO-005",6:"V1-STO-006 V1-STO-007",7:"V1-STO-006 V1-STO-008",8:"V1-STO-008",9:"V1-STO-009",10:"V1-STO-009"})
    family("DAT",{1:"V1-DAT-001 V1-DAT-003",2:"V1-DAT-002 V1-DAT-003 V1-DAT-005",3:"V1-DAT-002 V1-DAT-004",4:"V1-DAT-004 V1-DAT-005 V1-REC-023",5:"V1-DAT-004 V1-DAT-005",6:"V1-DAT-006"})
    family("TFS",{1:"V1-TFS-001",2:"V1-TFS-002 V1-STO-002",3:"V1-TFS-001 V1-TFS-002",4:"V1-TFS-003 V1-STO-006",5:"V1-TFS-003 V1-STO-006",6:"V1-TFS-004 V1-TFS-005",7:"V1-TFS-008 V1-TFS-009",8:"V1-TFS-001 V1-TFS-002",9:"V1-TFS-007",10:"V1-TFS-007 V1-DAT-005",11:"V1-TFS-007 V1-DAT-005 V1-REC-023",12:"V1-TFS-005",13:"V1-TFS-006",14:"V1-STO-005 V1-TFS-008",15:"V1-STO-005 V1-TFS-008",16:"V1-TFS-006 V1-TFS-008"})
    family("AGT",{1:"V1-TSK-001 V1-AGT-001 V1-AGT-002",2:"V1-TSK-002 V1-TSK-003 V1-TSK-007",3:"V1-TSK-008 V1-TSK-005 V1-API-010 V1-REC-010",4:"V1-TSK-006 V1-TSK-016 V1-AGT-002 V1-CLU-005",5:"V1-AGT-003 V1-AGT-005 V1-API-016 V1-API-018",6:"V1-AGT-004",7:"V1-AGT-008",8:"V1-AGT-009 V1-AGT-010 V1-AGT-011 V1-AGT-012",9:"V1-AGT-008",10:"V1-AGT-006 V1-AGT-007 V1-TSK-014",11:"V1-AGT-006 V1-AGT-007 V1-ARCH-004"})
    section("AGT-008","09","9.6 各协议负责什么","MCP/A2A/AG-UI/OpenAPI职责分别保留；现代和旧代际不能混为同一握手/认证。","superseded_by","V1-AGT-009 V1-AGT-010 V1-AGT-011 V1-AGT-012")
    section("AGT-009","09","9.9 最低能力与验收","至少一个真实第三方与原生Agent同类任务验收仍保留。","consolidated_into","V1-AGT-008")
    family("NET",{1:"V1-NET-001",2:"V1-NET-001 V1-NET-003",3:"V1-NET-002",4:"V1-NET-003 V1-NET-004"})
    family("DEV",{1:"V1-DEV-001",2:"V1-DEV-002",3:"V1-DEV-002 V1-DEV-003",4:"V1-UX-002 V1-GUI-002 V1-GUI-007 V1-SEC-003",5:"V1-DEV-004 V1-UPD-005"})
    family("CLU",{1:"V1-CLU-001",2:"V1-CLU-002",3:"V1-CLU-003",4:"V1-CLU-010",5:"V1-CLU-004 V1-CLU-005",6:"V1-CLU-005 V1-CLU-006",7:"V1-CLU-007 V1-FND-006",8:"V1-ARCH-002 V1-CLU-010",9:"V1-CLU-011",10:"V1-CLU-011 V1-NET-004 V1-STO-005",11:"V1-CLU-012",12:"V1-CLU-008 V1-CLU-009 V1-REC-013 V1-REC-014"})
    family("EVT",{1:"V1-TSK-002 V1-TSK-003",2:"V1-TSK-004 V1-TSK-010",3:"V1-TSK-011",4:"V1-TSK-011 V1-TSK-012 V1-TSK-013",5:"V1-TSK-012 V1-TSK-013",6:"V1-TSK-006 V1-TSK-016",7:"V1-TSK-015"})
    family("WFL",{1:"V1-WFL-001",2:"V1-BASE-004 V1-TSK-001 V1-WFL-001",3:"V1-WFL-011",4:"V1-WFL-002 V1-WFL-003 V1-WFL-010",5:"V1-WFL-002 V1-WFL-003 V1-WFL-005 V1-WFL-006",6:"V1-WFL-004 V1-TSK-005 V1-REC-010 V1-REC-011",7:"V1-SEC-004 V1-TSK-014 V1-CHAT-009",8:"V1-WFL-007 V1-WFL-014 V1-SEC-005 V1-SEC-007",9:"V1-WFL-008 V1-WFL-009",10:"V1-WFL-012 V1-WFL-013",11:"V1-WFL-014",12:"V1-WFL-001",13:"V1-WFL-010 V1-TSK-004 V1-TSK-010 V1-REC-009",14:"V1-ARCH-003 V1-ARCH-004"})
    section("WFL-014","11","11.11 验收和可选部署","最小Task/Workflow/Chat能力与Project/Studio可选部署分开；后端尚未ADR批准。","consolidated_into","V1-ARCH-003 V1-ARCH-004")
    family("CHAT",{1:"V1-CHAT-001 V1-CHAT-002",2:"V1-CHAT-002 V1-CHAT-003",3:"V1-CHAT-002",4:"V1-CHAT-004",5:"V1-CHAT-004 V1-CHAT-005 V1-CHAT-006 V1-CHAT-012",6:"V1-CHAT-008",7:"V1-CHAT-009",8:"V1-CHAT-009 V1-CHAT-010",9:"V1-CHAT-010",10:"V1-SEC-004 V1-CHAT-003 V1-CHAT-009",11:"V1-CHAT-009 V1-BASE-004",12:"V1-CHAT-011 V1-CHAT-012",13:"V1-CHAT-011 V1-CHAT-012 V1-REC-023",14:"V1-CHAT-013 V1-AGT-008 V1-AGT-011",15:"V1-CHAT-006 V1-CHAT-007 V1-API-014",16:"V1-CHAT-014"})
    family("OBS",{1:"V1-OBS-001",2:"V1-OBS-002",3:"V1-OBS-003 V1-SEC-020",4:"V1-OBS-004",5:"V1-OBS-002 V1-OBS-005",6:"V1-OPS-004 V1-ARCH-003 V1-REL-005"})
    family("OPS",{n:f"V1-OPS-{n:03d}" for n in range(1,6)})
    family("REL",{1:"V1-REL-001 V1-REL-002",2:"V1-REL-002 V1-REL-003",3:"V1-API-008 V1-REL-006",4:"V1-DEVX-001",5:"V1-REL-004 V1-REL-005 V1-REL-006 V1-OBS-004"})
    family("UX",{1:"V1-UX-001",2:"V1-UX-001 V1-SEC-013 V1-CLU-002",3:"V1-UX-001 V1-API-009",4:"V1-UX-002 V1-GUI-009 V1-SEC-019",5:"V1-API-006 V1-TSK-015 V1-FND-006 V1-UX-002",6:"V1-UX-002",7:"V1-UX-002 V1-SEC-017",8:"V1-SEC-014 V1-SEC-016 V1-NET-003 V1-GUI-009"})
    family("GOV",{1:"V1-GOV-006",2:"V1-GOV-007 V1-ARCH-005",3:"V1-GOV-001 V1-GOV-002 V1-GOV-005 V1-REL-005",4:"V1-GOV-008"})
    for n in range(1,11):
        section(f"EXT-{n:03d}","13","7. 高级能力进入条件",f"保留原EXT-{n:03d}对应行的能力、扩展点与启用前门槛。retained_context不表示删除或可不满足安全要求；未启用不宣称已支持。")
    family("R",{1:"V1-REC-004 V1-REC-005 V1-REC-006 V1-REC-007",2:"V1-TSK-003 V1-TSK-004 V1-TSK-005 V1-TSK-006 V1-WFL-004",3:"V1-CHAT-004 V1-CHAT-006 V1-CHAT-007 V1-REC-013",4:"V1-SEC-005 V1-SEC-010 V1-SEC-012 V1-AGT-005",5:"V1-SEC-017 V1-SEC-018 V1-SEC-019",6:"V1-AGT-009 V1-AGT-010 V1-AGT-011",7:"V1-AGT-002 V1-WFL-010",8:"V1-UPD-005",9:"V1-APP-010",10:"V1-WFL-012 V1-WFL-013 V1-WFL-014",11:"V1-WFL-002 V1-WFL-003 V1-WFL-010",12:"V1-ARCH-003 V1-GOV-008",13:"V1-TFS-008",14:"V1-DAT-001 V1-DAT-003 V1-DAT-004",15:"V1-GUI-004 V1-GUI-005 V1-GUI-006"})
    # Refine chapter-level grouped inheritance to the individual legacy clauses.
    put("API-006","V1-API-002 V1-STO-004 V1-APP-007")
    put("APP-008","V1-APP-004 V1-APP-007 V1-STO-004")
    put("PKG-006","V1-APP-001 V1-APP-006 V1-APP-007 V1-APP-008")
    put("PKG-008","V1-APP-008 V1-APP-011")
    put("PKG-010","V1-APP-008")
    put("CU-007","V1-GUI-005 V1-GUI-006 V1-GUI-010")
    put("CU-008","V1-GUI-009")
    put("GUI-002","V1-GUI-001 V1-GUI-002")
    put("GUI-003","V1-GUI-001 V1-GUI-002 V1-APP-003")
    put("GUI-004","V1-GUI-007")
    put("GUI-005","V1-GUI-002")
    put("GUI-006","V1-GUI-002 V1-GUI-003 V1-GUI-007")
    put("GUI-007","V1-GUI-003")
    put("GUI-008","V1-GUI-001 V1-GUI-002 V1-GUI-009")
    put("GUI-010","V1-GUI-005 V1-GUI-010")
    put("GUI-011","V1-GUI-004 V1-GUI-010")
    put("GUI-013","V1-GUI-007 V1-GUI-009 V1-SEC-014 V1-SEC-015")
    put("GUI-015","V1-GUI-005 V1-GUI-010")
    put("STO-003","V1-STO-004 V1-FND-004 V1-SEC-010")
    put("TFS-006","V1-TFS-004 V1-TFS-005 V1-TFS-009")
    put("TFS-007","V1-TFS-002 V1-TFS-004 V1-TFS-006")
    put("TFS-015","V1-STO-003 V1-TFS-008")
    put("CLU-010","V1-CLU-011 V1-NET-004 V1-STO-003")
    for old in "API-009 API-010 SEC-003 SEC-004 SEC-008 AGT-002 DAT-001 DAT-002 DAT-003 TFS-006 TFS-011 GUI-010 GUI-011 WFL-005 CHAT-005 R-05".split():
        M[old]["disposition"] = "superseded_by"
        M[old]["rationale"] = "旧表述的歧义或不足由 v1 明确语义替换；保留功能目标与原验收场景，不能继续使用较宽松的旧保证。"


TEST_MAP = {}


def test(old, doc, title, requirement_ids="", test_ids="", label=None):
    if title == "验收矩阵":
        title = {"06":"5. 验收与旧要求映射","07":"4. 验收、故障处理与旧要求映射"}[doc]
    targets = [{"document": DOCS[doc], "section": title}]
    if label:
        if re.search(r"\|\s*" + re.escape(label) + r"\s*\|", (ROOT / DOCS[doc]).read_text()):
            targets[0]["table_row"] = label
        else:
            targets[0]["coverage_focus"] = label
    for rid in requirement_ids.split():
        targets.append(dict(NEW_REQ[rid]))
    for tid in test_ids.split():
        found = [p for p in (ROOT / "docs").glob("*.md") if re.search(r"\|\s*`?" + re.escape(tid) + r"(?:`|\s|\|)", p.read_text())]
        assert found, (old, tid)
        targets.append({"document": "docs/" + found[0].name, "test_id": tid})
    TEST_MAP[old] = {"status": "Mapped", "disposition": "consolidated_into", "targets": targets, "rationale": "原场景和通过条件保留；所列v1条款/验收行补充纠错与故障范围，不缩减原覆盖。测试执行状态为NotRun。"}


def testmaps():
    for n, tids in {1:"SEC-V1-01",2:"SEC-V1-05 SEC-V1-11",3:"SEC-V1-02",4:"SEC-V1-06",5:"SEC-V1-13",6:"SEC-V1-14",7:"SEC-V1-08"}.items():
        test(f"SEC-T{n:02d}","04","7. 审计、隐私与故障行为",test_ids=tids)
    for n,tids in {1:"AT-STA-014",2:"AT-STA-015",3:"AT-STA-015",4:"AT-STA-015",5:"AT-STA-013"}.items():test(f"APP-T{n:02d}","07","验收矩阵",test_ids=tids)
    for n,tids in {1:"AT-STA-016",2:"AT-STA-017",3:"AT-STA-016",4:"AT-STA-014 AT-STA-015",5:"AT-STA-016"}.items():test(f"PKG-T{n:02d}","07","验收矩阵",test_ids=tids)
    for n,doc,req,label in [(1,"09","V1-AGT-002","第62步后崩溃"),(2,"08","V1-TSK-008","未知效果"),(3,"08","V1-TSK-007 V1-TSK-008","未知效果"),(4,"08","V1-TSK-006 V1-TSK-016","取消竞争"),(5,"09","V1-AGT-001 V1-AGT-007","最低能力与验收"),(6,"09","V1-AGT-008","最低能力与验收"),(7,"09","V1-AGT-006","本地模型失败")]:test(f"AGT-T{n:02d}",doc,"8.9 必须执行的验收" if doc=="08" else "9.9 最低能力与验收",req,label=label)
    for n,tids in {1:"AT-STA-020",2:"AT-STA-020 AT-STA-026",3:"AT-STA-021",4:"AT-STA-021 AT-STA-026",5:"AT-STA-021",6:"AT-STA-022 AT-STA-023",7:"AT-STA-026"}.items():test(f"CU-T{n:02d}","07","验收矩阵",test_ids=tids)
    for n,tids in {1:"AT-STA-019",2:"AT-STA-019 AT-STA-025",3:"AT-STA-021",4:"AT-STA-020",5:"AT-STA-021 AT-STA-022",6:"AT-STA-024"}.items():test(f"GUI-T{n:02d}","07","验收矩阵",test_ids=tids)
    for fam,vals in {"ACC-STO":{1:"AT-STA-001",2:"AT-STA-002",3:"AT-STA-003",4:"AT-STA-003"},"ACC-DAT":{1:"AT-STA-005 AT-STA-006",2:"AT-STA-007 AT-STA-011"},"ACC-TFS":{1:"AT-STA-008",2:"AT-STA-008 AT-STA-010",3:"AT-STA-008",4:"AT-STA-002 AT-STA-009",5:"AT-STA-003 AT-STA-011",6:"AT-STA-007 AT-STA-010",7:"AT-STA-005 AT-STA-011",8:"AT-STA-004 AT-STA-012"}}.items():
        for n,tids in vals.items():test(f"{fam}-{n:02d}","06","验收矩阵",test_ids=tids,requirement_ids="V1-TFS-008" if fam=="ACC-TFS" and n==3 else "")
    for n,req in {1:"V1-CLU-001 V1-CLU-003",2:"V1-CLU-004 V1-CLU-006 V1-CLU-007",3:"V1-CLU-005 V1-CLU-006",4:"V1-CLU-002 V1-CLU-003 V1-CLU-012",5:"V1-CLU-008 V1-CLU-009 V1-CLU-010"}.items():test(f"ACC-CLU-{n:02d}","12","7. 最低故障试验",req)
    for n,req,row in [(1,"V1-TSK-002 V1-TSK-003 V1-TSK-010","提交原子性/有界资源"),(2,"V1-TSK-011 V1-TSK-012 V1-TSK-013","事件恢复"),(3,"V1-TSK-015","定时与预算")]:test(f"ACC-EVT-{n:02d}","08","8.9 必须执行的验收",req,label=row)
    for n,req in {1:"V1-OBS-001",2:"V1-OBS-003 V1-OBS-004 V1-SEC-020",3:"V1-OBS-005 V1-OPS-004 V1-REL-005"}.items():test(f"ACC-OBS-{n:02d}","13","2. 观测与审计",req)
    for n,req,row in [(1,"V1-WFL-001 V1-WFL-011","Task/WorkItem关联"),(2,"V1-WFL-004 V1-WFL-007 V1-WFL-008 V1-WFL-010","动态展开后引擎崩溃/派发与取消交错"),(3,"V1-TSK-008 V1-REC-010","工作流/Task备份不在同一恢复点"),(4,"V1-WFL-012 V1-WFL-013 V1-WFL-014","外部ETag/Git SHA乱序"),(5,"V1-ARCH-003 V1-ARCH-004","未安装Project/Studio")]:test(f"ACC-WFL-{n:02d}","11","11.11 验收和可选部署",req,label=row)
    for n,req in {1:"V1-CHAT-001 V1-CHAT-002 V1-CHAT-009",2:"V1-CHAT-004 V1-CHAT-005 V1-CHAT-008",3:"V1-CHAT-009 V1-CHAT-010",4:"V1-CHAT-011 V1-CHAT-012",5:"V1-CHAT-009 V1-CHAT-012 V1-WFL-007",6:"V1-CHAT-014 V1-ARCH-004"}.items():test(f"ACC-CHAT-{n:02d}","10","10.10 最低验收矩阵",req)
    for fam,n,doc,req in [("AT-FND",1,"03","V1-FND-001 V1-FND-004 V1-FND-005"),("AT-FND",2,"03","V1-FND-002 V1-FND-005"),("AT-FND",3,"03","V1-FND-004 V1-STO-002"),("AT-FND",4,"03","V1-FND-007 V1-UPD-005"),("AT-UPD",1,"03","V1-UPD-001 V1-UPD-002"),("AT-UPD",2,"05","V1-UPD-003 V1-REC-008 V1-REC-018"),("AT-NET",1,"03","V1-NET-002 V1-NET-003"),("AT-DEV",1,"03","V1-DEV-001 V1-DEV-002 V1-DEV-003"),("AT-DEV",2,"03","V1-UX-002 V1-DEV-004"),("AT-REL",1,"13","V1-REL-001 V1-REL-002"),("AT-REL",2,"13","V1-REL-002 V1-REL-003")]:test(f"{fam}-{n:02d}",doc,{"03":"8. 用户路径与最低验收","05":"9. 验收、证据与原要求映射","13":"4. 安全测试与发布门禁"}[doc],req)
    xats = {
      1:("03","V1-FND-005 V1-CLU-001",""),2:("07","V1-APP-004 V1-APP-005 V1-APP-006","AT-STA-014 AT-STA-015"),3:("04","V1-SEC-003 V1-SEC-004","SEC-V1-01"),4:("08","V1-TSK-008","REC-V1-05"),5:("03","V1-UPD-001 V1-UPD-002 V1-UPD-003","REC-V1-12"),6:("05","V1-REC-004 V1-REC-005 V1-REC-007","REC-V1-01 REC-V1-02 REC-V1-03"),7:("05","V1-REC-008 V1-REC-018 V1-REC-021","REC-V1-15 AT-STA-003"),8:("12","V1-CLU-005 V1-CLU-006","REC-V1-09"),9:("07","V1-GUI-003 V1-GUI-004","AT-STA-020 AT-STA-021"),10:("03","V1-NET-002 V1-NET-003",""),11:("03","V1-DEV-002 V1-OPS-004",""),12:("09","V1-API-008 V1-AGT-008 V1-CLU-012",""),13:("03","V1-FND-005 V1-OBS-001","SEC-V1-14"),14:("13","V1-REL-001 V1-REL-002 V1-REL-003",""),15:("06","V1-DAT-004 V1-DAT-005","AT-STA-007 REC-V1-16"),16:("13","V1-BASE-001 V1-REL-005",""),17:("13","V1-CLU-002 V1-OPS-005 V1-OBS-004",""),18:("09","V1-AGT-004 V1-AGT-005 V1-AGT-006 V1-AGT-007","SEC-V1-08"),19:("02","V1-API-005 V1-OPS-002",""),20:("08","V1-TSK-003 V1-TSK-004 V1-TSK-011 V1-TSK-012 V1-TSK-013",""),21:("06","V1-TFS-001 V1-TFS-004 V1-DAT-005","AT-STA-008 AT-STA-010"),22:("06","V1-TFS-003 V1-TFS-005 V1-TFS-006","AT-STA-002 AT-STA-009 AT-STA-011"),23:("06","V1-TFS-007 V1-DAT-001 V1-DAT-004","AT-STA-005 AT-STA-011"),24:("07","V1-APP-008 V1-APP-009","AT-STA-016"),25:("07","V1-APP-009 V1-APP-010","AT-STA-016 AT-STA-017 AT-STA-018"),26:("07","V1-APP-008","AT-STA-016"),27:("07","V1-GUI-001 V1-GUI-002 V1-GUI-003","AT-STA-019 AT-STA-020"),28:("07","V1-GUI-004 V1-GUI-005 V1-GUI-006","AT-STA-021 AT-STA-022 AT-STA-023"),29:("07","V1-GUI-003 V1-GUI-007 V1-GUI-008","AT-STA-020 AT-STA-024 AT-STA-025"),30:("10","V1-CHAT-002 V1-CHAT-009",""),31:("10","V1-CHAT-004 V1-CHAT-005 V1-CHAT-008 V1-CHAT-012",""),32:("10","V1-CHAT-009 V1-CHAT-010 V1-AGT-007",""),33:("11","V1-WFL-001 V1-WFL-004 V1-TSK-008",""),34:("10","V1-CHAT-011 V1-CHAT-012","SEC-V1-01"),35:("11","V1-WFL-012 V1-WFL-013 V1-WFL-014",""),36:("12","V1-CLU-005 V1-SEC-018 V1-CHAT-003 V1-CHAT-006","SEC-V1-05"),37:("10","V1-ARCH-003 V1-ARCH-004 V1-CHAT-014",""),38:("05","V1-REC-004 V1-REC-005","REC-V1-01"),39:("08","V1-TSK-003 V1-TSK-004 V1-TSK-005 V1-TSK-006","REC-V1-04 REC-V1-06"),40:("10","V1-CHAT-004 V1-CHAT-006 V1-CHAT-007","REC-V1-10"),41:("04","V1-SEC-005 V1-SEC-012 V1-AGT-005","SEC-V1-08"),42:("04","V1-SEC-017 V1-SEC-018 V1-SEC-019","SEC-V1-13 AT-STA-007 AT-STA-026"),43:("09","V1-AGT-009 V1-AGT-010 V1-AGT-011",""),44:("11","V1-AGT-002 V1-WFL-010","REC-V1-12"),45:("07","V1-GUI-004 V1-GUI-005 V1-GUI-006","AT-STA-021 AT-STA-022 AT-STA-023")}
    titles={"02":"3. 并发、条件写和状态投影","03":"8. 用户路径与最低验收","04":"7. 审计、隐私与故障行为","05":"9. 验收、证据与原要求映射","06":"验收矩阵","07":"验收矩阵","08":"8.9 必须执行的验收","09":"9.9 最低能力与验收","10":"10.10 最低验收矩阵","11":"11.11 验收和可选部署","12":"7. 最低故障试验","13":"4. 安全测试与发布门禁"}
    for n,(doc,req,tids) in xats.items():test(f"XAT-{n:02d}",doc,titles[doc],req,tids)
    for n,doc,req,tids in [(1,"10","V1-CHAT-002 V1-CHAT-009",""),(2,"10","V1-CHAT-004 V1-CHAT-005 V1-CHAT-008",""),(3,"04","V1-SEC-005 V1-SEC-012","SEC-V1-08"),(4,"11","V1-WFL-004 V1-WFL-010 V1-AGT-002",""),(5,"08","V1-TSK-008 V1-WFL-004","REC-V1-05"),(6,"11","V1-SEC-005 V1-SEC-009 V1-WFL-007","SEC-V1-06"),(7,"11","V1-WFL-012 V1-WFL-013 V1-WFL-014",""),(8,"10","V1-CHAT-010 V1-TSK-014",""),(9,"07","V1-GUI-003","AT-STA-020"),(10,"12","V1-CLU-005 V1-CLU-008","REC-V1-09"),(11,"06","V1-TFS-004 V1-TFS-005 V1-TFS-006 V1-DAT-005","AT-STA-008 AT-STA-009 AT-STA-011"),(12,"05","V1-REC-005 V1-REC-008 V1-REC-018","REC-V1-01 REC-V1-15")]:test(f"WF-AT-{n:02d}",doc,titles[doc],req,tids)


def sort_id(s):
    fam, num=s.rsplit("-",1)
    return (fam, num.startswith("T"), int(num.lstrip("T")))


def target_text(t):
    label=t.get("requirement_id") or t.get("test_id") or t.get("section",t["document"])
    suffix="（表行："+t["table_row"]+"）" if t.get("table_row") else "（主题："+t["coverage_focus"]+"）" if t.get("coverage_focus") else ""
    return f'[{label}](../{t["document"]})'+suffix


def quote_preview(rec):
    c=rec["canonical_source"];s=clean(c.get("selected_excerpt",c["original_text"]))
    s=re.sub(r"\s+"," ",s)
    return s if len(s)<=260 else s[:260]+"…（完整原文见JSON与归档）"


def markdown_register(items, title, intro):
    out=[f"# {title}","",intro,"","所有结果为文档迁移定位；工程状态仍为 **NotVerified / NotRun**。每项保留原编号，重复出现在多个版本只算一个逻辑编号；不同文本版本和所有引用仍在JSON中分别保留。","","完整机器登记：[legacy-requirements.json](../registries/legacy-requirements.json)。来源身份、文件ID及SHA：[source-inventory.json](../sources/source-inventory.json)。","","`superseded_by` 表示 v1 的修正语义替代旧表述；`consolidated_into` 表示并入列出的明确条款；`retained_context` 表示保留产品边界、待决事项或可选能力门槛。三者都不等于实现完成。"," "]
    for fam,group in __import__('itertools').groupby(items,key=lambda r:r["family"]):
        group=list(group);out.extend([f"## {fam}（{len(group)} 项）",""])
        for r in group:
            c=r["canonical_source"];m=r["migration"]
            out.extend([f'### {r["legacy_id"]}',"",f'**迁移：** `{m["disposition"]}` / `{m["status"]}`。'+m.get("rationale",""),"",'**去向：** '+"；".join(target_text(t) for t in m["targets"])+"。","",f'**选定原文：** [{c["source_id"]}](../{c["source_file"]})，{c["section"]}，第 {c["line_start"]}–{c["line_end"]} 行。',"","> "+quote_preview(r),"",f'**溯源范围：** {len(r["source_definitions"])} 处定义、{len(r["source_references"])} 处引用；{r["historical_definition_variant_count"]} 个逐字不同的历史定义块。原文保留旧拼写/旧错误，不直接作为当前行为规范。',""])
    return "\n".join(out)+"\n"


def main():
    records=extract();maps();testmaps()
    requirements=[];tests=[];risks=[];decisions=[];work_items=[]
    for legacy_id in sorted(records,key=sort_id):
        r=records[legacy_id];cat=r["category"]
        if cat in ("requirement","risk"):
            r["migration"]=M.get(legacy_id,{"status":"NeedsReview","disposition":"retained_context","targets":[],"rationale":"未发现足够精确的v1语义去向，禁止猜测映射。"})
            r["implementation_status"]="NotVerified"
            (requirements if cat=="requirement" else risks).append(r)
        elif cat=="test":
            r["migration"]=TEST_MAP.get(legacy_id,{"status":"NeedsReview","disposition":"retained_context","targets":[],"rationale":"没有精确的新验收定位；原测试继续NotRun。"})
            r["execution_status"]="NotRun";tests.append(r)
        elif cat=="decision":
            r["decision_status"]="NotApprovedByThisPackage";decisions.append(r)
        else:
            r["work_item_status"]="HistoricalProposalOnly";r["identity_rule"]="(source_id, legacy_id); same spelling is not global identity"
            r["source_qualified_definitions"]=[{"source_qualified_id":c["source_id"]+"::"+legacy_id,**c} for c in r["source_definitions"]]
            work_items.append(r)
    rc=sum(r["migration"]["status"]=="Mapped" for r in requirements);tc=sum(r["migration"]["status"]=="Mapped" for r in tests)
    summary={"unique_requirement_ids":len(requirements),"unique_test_ids":len(tests),"unique_risk_ids":len(risks),"unique_adr_ids":len(decisions),"unique_work_item_spellings":len(work_items),"requirement_definition_count":sum(len(r["source_definitions"]) for r in requirements),"test_definition_count":sum(len(r["source_definitions"]) for r in tests),"requirements_mapped":rc,"tests_mapped":tc,"requirement_mapping_percent":round(rc/len(requirements)*100,2),"test_mapping_percent":round(tc/len(tests)*100,2),"unmapped_requirement_ids":[r["legacy_id"] for r in requirements if r["migration"]["status"]!="Mapped"],"unmapped_test_ids":[r["legacy_id"] for r in tests if r["migration"]["status"]!="Mapped"],"reference_only_requirement_ids":[r["legacy_id"] for r in requirements if r["definition_status"]!="Defined"],"reference_only_test_ids":[r["legacy_id"] for r in tests if r["definition_status"]!="Defined"],"requirement_families":dict(collections.Counter(r["family"] for r in requirements)),"test_families":dict(collections.Counter(r["family"] for r in tests))}
    registry={"schema_version":"1.0.0","document_version":"1.0.0","generated_from":"sources/source-inventory.json","scope":"Source-backed numbered requirements and tests in all archived Project md/txt and retrieved Space exports. Unnumbered narrative is addressed in migration/v0-to-v1.md, not counted as numbered requirements.","counting_rule":"Normalized unique legacy ID across versioned definitions. Work-item identity remains source-qualified; requirements/tests retain every observed definition and reference.","mapping_limit":"Mapped means a reviewed documentation destination exists, not that implementation, full semantic equivalence, test execution or ADR approval is proven.","summary":summary,"requirements":requirements,"tests":tests,"risk_references":risks,"decision_references":decisions,"work_item_references":work_items}
    (ROOT/"registries/legacy-requirements.json").write_text(json.dumps(registry,ensure_ascii=False,indent=2)+"\n")
    (ROOT/"migration/legacy-requirements.md").write_text(markdown_register(requirements,"旧规范逐项迁移表",f"旧规范共 **{len(requirements)}** 个唯一编号；已有明确 v1 文档去向 **{rc}** 项，定位覆盖率 **{summary['requirement_mapping_percent']}%**。该分母包括 BASE 的20项固定约束；旧附录单列的专业规范为225项。"))
    (ROOT/"migration/legacy-tests.md").write_text(markdown_register(tests,"旧验收逐项保留与迁移",f"旧验收共 **{len(tests)}** 个唯一编号，全部保留原场景、原通过条件及来源；已有明确 v1 文档去向 **{tc}** 项，定位覆盖率 **{summary['test_mapping_percent']}%**。不得将场景合并数解释成删减测试或提高测试通过率。"))
    print(json.dumps(summary,ensure_ascii=False,indent=2))
    if summary["unmapped_requirement_ids"] or summary["unmapped_test_ids"]:
        raise SystemExit("Unmapped legacy IDs require review")


if __name__=="__main__":
    main()
