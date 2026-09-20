# Step 2 Spec：手工种子 Wiki + JSONL 事实平面 + 最小查询闭环

> 版本：v1.0（2026-09-20，主模型代 planner 产出——planner-architect 两次派发均因 provider 上下文窗口超限失败，按"主模型可随时接管"经验由主模型出 spec）
> 上承接：Step1（commit 95c0652，workspace + wiktor-core 四模块 + qdrant 适配 + CLI status/vector ping）
> 下一步：Builder 按本 spec 逐条实现，测试工程师按「12. 验收判据」写测试

## 1. 目标与范围

**目标**：用 20 个手工种子 Wiki 页面 + 100+ 条商品 JSONL 事实，跑通「seed 导入 → FTS5 检索 + 事实平面过滤下推 → CLI 展示」的最小查询闭环，验证两平面数据模型与领域包 YAML 表达力——**不接 LLM**，手工页面模拟编译产物。

**范围**：
- 新增 `examples/milk-tea/` 数据集（domain.yaml / seed-wiki 20 页 / products.jsonl / golden-queries.jsonl）
- wiktor-core 新增：JSONL 数据源适配器、种子页解析、迁移 0002（中文 FTS）、kernel 的 `seed_pages`/`search` 方法、DomainConfig serde 化
- wiktor-cli 新增：`seed` / `search` 子命令
- golden-queries 雏形 ≥20 条（正式评测在 Step 4，本步通过率门槛见 §12）

**范围外**：LLM 编译、QUG、混合向量检索（qdrant 参与）、TUI、跨实体关系提取——分别属于 Step 3 / Step 4 / 后续阶段。

## 2. 关键设计决策（拍板记录）

| # | 决策 | 理由 |
|---|------|------|
| D1 | **FTS 中文分词：迁移 0002 将 `pages_fts` 重建为 `tokenize='trigram'`** | 现有 `unicode61` 对无空格 CJK 几乎无效（整段中文=1 个 token）。trigram（SQLite 3.34+，rusqlite 0.32 bundled ≈ 3.46+）按 3-gram 切分，中文短语/子串检索开箱即用且保留 BM25。DML：`DROP TABLE pages_fts`（触发器随表级联删除）→ 重建表 + 重建 3 个触发器 + 回填 `SELECT ... FROM pages`。**CURRENT_SCHEMA_VERSION → 2** |
| D2 | **短查询（<3 字符，如"珍珠"）走 LIKE 兜底** | trigram 索引要求查询 ≥3 字符；中文双字词极常见。`title LIKE '%q%' OR content LIKE '%q%'`，score 固定 1.0。实现时先写单测验证 trigram MATCH 行为 |
| D3 | **JsonlDataSource 放 wiktor-core 内 `data/` 模块**（不建独立 crate） | MASTER-PLAN §9 不预建空壳；独立 crate 等出现第二个数据源（postgres，阶段二）再拆 |
| D4 | **种子页解析放 wiktor-core 内 `seed/` 模块** | Step 3 编译管线同样产出 WikiPage，页面格式解析是 core 职责，CLI 只做文件枚举与编排 |
| D5 | **domain.yaml v1 只含 `entities` + `compile` + `query` 段**；`types/relations/qug` 段**预留但本步不解析** | 现有 `DomainConfig/EntityConfig/FieldDefinition` 结构即对齐此范围；QUG 是 Step 4 内容，YAML 结构向后可扩（serde `deny_unknown_fields` 不开） |
| D6 | **不引入 QueryEngine struct**：查询编排 = `SqliteKernel::search` 方法 + CLI 薄封装 | 单查询路径尚无编排复杂度；Step 4 的 QueryEngine 直接调用 `kernel::search` |
| D7 | **过滤下推语义**：FTS 命中知识页（entity_id）→ 事实平面过滤出满足条件的 SKU → 取其 `category` 值集合 → `pages.entity_id IN (集合)` 与 FTS 候选取交集 | 符合两平面模型：检索锚定知识页，过滤锚定 SKU 事实，经 category 关联。单条 SQL 完成，非 RRF 后过滤 |
| D8 | **过滤语法**：`--filter "price<=20,sugar_level>=50,size=中杯,ingredient_ids in=pearl,taro"`；支持 `<=`/`>=`/`=`/`in=`；**不支持 boolean 过滤**（无 BooleanEquals 条件，遇 `=true/false` 报"暂不支持"） | 最小改动集，boolean 字段仍入 facts 保 ETL 完整性 |
| D9 | **Facts/Filters 放置裁定**（struct-style-guard 上轮上报）：**维持现状**——`FactValue/Facts/Filters/FilterCondition/FieldDefinition/FieldType` 留在 `types/mod.rs`（事实平面过滤下推的公共载荷，与 error 同层聚合合理）；**修正 step1 spec §2.1 注释**（声称在 entity.rs）使其与实现一致。不动代码 | 避免无谓重构；公共导出面 `types::*` 已一致 |
| D10 | **seed 幂等键 = `pages.page_id`（INSERT OR REPLACE）+ facts CAS（source_revision）** | 重复 `wiktor seed` 不产生重复页/重复事实；content_hash 计算为 `blake3(title + content)` 供未来增量判断，本步不参与幂等判定 |

