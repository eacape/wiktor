# Step 14 (P2-II) Design Spec: Scorer Verification & Calibration (Direction One)

> Version: v1.0 (2026-09-26)
> Authoritative basis: `docs/MASTER-PLAN.md` §5.x (compile quality gate); user decision "Direction One" — redefine P2-II from "annotate 50–100 pages and compute correlation" into "scorer verification + calibration" (a discrimination probe + a human-agreement-rate pipeline on candidate pages).
> Implemented by: the `wiktor quality` discrimination probe (implemented and measured by the main model); the consistency pipeline (spec'd in this doc; annotation is a human/offline process).

## 1. Background: ceiling effect and the redirect

The original P2-II planned to manually annotate 50–100 pages and compute a rank correlation (Spearman/Pearson) between the scorer and its five continuous dimensions, to "verify the scorer."

**Measurements overturn that premise**: the scorer is rule-based + extraction-shaped output contract — any page that passes the compile gate necessarily scores all-1.0 on the five dimensions (coverage/density/citation/schema/consistency). Verified on three independent DBs:

| DB | page_count | non-full-score pages | result |
|---|---|---|---|
| production serve (real-LLM compile) | 21 | 0 | all 1.0 |
| seed DB (deterministic import) | 20 | 0 | all 1.0 |
| mock-compile DB (accepted pages) | 3 | 0 | all 1.0 |

**Correlation is undefined on zero variance** (zero denominator). The scorer only discriminates at one point: the **accept/reject gate** — rejecting low-quality pages (low coverage, sparse density, missing citation) and passing the rest; unit tests A9/A10 prove that discrimination.

The user therefore chose **Direction One**: not "annotate → correlate", but a "verify + calibrate" pipeline:

1. a **discrimination probe** (see where the scorer actually discriminates, and which pages are full-score / ceiling);
2. a **human-agreement-rate pipeline on candidate pages** (human should-publish verdict vs the scorer accept/reject gate, measured as agreement + κ), as the verification of the scorer gate's trustworthiness.

## 2. Deliverable overview

| # | deliverable | status |
|---|---|---|
| A1 | scorer capability audit (ceiling verdict) | done (§1 conclusion of this doc) |
| A2 | `wiktor quality` discrimination probe | done + measured on 3 DBs (§3) |
| A3 | human annotation spec + consistency-rate pipeline spec | this doc §4/§5 (pending human run) |

## 3. Discrimination probe (implemented)

`wiktor quality [--json]` (read-only; `crates/wiktor-cli/src/main.rs` `cmd_quality`).

**Output contract**:
- Human table: first line `page_count=N  non_uniform=M (not-all-1.0 pages)`; when M=0 and N>0 it prints the bilingual ceiling note — "all pages score all-1.0 (ceiling effect) — correlation is undefined on zero variance; discrimination is on low-quality pages only" — then a per-page five-dimension table `overall/status/cov/cit/den/sch + title` and a mean row.
- `--json`: `{page_count, non_uniform_count, mean:{coverage,citation,schema_compliance,density,overall}, non_uniform_pages:[{page_id,title,status,coverage,citation,density,overall}]}`.
- `non_uniform` verdict = any dimension < 1.0 (coverage/citation/schema_compliance/density/overall).

**Measured on 3 DBs**: all have `page_count>0`, `non_uniform=0`, and the bilingual copy truthfully reports the ceiling — the probe itself is verified.

## 4. Human annotation spec (A3)

### 4.1 Sampling unit (key design)
**Sample BOTH accepted and rejected/quarantined pages**, otherwise the ceiling recurs: sampling accepted pages only would make every human verdict should-publish=Y, so agreement is always 1.0 — no information (it just moves the scorer's zero variance into the human set).

- Scope: all candidate pages that entered the gate across compile tasks (including rejected/quarantined), stratified by status, 25–50 each, ≥50 total.
- Each page ships with its **source excerpt** (the source paragraphs/points it was compiled from) and its **compiled artifact** (the produced article + `[[ref:rN]]` citations), so a human can judge independently without seeing the scorer output.

### 4.2 Judgement dimensions
Primary (feeds the agreement rate):

- **should-publish (Y/N)**: given the source, does the page meet the quality floor to be published as a knowledge-base entry?

Secondary (to locate calibration, not fed into the primary κ):

- **coverage**: does it cover all key points of the source?
- **density**: are there padded / low-information-density stretches?
- **citation**: do key facts each carry a `[[ref]]` pointing at the source?
- **schema**: does the artifact follow the output contract (title/sections/body)?
- **consistency**: are there factual statements contradicting the source?

### 4.3 Judgement criteria (aligned with scorer rules)
The human grades each dimension in §4.2 as Y/N; any key dimension being N (especially coverage missing points / citation missing fact refs / density empty) means **should-publish=N**. The ruler is the same source as the scorer rules, so "agreement rate" means "does the same ruler produce the same verdict in different hands", not a comparison of two different standards.

### 4.4 Tooling & format
- Offline: annotation results as CSV/JSONL, one row per page: `page_id,status,human_should_publish,human_coverage,human_density,human_citation,human_schema,human_consistency,note`.
- **The annotator does not see the scorer output** (blinded review); the scorer `status` is merged in only after annotation.

## 5. Consistency-rate measurement pipeline (A3)

### 5.1 Flow
1. sample per §4.1 → fill the annotation sheet; 2. blinded review fills `human_should_publish`; 3. merge `status` (scorer) with `human_should_publish` → a 2×2 confusion matrix; 4. compute agreement + Cohen's κ; 5. compare against the §5.3 thresholds — pass if met, else go to 5.4 calibration.

### 5.2 Statistics
- **Agreement rate**: `(Y/Y + N/N) / total`.
- **Cohen's κ** `κ = (P_o − P_e) / (1 − P_e)`, `P_e` = expected agreement under the marginal probabilities of the two judges (corrects the within-class baseline agreement). κ ≥ 0.8 strong, 0.6–0.8 moderate, <0.6 needs calibration.

### 5.3 Verification thresholds (suggested)
- Gate passes when: agreement ≥ 0.85 **and** κ ≥ 0.6 (on a ≥50-page sample that includes rejected pages).
- Note: since it is a reproduction of "one rule ruler", expected agreement should be high; if it is instead low, the **rule is ambiguous or the extraction implementation has drifted from the rule description** — precisely what we want to expose.

### 5.4 Calibration actions (when not passing)
1. Locate the mismatches: `human=Y, status≠accepted` (false reject) or `human=N, status=accepted` (false accept).
2. False-reject dominant → tighten the scoring gate or add that dimension's rule; false-accept dominant → relax / re-audit the extraction implementation.
3. Re-run §5.3 until passing; record each calibration as a deviation (STEP14-P2-II-xxx).

## 6. Acceptance

- A2 probe landed: `wiktor quality` (human table + `--json`) truthfully reports the ceiling on the seed / mock-compile DBs (measured PASS).
- A3 spec ready: §4/§5 of this doc serve as the operating manual for annotation and agreement computation; the pipeline needs no new Rust code — data is an offline sheet + a one-off script (or derived from the `feedback`/`quality` read surfaces) to compute.
- Gates: workspace green + clippy `-D warnings` 0 + fmt clean.

## 7. Scope (explicitly out of scope)

- No "annotate 50–100 pages and compute correlation" (overturned by §1 ceiling; user chose Direction One).
- This doc only specs the consistency pipeline; **annotation execution is an offline process** — no automated annotation tool (automation is meaningless without real human annotators).
- The κ/agreement computation script, if later needed, can be added as a one-off tool; it is not part of this step's code changes.