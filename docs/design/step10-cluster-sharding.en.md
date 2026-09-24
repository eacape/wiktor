# Step 10 Design: Cluster-Sharding Plan (design only, not implemented)

> Version: v1.0 (2026-09-24)
> Authority: `docs/MASTER-PLAN.md` v3.2 §14 decided, §17 landing dependency #9 (HA: litestream → Raft) and #10 (cluster sharding + plugin ecosystem)
> Nature: **plan-only**. Single-host 2 GiB hardware + Raft explicitly deferred (rqlite pattern, WAL as Raft log); this step builds no crate and writes no implementation. Chinese is authoritative; this English doc mirrors it.
> Related: `step10-plugin-ecosystem(.en).md` (the implemented part of this Step).

## 1. Why cluster sharding is plan-only for now

- **The current shape is a single writer** (SQLite single-writer + WAL + litestream replication). Sharding presupposes "multiple writable shards", which conflicts with the single-writer boundary; sharding early would tear apart the atomic publish, CAS, content hash, QUG, and publish state machine.
- **The deferred HA follows the rqlite pattern** (WAL as Raft log). Raft provides "multiple replicas of the same data + leader election", which is orthogonal to sharding (splitting data by key across nodes). The plan must make the seam explicit — do not mistake replication for sharding, or sharding for replication.
- Data scale: thousands of pages, tens of thousands of SKUs/facts. A single-machine SQLite + qdrant is ample; the motivation for sharding (capacity/throughput/ultra-large scale) is not yet present.

So #10's "cluster sharding" converges in this Step into an **executable evolution document** marked "design plan", not an implementation acceptance item.

## 2. What sharding is and is not (boundary clarification)

Not:
- Not multi-replica / primary-secondary (that is Raft / litestream's job: the same data stored in several places for disaster recovery + read scaling).
- Not making qdrant distributed (qdrant itself can be distributed; Wiktor's SQLite fact/knowledge planes are authoritative; vectors are derived indexes and should not share the shard scheme).

Is:
- Splitting the **fact/knowledge-plane data** by a shard key into multiple writable SQLite shards, each a separate single writer; a global query must merge results across shards.
- On the recall side, vector search in qdrant does centralized candidate recall + global filter; the SQLite shards only serve authoritative page/fact reads and writes — so sharding's main surface is **writes** (compile/publish) and **point/entity retrieval**, not full-text search.

This asymmetry (vectors centralized, SQLite distributed) is the core observation of this plan and directly determines the shard key and routing.

## 3. Shard-key selection

Candidates (entity_id / domain / write hotspot / atomicity):
| Key | Pros | Cons | Verdict |
|---|---|---|---|
| `domain` | naturally isolates domain packs; the atomic unit (compile/publish/QUG) is whole-domain, so it stays on one shard | a single domain could still outgrow one host | **first-level key** |
| entity_id hash | uniform distribution, linear capacity scaling | scatters one domain / one publish transaction; atomicity torn | **not the first-level key** |
| entity_id range | adjacent entities co-located | hot spots, balancing difficulty | not chosen |

**Decision: shard key = `domain` (first level), plus entity_id hash as a second level only when needed.** Reason: Wiktor's atomic unit is "one domain's one compile publish" (pages/scores/FTS5/facts in one SQLite transaction + two-phase qdrant sync); keeping all of a domain's data on one shard leaves atomicity, CAS, QUG publish, and content hash untouched. Only when a single domain exceeds one host's capacity do we hash within the domain by entity_id, accepting the complexity jump of "the domain's publish needs cross-shard coordination" (§6).

## 4. Routing

- Write routing: `server` receives a request → `shard_key(domain)` (or domain+hash) maps to a shard connection via a **static or dynamic `domain → shard` mapping table**.
- Read routing: full-text/vector recall happens centrally (qdrant), returning hit entities → fetch authoritative page/fact from each shard by the hit entity's domain; or each shard runs its own `QueryEngine` and the results are merged.
- Two merge strategies:
  - **Central recall + shard fetch** (recommended): qdrant returns top-k candidates (with domain), then fetch shards by domain concurrently. Bounds the result count and avoids full broadcast.
  - **Shard broadcast + merge**: each shard runs local retrieval and RRF/score-merges. Good when "every shard must have results", at the cost of N queries.

## 5. Rebalance

- Sharding is a **logical layer**: the `domain → shard` mapping table is managed by central metadata (a "catalog shard" today, or future Raft metadata).
- Rebalance = migrate one `domain`'s entire dataset (SQLite file/WAL + replay) to another shard, then atomically switch the mapping. Because the migration unit is "the whole domain", there is no cross-row data movement: rebalance is "snapshot + incremental catch-up + pointer switch", low complexity.
- `generation/epoch` already anchors content hash and publish versions; it doubles as the migration version anchor: freeze writes during migration (or dual-write catch-up), switch mapping once epochs align.

## 6. Cross-shard consistency (the boundary of cross-domain joins)

- **No cross-shard transactions**: keep "single-transaction atomic publish" within one domain only; all aggregate/join merging is query-side, never write-side. The docs must state plainly: cross-domain referential integrity is guaranteed by the upper layer (domain pack); SQLite never does distributed transactions across shards.
- The two-plane rebuildability promise is unchanged: the knowledge plane (Markdown) and fact plane (source JSONL) each rebuild fully; sharding is only physical distribution and does not change rebuild semantics.
- If a domain is later hashed internally by entity_id, that domain's compile/publish must be upgraded to "two-phase + coordinator" (per-shard local staged write, then a unified epoch commit) — the explicitly-marked complexity jump, not done by default.

## 7. Seam with the deferred Raft / rqlite

- **Raft owns replicas and election** (multiple replicas of the same data for DR + linear reads/writes); **sharding owns data splitting** (different data on different nodes). They compose: each shard can also be a Raft multi-replica group.
- Suggested landing path:
  1. Stay single-machine: single writer + litestream (delivered, Step 9).
  2. When DR/read scaling is needed: introduce the rqlite pattern, WAL as Raft log, multi-replica + automatic election (MASTER-PLAN #9 deferred).
  3. When write capacity must scale horizontally: only then do `domain`-level sharding, with a routing table + whole-domain migration; internal per-domain secondary sharding is far-future.
- Convention: **sharding lands no earlier than Raft**; while single-writer + Raft replication still meets scale, sharding only adds complexity without benefit.

## 8. Explicit non-implementations (this Step's boundary)

- No shard crate / no routing code / no shard configuration option.
- No change to the server's existing single-DB assembly path.
- No etcd/consul for metadata; the mapping table is abstracted as "future catalog shard or Raft metadata" — today it is only a written convention.

## 9. Acceptance

- This doc (EN + zh: `step10-cluster-sharding.md`) delivered, marked "design plan, not implemented".
- MASTER-PLAN #10 notes: cluster sharding = design plan; second domain pack / vector split / Meilisearch outlet = implemented.
- No code, no new crate, no test; the existing workspace 393+ test baseline is unaffected.

<!-- END STEP10 CLUSTER-SHARDING SPEC v1.0 -->