## 3. seed-wiki 页面契约

### 3.1 文件格式

每个页面 = 一个 `.md` 文件，**YAML frontmatter（`---` 包裹）+ Markdown 正文**：

```markdown
---
page_id: "milk-tea:drink:boba-milk-tea"
entity_id: "milk-tea:drink:boba-milk-tea"
title: "波霸奶茶"
entity_type: "drink"
aliases: ["珍珠奶茶", "波霸奶茶"]
tags: ["奶茶", "经典"]
---

波霸奶茶是以红茶为基底、加入波霸珍珠的经典台湾奶茶。

## 简介

波霸奶茶源自台湾……（正文）

## 成分

- 红茶
- 波霸珍珠
- 鲜奶或奶精

## 口感与特征

珍珠软糯、茶味浓郁……
```

### 3.2 字段约定（frontmatter）

| 字段 | 必填 | 类型 | 约定 |
|------|------|------|------|
| `page_id` | ✅ | string | = `entity_id`（seed 场景 1 实体 1 页）；须为合法 EntityId key（`domain:type:id`，组件无冒号非空） |
| `entity_id` | ✅ | string | 同上 |
| `title` | ✅ | string | 页面标题，写入 `pages.title` 与 FTS `title` 列 |
| `entity_type` | ✅ | string | 实体类型（drink/ingredient/brand/concept/practice），写入 `pages.entity_type` |
| `aliases` | ⬜ | string[] | 同义词，供 Step 4 QUG 同义边；本步仅解析不索引 |
| `tags` | ⬜ | string[] | 分类标签，本步仅解析不索引 |

### 3.3 正文约定

- 正文以一段**导语**（非标题段落）开头，随后是若干 `## 节`。
- **章节 = `##` 二级标题**：标题行去掉 `## ` 得 heading，后续内容直到下一个 `##` 为节内容；导语归入首节（heading = `简介`，若导语前无标题）或独立一节的规则：**导语若存在且首个 `##` 前有内容 → 归为首节，heading 记 `概述`**。
- `##` 之后的 `###` 归入当前 `##` 节的 content 原文（不细分）。
- 页面间引用（wiki 链接）：`[[target_entity_id|显示文本]]`——target 为实体 key，Step 4 关系提取的原料；本步仅原样保留在正文，不解析。

### 3.4 20 页覆盖清单

