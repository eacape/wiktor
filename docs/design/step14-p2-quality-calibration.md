# Step 14 (P2-II) 设计规范：评分器验证与校准（方向一）

> 版本：v1.0（2026-09-26）
> 权威依据：`docs/MASTER-PLAN.md` §5.x（编译质量门禁）；用户拍板「方向一」——把 P2-II 从「人工标注 50–100 页算相关系数」重定义为「评分器验证 + 校准」（区分度探针 + 候选页人工判定一致率）
> 实现对象：区分度探针 `wiktor quality`（主模型已实现并实测）；一致率管道（本文档给出规范，标注执行为人工/离线流程）

## 1. 背景：满分饱和（ceiling effect）与方案重定向

原 P2-II 方案拟人工标注 50–100 页，与评分器五维连续分算相关系数（Spearman/Pearson）来「验证评分器」。

**实测推翻该前提**：评分器是**规则式 + 抽取式输出契约**——只要页通过了编译 gate，其五维（coverage/density/citation/schema/consistency）必然全 1.0。已在三处独立库验证：

| 库 | page_count | 非满分页 | 结论 |
|---|---|---|---|
| 生产 serve（真实 LLM 编译） | 21 | 0 | 全 1.0 |
| seed 库（确定性导入） | 20 | 0 | 全 1.0 |
| mock 编译库（accepted 页） | 3 | 0 | 全 1.0 |

**零方差上相关系数未定义**（分母为零）。评分器只在一个点上给出真实区分：**accept/reject gate**——对劣质页（低 coverage、稀疏 density、缺 citation）拒绝，对其余放行；单测 A9/A10 已证明该区分。

因此用户拍板**方向一**：不再做「标注→相关」，而做「验证 + 校准」双管道：

1. **区分度探针**（查看评分器到底在哪些页上区分、哪些是满分饱和）；
2. **候选页人工判定一致率**（人工 should-publish 判定 vs 评分器 accept/reject gate，测两者一致率 + kappa），作为评分器门禁可信度的验证。

## 2. 交付物总览

| 编号 | 交付物 | 状态 |
|---|---|---|
| A1 | 评分器能力审计（ceiling 判定） | 已完成（本文档 §1 结论） |
| A2 | 区分度探针 `wiktor quality` | 已完成 + 3 库实测（§3） |
| A3 | 人工标注规范草案 + 一致率测量管道规范 | 本文档 §4/§5（待人工执行） |

## 3. 区分度探针（已实现）

`wiktor quality [--json]`（只读，`crates/wiktor-cli/src/main.rs` `cmd_quality`）。

**输出契约**：
- 人类表：首行 `page_count=N  non_uniform=M (not-all-1.0 pages)`；M=0 且 N>0 时打印中英双语 ceiling 说明——「全部页满分（ceiling）——零方差上相关系数未定义；评分区分仅对劣质页成立」；随后逐页五维表 `overall/status/cov/cit/den/sch + title` 与均值行。
- `--json`：`{page_count, non_uniform_count, mean:{coverage,citation,schema_compliance,density,overall}, non_uniform_pages:[{page_id,title,status,coverage,citation,density,overall}]}`。
- `non_uniform` 判定 = 任一切面 < 1.0（coverage/citation/schema_compliance/density/overall）。

**三库实测**：全部 `page_count>0`、`non_uniform=0`、双语文案如实报出 ceiling——探针本身验证通过。

## 4. 人工标注规范草案（A3）

### 4.1 样本单元与抽取（关键设计）
**必须同时抽取 accepted 与 rejected/quarantined 页**，否则会复现 ceiling：若只抽 accepted 页，人工判定应全为 should-publish=Y，一致率恒 1.0，无信息量（等于把评分器的零方差搬进人工集）。

