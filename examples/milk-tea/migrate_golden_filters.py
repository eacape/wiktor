#!/usr/bin/env python3
"""Migrate examples/milk-tea/golden-queries.jsonl filters to the domain-generic
STEP10 D3 format.

把 examples/milk-tea/golden-queries.jsonl 的 filters 迁移到领域通用 STEP10 D3 格式。

The old flat filter keys (price_min/price_max/sugar_min/sugar_max/size/
ingredients/exclude_ingredients) are rewritten into the generic tagged list form
(``[{"type":"numeric_range","field":"price",...}]``), which the generalized
``GoldenFilters`` (a Vec of ``GoldenFilterCondition``) accepts. Record order,
count (134), kind quotas and all other fields are preserved.

Usage / 用法: python3 migrate_golden_filters.py
"""
import json

PATH = "examples/milk-tea/golden-queries.jsonl"


def migrate_filters(f):
    """Map a flat filters object to the generic condition list (idempotent).
    把扁平 filters 对象映射为通用条件列表（幂等）。"""
    # Already generic: a bare condition list (earlier pass) or a
    # {"conditions": [...]} object.
    if isinstance(f, list):
        return f
    if isinstance(f, dict) and "conditions" in f:
        return f["conditions"]
    if not f:
        return []
    conds = []
    if "price_min" in f or "price_max" in f:
        conds.append({"type": "numeric_range", "field": "price",
                      "min": f.get("price_min"), "max": f.get("price_max")})
    if "sugar_min" in f or "sugar_max" in f:
        conds.append({"type": "numeric_range", "field": "sugar_level",
                      "min": f.get("sugar_min"), "max": f.get("sugar_max")})
    if "size" in f:
        conds.append({"type": "text_equals", "field": "size", "value": f["size"]})
    if f.get("ingredients"):
        conds.append({"type": "ref_contains", "field": "ingredient_ids",
                      "refs": f["ingredients"]})
    if f.get("exclude_ingredients"):
        conds.append({"type": "ref_excludes", "field": "ingredient_ids",
                      "refs": f["exclude_ingredients"]})
    return conds


def main() -> None:
    out_lines = []
    n_new = 0
    with open(PATH, encoding="utf-8") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line.strip():
                out_lines.append("")
                continue
            rec = json.loads(line)
            if "filters" in rec:
                conds = migrate_filters(rec["filters"])
                rec["filters"] = {"conditions": conds} if conds else {}
                if rec.get("kind"):
                    n_new += 1
            out_lines.append(json.dumps(rec, ensure_ascii=False))
    with open(PATH, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out_lines) + "\n")
    print(f"migrated {PATH}: {len(out_lines)} records, {n_new} new-format filters updated")


if __name__ == "__main__":
    main()