| # | 文件名（`<type>_<id>.md`） | entity_id | 类型 |
|---|--------------------------|-----------|------|
| 1 | drink_boba-milk-tea.md | milk-tea:drink:boba-milk-tea | drink |
| 2 | drink_tapioca-milk-tea.md | milk-tea:drink:tapioca-milk-tea | drink |
| 3 | drink_coconut-sago.md | milk-tea:drink:coconut-sago | drink |
| 4 | drink_mango-pomelo-sago.md | milk-tea:drink:mango-pomelo-sago | drink |
| 5 | drink_cheese-tea.md | milk-tea:drink:cheese-tea | drink |
| 6 | drink_matcha-latte.md | milk-tea:drink:matcha-latte | drink |
| 7 | drink_mango-smoothie.md | milk-tea:drink:mango-smoothie | drink |
| 8 | drink_lemon-tea.md | milk-tea:drink:lemon-tea | drink |
| 9 | ingredient_pearl.md | milk-tea:ingredient:pearl | ingredient |
| 10 | ingredient_coconut-jelly.md | milk-tea:ingredient:coconut-jelly | ingredient |
| 11 | ingredient_sago.md | milk-tea:ingredient:sago | ingredient |
| 12 | ingredient_taro-ball.md | milk-tea:ingredient:taro-ball | ingredient |
| 13 | ingredient_cheese-foam.md | milk-tea:ingredient:cheese-foam | ingredient |
| 14 | ingredient_red-bean.md | milk-tea:ingredient:red-bean | ingredient |
| 15 | brand_demo-a.md | milk-tea:brand:demo-a | brand |
| 16 | brand_demo-b.md | milk-tea:brand:demo-b | brand |
| 17 | concept_milk-tea.md | milk-tea:concept:milk-tea | concept |
| 18 | concept_fruit-tea.md | milk-tea:concept:fruit-tea | concept |
| 19 | practice_no-ice.md | milk-tea:practice:no-ice | practice |
| 20 | practice_half-sugar.md | milk-tea:practice:half-sugar | practice |

内容要求：每页 ≥2 个 `##` 节、导语 + 正文 ≥150 字，含可被中文检索命中的关键词（页面标题词在正文/导语出现）。

## 4. products.jsonl 事实平面契约

### 4.1 记录格式

JSON Lines，每行一个 SKU：

```jsonl
{"entity_id":"milk-tea:product:sku_1001","name":"波霸奶茶(中杯)","description":"红茶底+波霸","category":"milk-tea:drink:boba-milk-tea","price":18.0,"stock":120,"sugar_level":50,"size":"中杯","on_sale":true,"ingredient_ids":["milk-tea:ingredient:pearl","milk-tea:ingredient:cheese-foam"]}
```

### 4.2 字段 → facts 映射

| JSONL 字段 | field_name | FactValue | filterable | 说明 |
|-----------|-----------|-----------|-----------|------|
| `entity_id` | — | — | — | SKU 实体 key，`milk-tea:product:sku_XXXX` |
| `name` | `name` | Text | 否 | 展示 |
| `description` | `description` | Text | 否 | 展示 |
| `category` | `category` | Text | ✅ | **指向知识页 entity_id**（drink/concept/…），过滤下推的关联锚点 |
| `price` | `price` | Numeric | ✅ | 元 |
| `stock` | `stock` | Numeric | ✅ | 件 |
| `sugar_level` | `sugar_level` | Numeric | ✅ | 0-100（糖度百分比） |
| `size` | `size` | Text | ✅ | 中杯/大杯/超大杯 |
| `on_sale` | `on_sale` | Boolean | 否 | 本步不支持 boolean 过滤（D8） |
| `ingredient_ids` | `ingredient_ids` | RefList | ✅ | 配料实体 key 列表，进 `fact_refs` |

### 4.3 数量与生成

- **≥100 条** SKU，覆盖 8 个 drink 页 + 少量 concept（如 fruit-tea 下品类 SKU），价格区间 8–35 元、糖度 0–100、size 三档，保证过滤查询有区分度。
- 手写种子（首 10 条精准对齐 golden-queries 期望值）+ 脚本批量生成（确定性随机，固定 seed 便于复现）或全手工——实现时二选一，**数量必须 ≥100**。

### 4.4 幂等与 revision

- `source_revision` 默认 `1`（每行可带 `"source_revision": N` 覆盖）。
- 重导入走 `upsert_facts` 的 CAS：同 revision 覆盖、旧 revision 被拒（Step1 已实现）。

## 5. domain.yaml 领域包