- 抽样范围：编译任务里所有**进入过 gate** 的候选页（含 rejected/quarantined），按状态分层（stratified）各取 25–50 页，共 ≥50 页。
- 每页附带其**来源片段**（源文档对应段落/要点）与**编译后产物**（编译出的 article + `[[ref:rN]]` 引用），供人工在不看评分器输出的前提下独立判定。

### 4.2 判定维度
主判（进入一致率计算）：

- **should-publish（Y/N）**：基于来源，该页是否达到了「可对外发布为知识库条目」的质量底线。

分辩（供校准定位，不进入 κ 主判）：

- **coverage**：是否覆盖了来源的全部关键要点？
- **density**：是否存在凑字数/低信息密度片段？
- **citation**：关键事实是否都有 `[[ref]]` 指向来源？
- **schema**：产物是否遵守了输出契约（标题/分节/正文结构）？
- **consistency**：是否存在与来源矛盾的事实陈述？

### 4.3 判定准则（与评分器规则对齐）
人工按 4.2 逐维给 Y/N，任一关键维为 N（尤其 coverage 缺要点 / citation 缺事实引用 / density 空洞）即 **should-publish=N**。评价尺子与评分器规则同源，保证「一致率」语义是「两把相同的尺子在不同执行者手上是否一致」，而非两套标准互比。

### 4.4 工具与格式
- 离线化：标注结果建议为 CSV/JSONL 一行一页：`page_id,status,human_should_publish,human_coverage,human_density,human_citation,human_schema,human_consistency,note`。
- **标注者不见评分器输出**（盲评）；评分器 status 在合并前分离。

## 5. 一致率测量管道（A3）

### 5.1 流程
1. 4.1 抽样 → 落标注表；2. 盲评填 `human_should_publish`；3. 合并 `status`（scorer）与 `human_should_publish`，得 2×2 混淆矩阵；4. 计算一致率与 Cohen's kappa；5. 对照 §5.3 门限，超限则验证通过，未超限进入 5.4 校准。

### 5.2 统计量
- **一致率**：`(Y/Y + N/N) / total`。
- **Cohen's kappa** `κ = (P_o − P_e) / (1 − P_e)`，`P_e` 为两判定的边缘概率下的期望一致率（修正类内基线一致）。κ ≥ 0.8 视为强一致、0.6–0.8 中等、<0.6 需校准。

### 5.3 验证门限（建议）
- 门禁通过：一致率 ≥ 0.85 **且** κ ≥ 0.6（在 ≥50 页、含 rejected 的样本上）。
- 说明：由于是「同一把规则尺」的复现，预期一致率应偏高；若反而低，说明**规则有歧义或抽取式实现与规则描述脱节**——正是要暴露的点。

### 5.4 校准动作（当未通过）
1. 定位不一致行：`human=Y, status≠accepted`（漏放）或 `human=N, status=accepted`（误放）。
2. 漏放为主 → 收紧 scoring gate 或补该维度规则；误放为主 → 放宽/复核抽取实现。
3. 改后重跑 §5.3，直至达标；每次校准记录偏差（STEP14-P2-II-xxx）。

## 6. 验收

- A2 探针落地：`wiktor quality`（人类表 + `--json`）在 seed / mock 编译库上如实报 ceiling（实测 PASS）。
- A3 规范就绪：本文档 §4/§5 可作为人工标注与一致率计算的操作手册；管道无需新增 Rust 代码，数据为离线表 + 一次性脚本（或 `feedback`/`quality` 只读面导出）即可计算。
- 门禁：workspace 全绿 + clippy `-D warnings` 0 + fmt 干净。

## 7. 范围说明（明确没做）

- 不做「人工标注 50–100 页算相关系数」（被 §1 ceiling 推翻，用户拍板方向一）。
- 一致率管道本文档只出规范，**标注执行为线下流程**，不落自动标注工具（无真实人工标注者参与时自动化无意义）。
- kappa/一致率的计算脚本若后续需要，可作为一次性工具补做，不在本次代码改动范围。