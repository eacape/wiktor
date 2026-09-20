#!/usr/bin/env python3
"""Generate examples/milk-tea/products.jsonl (deterministic, >=100 SKUs).

生成 examples/milk-tea/products.jsonl（确定性随机，>=100 条 SKU）。

Usage / 用法: python3 gen_products.py [count] [seed]
"""
import json
import random
import sys

DEFAULT_COUNT = 120
DEFAULT_SEED = 20260921

# Knowledge pages used as `category` (drink pages + fruit-tea concept page).
# 作为 `category` 的知识页（8 个饮品页 + 水果茶概念页）。
CATEGORIES = [
    "milk-tea:drink:boba-milk-tea",
    "milk-tea:drink:tapioca-milk-tea",
    "milk-tea:drink:coconut-sago",
    "milk-tea:drink:mango-pomelo-sago",
    "milk-tea:drink:cheese-tea",
    "milk-tea:drink:matcha-latte",
    "milk-tea:drink:mango-smoothie",
    "milk-tea:drink:lemon-tea",
    "milk-tea:concept:fruit-tea",
]

SIZES = ["中杯", "大杯", "超大杯"]

# Ingredient entity keys (fact_refs).
# 配料实体 key（写入 fact_refs）。
INGREDIENTS = [
    "milk-tea:ingredient:pearl",
    "milk-tea:ingredient:coconut-jelly",
    "milk-tea:ingredient:sago",
    "milk-tea:ingredient:taro-ball",
    "milk-tea:ingredient:cheese-foam",
    "milk-tea:ingredient:red-bean",
]

# Chinese display names per knowledge page (used for `name`/`description`).
# 各知识页的中文展示名（用于 name/description）。
DRINK_NAMES = {
    "milk-tea:drink:boba-milk-tea": "波霸奶茶",
    "milk-tea:drink:tapioca-milk-tea": "珍珠奶茶",
    "milk-tea:drink:coconut-sago": "椰香西米露",
    "milk-tea:drink:mango-pomelo-sago": "杨枝甘露",
    "milk-tea:drink:cheese-tea": "芝士奶盖茶",
    "milk-tea:drink:matcha-latte": "抹茶拿铁",
    "milk-tea:drink:mango-smoothie": "芒果冰沙",
    "milk-tea:drink:lemon-tea": "柠檬茶",
    "milk-tea:concept:fruit-tea": "水果茶",
}

def main() -> None:
    count = int(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_COUNT
    seed = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_SEED
    rng = random.Random(seed)

    # base price range per category (yuan); lemon/fruit-tea cheap, mango dear.
    # 每类的基础价格区间（元）：柠檬/果茶便宜，芒果类偏贵。
    base_price = {
        "milk-tea:drink:boba-milk-tea": (14, 22),
        "milk-tea:drink:tapioca-milk-tea": (12, 20),
        "milk-tea:drink:coconut-sago": (13, 21),
        "milk-tea:drink:mango-pomelo-sago": (22, 35),
        "milk-tea:drink:cheese-tea": (18, 28),
        "milk-tea:drink:matcha-latte": (16, 26),
        "milk-tea:drink:mango-smoothie": (20, 32),
        "milk-tea:drink:lemon-tea": (8, 16),
        "milk-tea:concept:fruit-tea": (10, 24),
    }
    # fixed ingredient sets per category.
    # 每类固定配料集合。
    category_ingredients = {
        "milk-tea:drink:boba-milk-tea": ["milk-tea:ingredient:pearl"],
        "milk-tea:drink:tapioca-milk-tea": ["milk-tea:ingredient:pearl"],
        "milk-tea:drink:coconut-sago": ["milk-tea:ingredient:coconut-jelly"],
        "milk-tea:drink:mango-pomelo-sago": ["milk-tea:ingredient:coconut-jelly"],
        "milk-tea:drink:cheese-tea": ["milk-tea:ingredient:cheese-foam"],
        "milk-tea:drink:matcha-latte": ["milk-tea:ingredient:taro-ball"],
        "milk-tea:drink:mango-smoothie": [],
        "milk-tea:drink:lemon-tea": [],
        "milk-tea:concept:fruit-tea": ["milk-tea:ingredient:sago"],
    }

    lines = []
    for i in range(1, count + 1):
        cat = CATEGORIES[i % len(CATEGORIES)]
        lo, hi = base_price[cat]
        price = round(rng.uniform(lo, hi), 1)
        stock = rng.randint(0, 300)
        sugar = rng.randint(0, 100)
        size = SIZES[i % len(SIZES)]
        # 8% of SKUs are off-sale.
        # 8% 的商品停售。
        on_sale = rng.random() >= 0.08
        ings = list(category_ingredients[cat])
        # sometimes add a random extra topping.
        # 偶尔加一个随机小料。
        if rng.random() < 0.3:
            extra = rng.choice(INGREDIENTS)
            if extra not in ings:
                ings.append(extra)

        sku_id = f"sku_{i:04d}"
        zh = DRINK_NAMES[cat]
        record = {
            "entity_id": f"milk-tea:product:{sku_id}",
            "name": f"{zh}({size})",
            "description": f"{size} {zh}",
            "category": cat,
            "price": price,
            "stock": stock,
            "sugar_level": sugar,
            "size": size,
            "on_sale": on_sale,
            "ingredient_ids": ings,
            "source_revision": 1,
        }
        lines.append(json.dumps(record, ensure_ascii=False))

    out = "examples/milk-tea/products.jsonl"
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {len(lines)} SKUs to {out}")

if __name__ == "__main__":
    main()