```yaml
name: milk-tea
version: "0.1.0"

entities:
  - name: product
    source: jsonl://examples/milk-tea/products.jsonl
    id_field: entity_id
    type_field: category
    fields:
      - { name: name, field_type: text, filterable: false }
      - { name: description, field_type: text, filterable: false }
      - { name: category, field_type: text, filterable: true }
      - { name: price, field_type: numeric, filterable: true }
      - { name: stock, field_type: numeric, filterable: true }
      - { name: sugar_level, field_type: numeric, filterable: true }
      - { name: size, field_type: text, filterable: true }
      - { name: on_sale, field_type: boolean, filterable: false }
      - { name: ingredient_ids, field_type: reflist, filterable: true }

compile:
  quality_threshold: 0.75
  max_recompiles: 2

query:
  filters: [price, sugar_level, size, ingredient_ids]
```

解析要求（D5）：
- `DomainConfig`/`EntityConfig` 增加 `#[derive(Deserialize)]` + `#[serde(rename_all = "snake_case")]`（对齐 `FieldType` 已有标签）。
- `EntityConfig.fields` 直接复用 `FieldDefinition`（已 serde）。
- `type_field`/`id_field` 本步仅解析不消费（保留在结构里）。
- `query.filters` 解析为 `Vec<String>` 存 DomainConfig（新增字段，serde default = vec![]），CLI 校验过滤器字段名 ∈ 此列表（宽松：未配置也可过滤，仅提示）。
- `source` 的 `jsonl://` 前缀剥掉后为相对路径，**相对 domain.yaml 所在目录**解析。

## 6. golden-queries.jsonl 雏形

```jsonl
{"query":"波霸奶茶","expected_hits":["milk-tea:drink:boba-milk-tea"],"filters":{}}
{"query":"珍珠奶茶","expected_hits":["milk-tea:drink:boba-milk-tea","milk-tea:drink:tapioca-milk-tea"],"filters":{}}
{"query":"奶茶","expected_hits":["milk-tea:concept:milk-tea","milk-tea:drink:boba-milk-tea","milk-tea:drink:tapioca-milk-tea"],"filters":{}}
{"query":"价格低于20元的奶茶","expected_hits":["milk-tea:drink:boba-milk-tea","milk-tea:drink:lemon-tea"],"filters":{"price_max":20}}
{"query":"不要珍珠","expected_hits":["milk-tea:drink:coconut-sago","milk-tea:drink:mango-pomelo-sago"],"filters":{"exclude_ingredients":["milk-tea:ingredient:pearl"]}}
```

记录格式：

| 字段 | 类型 | 说明 |
|------|------|------|
| `query` | string | 自然语言查询词 |
| `expected_hits` | string[] | 期望命中的知识页 entity_id（**知识页**，非 SKU） |
| `filters` | object | 扁平过滤：`price_max` / `price_min` / `sugar_max` / `sugar_min` / `size` / `ingredients`(=RefContains) / `exclude_ingredients`(=RefExcludes) |

- **≥20 条**，覆盖：精确词（≥4 字 MATCH 路径）、双字词（<3 字 LIKE 路径，如"珍珠"）、数字过滤（price/sugar）、排除（不含珍珠）、组合（词+过滤）。
- **通过判定（本步）**：`search(query, filters)` 返回的命中集合与 `expected_hits` **交集非空**即算通过（Step 4 才收紧为排序/全命中评估）。
- **通过率门槛：≥80%**（手工种子数据可控，此门槛合理；达不到=实现 bug）。

## 7. DDL 增量：迁移 0002

```sql
-- pages_fts 重建为 trigram（unicode61 对中文无效）。DROP 级联删除原 3 个触发器。
DROP TABLE IF EXISTS pages_fts;

CREATE VIRTUAL TABLE pages_fts USING fts5(
    page_id UNINDEXED,
    entity_id UNINDEXED,
    title,
    content,
    tokenize = 'trigram'
);

CREATE TRIGGER pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_update AFTER UPDATE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_delete AFTER DELETE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
END;

-- 回填存量页面（空库时无操作，语义安全）
INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT page_id, entity_id, title, content FROM pages;
```

- `CURRENT_SCHEMA_VERSION` 1 → **2**；`migrations()` 追加 `(2, MIGRATION_0002)`。
- 迁移自检单测：迁移 2 次幂等；`pages_fts` 的 sqlite_master SQL 含 `trigram`；回填后行数 = pages 行数。

## 8. JsonlDataSource（wiktor-core `data/` 模块）

```rust
// crates/wiktor-core/src/data/jsonl.rs
pub struct JsonlDataSource {
    path: PathBuf,          // jsonl:// 前缀剥离后的文件路径
    schema: EntitySchema,   // 由 EntityConfig 构造
}

impl JsonlDataSource {
    /// 从 jsonl:// URI（相对 base_dir 解析）构造。
    pub fn from_config(cfg: &EntityConfig, base_dir: &Path) -> Result<Self>;
}

#[async_trait]
impl DataSource for JsonlDataSource {
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;
    fn schema(&self) -> EntitySchema;
}
```

- **游标**：`Cursor { offset, batch_size }`——`fetch` 从 `offset` 行开始读 `batch_size` 行；行号 = 文件行号（1-based）；EOF 返回空 Vec = 终止。
- **坏行策略：fail-fast**（返回 `Error::DataSource` 含行号与错误），seed 场景数据可控，早暴露问题。
- 每行解析：`entity_id`（完整 key → `EntityId::from_key`）、`source_revision`（缺省 1）、其余字段 → `BTreeMap<String, serde_json::Value>`。
- `EntitySchema.entity_type` = `EntityConfig.name`；`fields` = `EntityConfig.fields`。
- 不引入新依赖（serde_json 已有）。

## 9. seed 导入（CLI `wiktor seed` + core `seed/` 模块 + `SqliteKernel::seed_pages`）

### 9.1 命令

```
wiktor seed --db <path> --domain <domain.yaml> [--pages <dir>]
```

- `--domain` 必填；`--pages` 默认 `<domain.yaml 所在目录>/seed-wiki`。
- 事实文件路径：遍历 domain.yaml `entities[].source` 中 `jsonl://` 项，相对 domain.yaml 目录解析；**本步只导入第一个 `jsonl://` entity**（多个源留 Step 3）。

### 9.2 core `seed/` 模块（crates/wiktor-core/src/seed/mod.rs）

```rust
/// 解析一个 seed-wiki Markdown 文件 → WikiPage（frontmatter YAML + 正文章节切分）。
pub fn parse_page(content: &str) -> Result<WikiPage>;
```

- frontmatter：`---` 首行起、次行止（`---\n`），YAML 解析为 `SeedFrontmatter { page_id, entity_id, title, entity_type, aliases, tags }`（serde_yaml_ng）。
- 正文 = frontmatter 之后全部 Markdown；`##` 切分章节（§3.3）；导语归首节 heading=`概述`。
- `WikiPage { page_id, entity_id, title, content, sections, metadata }`；`metadata.domain_pack_version` 由调用方（CLI 从 domain.yaml）填充，`compiled_at = now`，`model_version = "seed-manual"`，`embedding_model = "none"`。
- `page_id` 从 frontmatter 的 page_id 字段取（解析为 EntityId key 校验合法性）。

### 9.3 `SqliteKernel::seed_pages`

```rust
pub fn seed_pages(&self, page: &WikiPage, domain: &str, status: PublishStatus) -> Result<()>;
```

- 单事务：`INSERT OR REPLACE INTO pages (...)`（PK=page_id，幂等）+ 删除旧 `page_sections` 重插（级联已删则免）+ `INSERT OR REPLACE INTO page_quality`（评分全 1.0，seed 手工页默认满分）。
- `pages` 列填充：`page_id`、`entity_id`(key)、`domain`、`entity_type`（from frontmatter，需加到 WikiPage 或从 EntityId 取——**用 EntityId.entity_type**）、`title`、`content`、`content_hash = blake3(title + "\0" + content)`、`generation = 1`、`status`、`domain_pack_version`、`compiled_at`、`model_version`、`embedding_model`、`created_at = updated_at = now`。
- FTS 由触发器自动同步（迁移 0002 重建后）。REPLACE 语义触发 UPDATE 触发器（DELETE+INSERT），无残留。

### 9.4 CLI 编排（seed）

1. 解析 domain.yaml → `DomainConfig`（serde_yaml_ng）。
2. 枚举 `--pages` 目录 `*.md` → `seed::parse_page` 逐个 → `kernel.seed_pages`（status=Accepted）。
3. 遍历 entities → `JsonlDataSource::from_config` → 循环 `fetch(cursor)` 直到空 → 每行转 `Facts`（字段按 FieldDefinition.field_type 转 `FactValue`）→ `kernel.upsert_facts`。
4. 输出统计：pages 数、facts 数、fact_refs 数、耗时。

**类型转换规则**（JSONL Value → FactValue）：

| FieldType | JSON | FactValue |
|-----------|------|-----------|
| numeric | number | Numeric(f64) |
| text | string | Text |
| boolean | bool | Boolean |
| reflist | string[] | RefList(Vec<String>) |
| 其它/缺失 | — | `Error::Validation`（缺必填字段报错带 entity_id） |

## 10. 查询编排（`SqliteKernel::search` + CLI `wiktor search`）

### 10.1 命令

```
wiktor search "波霸奶茶" --db <path> [--filter "price<=20,sugar_level>=50,size=中杯"] [--top-k 5]
```

### 10.2 filter 语法（D8，wiktor-cli 内解析）

| 语法 | FilterCondition |
|------|----------------|
| `price<=20` | NumericRange{field:price, min:None, max:Some(20)} |
| `price>=15` | NumericRange{field:price, min:Some(15), max:None} |
| `size=中杯` | TextEquals{field:size, value:中杯} |
| `ingredient_ids in=a,b` | RefContains{field:ingredient_ids, refs:[a,b]} |
| `ingredient_ids not_in=a` | RefExcludes{field:ingredient_ids, refs:[a]} |
| `on_sale=true` | **报错："boolean 过滤暂不支持"** |

解析规则：先找 `<=`/`>=`，再找 `=`/`in=`/`not_in=`；数值字段解析 f64。非法 → `anyhow` 错误退出。

### 10.3 `SqliteKernel::search`

```rust
pub fn search(
    &self,
    text: &str,
    filters: &Filters,
    top_k: usize,
    domain: Option<&str>,
) -> Result<Vec<SearchHit>>;
```

**长查询（`text.chars().count() >= 3`）**：

```sql
SELECT p.page_id, p.entity_id, p.title, bm25(pages_fts) AS score
FROM pages_fts f
JOIN pages p ON p.page_id = f.page_id
WHERE f.pages_fts MATCH ?1          -- ?1 = "\"<text>\""（引号转义后包成短语）
  AND p.status = 'accepted'
  [AND p.domain = ?N]
  [AND p.entity_id IN (
      SELECT DISTINCT cat.value_text
      FROM facts cat
      JOIN (SELECT DISTINCT entity_id FROM facts WHERE <filter_where>) ft
        ON ft.entity_id = cat.entity_id
      WHERE cat.field_name = 'category' AND cat.field_type = 'text'
  )]
ORDER BY score LIMIT ?M;
```

**短查询（<3 字符，如"珍珠"）**：MATCH 改 LIKE：

```sql
WHERE (p.title LIKE ?q OR p.content LIKE ?q)   -- ?q = "%珍珠%"
  AND p.status = 'accepted' [...过滤同上...]
ORDER BY p.page_id LIMIT ?M;                   -- score 统一记 1.0
```

**过滤下推实现**：新增 `schema::facts::filter_where(filters) -> Result<Option<(String, Vec<Value>)>>`，返回纯 WHERE 片段（现有 `translate_filters` 改为基于它拼完整 SELECT，保持兼容）。无过滤 → 省略 IN 子句。

**query_logs 写入**：search 内部写一条日志（`query_text`、`query_json`=serde(Query)、`rewrite_failure=0`、`hit_count`、`latency_ms`、`timestamp`）。MATCH 语法错误等失败路径也写（hit_count=0）再返回错误。

### 10.4 CLI 展示

```
$ wiktor search "珍珠奶茶" --db wiktor.db --filter "price<=20" --top-k 3
score  entity_id                           title
0.2874 milk-tea:drink:boba-milk-tea        波霸奶茶
0.2150 milk-tea:drink:tapioca-milk-tea     珍珠奶茶
```

列：score(4f) / entity_id / title，固定宽度对齐，无命中输出 `no hits`。

## 11. 文件与模块布局

```
examples/milk-tea/
├── domain.yaml
├── seed-wiki/                    # 20 个 *.md（§3.4 清单）
├── products.jsonl                # ≥100 条 SKU（§4）
└── golden-queries.jsonl          # ≥20 条（§6）

crates/wiktor-core/src/
├── data/                         # 新增：数据源适配器
│   ├── mod.rs                    #   pub mod jsonl; 模块头注释
│   └── jsonl.rs                  #   JsonlDataSource
├── seed/                         # 新增：种子页解析
│   ├── mod.rs                    #   parse_page + SeedFrontmatter
│   └── tests 内置
├── schema/migrations.rs          # 改：CURRENT_SCHEMA_VERSION=2 + MIGRATION_0002
├── schema/facts.rs               # 改：+filter_where，translate_filters 复用
├── kernel/sqlite.rs              # 改：+seed_pages +search（含日志）
├── traits/domain_pack.rs         # 改：DomainConfig/EntityConfig serde 化 + query.filters
└── lib.rs                        # 改：pub mod data; pub mod seed;

crates/wiktor-cli/src/
├── main.rs                       # 改：+Seed +Search 子命令
├── filter.rs                     # 新增：--filter 语法解析 → Filters
└── seed.rs                       # 新增：seed 编排（文件枚举+导入统计）或并入 main

Cargo.toml                        # 改：workspace.dependencies + serde_yaml_ng
crates/wiktor-core/Cargo.toml     # 改：+serde_yaml_ng
docs/design/step1-workspace-core-schema.md  # 改：§2.1 注释 Facts/Filters 实际在 types/mod.rs（D9 裁定修正）
```

依赖：workspace 加 `serde_yaml_ng = "0.10"`（core 引用；若版本冲突回退 0.9）。

## 12. 验收判据（test-engineer 照此写测试）

| # | 判据 | 断言 |
|---|------|------|
| A1 | 迁移 0002 幂等 | 连续 `migrate` 两次：schema 版本=2、迁移记录 2 条、无错 |
| A2 | 迁移 0002 trigram | `sqlite_master` 中 pages_fts SQL 含 `trigram`；回填行数=pages 行数 |
| A3 | seed 导入 | `seed_pages` 后：pages=20、page_sections≥40、page_quality=20 |
| A4 | seed 幂等 | 同页面重复 `seed_pages`：pages 行数不变、content_hash 不变 |
| A5 | 事实导入 | 100 条 SKU 导入后：facts ≥ 100×8（字段数）、fact_refs = 各 SKU reflist 总数 |
| A6 | 事实 CAS | 同 entity 低 revision 重导不覆盖（复用 Step1 单测语义） |
| A7 | 中文 MATCH（长查询） | search "波霸奶茶" 命中 boba-milk-tea 页，top1 分数>0 |
| A8 | 中文 LIKE（短查询） | search "珍珠" 命中含珍珠的页（≥1），score=1.0 |
| A9 | 过滤下推 | search "奶茶" + price_max=20：返回页均存在 price≤20 的 category SKU；不含无低价 SKU 的页 |
| A10 | 排除过滤 | search "奶茶" + exclude_ingredients=[pearl]：结果页无含珍珠 SKU 的饮品 |
| A11 | 组合过滤 | price 区间 + size 相等同时生效（两条件 AND） |
| A12 | 查询日志 | search 后 query_logs 行数 +1，hit_count 与返回一致 |
| A13 | golden-queries | 20 条跑通 ≥80%（交集非空判定）；`cargo test --workspace` 有集成测试跑 `examples/milk-tea` 全量 |
| A14 | 工程规范 | `cargo fmt --check` 干净、`cargo clippy --workspace --all-targets` 0 warning、全量测试绿 |

## 13. 实现顺序建议

1. 迁移 0002（trigram）+ 单测（A1/A2）——先验证中文 FTS 可行性
2. types serde 化 + serde_yaml_ng 依赖 + `filter_where` 重构（不破坏现有 translate_filters）
3. `data/jsonl.rs`（A5 依赖）+ `seed/mod.rs`（A3 依赖）
4. `SqliteKernel::seed_pages` + `search`（A4/A7-A12）
5. `examples/milk-tea/` 数据集（20 页 + ≥100 SKU + domain.yaml + golden）
6. CLI `seed` / `search` + filter 解析
7. golden-queries 集成测试（A13）+ 全量验收（A14）
8. struct-style-guard 巡检 → scp 上传 Linux → Linux push → 本机 pull

---

## 14. 实现修正记录（2026-09-20/21，主模型实现）

以下为实现过程中对 spec 的修正，均已落地并验证（44 测试全绿、clippy 0 warning）：

1. **存储层切 diesel（用户拍板）**：删除 rusqlite 依赖，`wiktor-core` 改用 **diesel 2.x（SQLite bundled，libsqlite3-sys 带 bundled feature）** + diesel_migrations。pages/facts/fact_refs/page_sections/page_quality 的 CRUD 走 diesel ORM DSL（`db_schema.rs` 的 table! 宏）；**FTS5 MATCH/bm25、过滤下推 IN 子查询、CAS upsert、查询日志** 属核心检索 SQL，保留 `diesel::sql_query` raw SQL 逃生（SQLite 类型亲和：数值参数以文本内联，REAL 列自动转换）。
2. **迁移系统改 diesel embed_migrations**：`migrations/0001_create_core/up.sql` + `migrations/0002_fts_trigram/up.sql`（版本跟踪表 `__diesel_schema_migrations`，`schema_version()` = 已应用迁移数）。**0002 修正：先 DROP 三个触发器再重建**（0001 已建同名触发器，否则 "trigger already exists"）。
3. **filter 分隔符**：条件间用 `,`；`in=`/`not_in=` 列表项用 **`|`**（原 spec 用 `,` 与条件分隔符冲突，已改）。
4. **refs 参数须用完整 entity key**：`ingredient_ids in=|not_in=` 的值必须是与 fact_refs 存储一致的完整 key（如 `milk-tea:ingredient:pearl`），短名（`pearl`）匹配不到行。
5. **解析库（不重复造轮子，用户拍板）**：frontmatter 用 `gray_matter`（YAML engine）替代手写 `---` 边界；正文章节切分用 `pulldown-cmark` 的 `into_offset_iter` 按 H2 事件取 offset，替代手写行匹配。
6. **SearchHit 增加 `title` 字段**（CLI 展示列需要）。
7. **短查询（<3 字符）LIKE 路径的过滤下推已由 `seed_pages_then_search_chinese`/golden 覆盖**：LIKE + IN(category) 组合在 SQLite 下正常生效（`%text%` 文本内联）。
8. **golden 判定**：交集非空即通过（spec §6 原样）；29 条 golden 实际通过率 100%（≥80% 门槛满足）。

**双语约束（2026-09-21 用户拍板）**：本项目文档与代码注释需英文版/中文版两个版本。本次 Step 2 涉及文件（代码注释、migrations、CLI、测试）已按中英并列注释书写；本 spec 中文版为权威，英文版 `step2-seed-wiki-query-loop.en.md` 待 doc-writer 同步（存量 Step1 文件后续统一补）。

---

**文档版本**：v1.1（含实现修正记录 v1.0→v1.1）。**下一步**：Step 3（LLM 编译管线 + 质量评分）。
