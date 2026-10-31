# API Reference

Reference for the HTTP API surface of evorule-server. The single source of truth is `GET /api/openapi.json` (Swagger UI requires `--openapi-ui`, served at `GET /api/docs`). This page is a manually curated summary of the main endpoint groups, carried over from the repository README.

## Health & Meta

| Path | Method | Description |
| --- | --- | --- |
| `/api/health` | GET | Health check (response includes plugin mount facts + external plugin liveness status) |
| `/api/health/liveness` | GET | Liveness probe (always 200) |
| `/api/health/readiness` | GET | Readiness probe (503 during shutdown) |
| `/api/plugins/{id}/admin/proposals` | GET | Plugin approval proxy: list pending proposals (passthrough of plugin admin surface) |
| `/api/plugins/{id}/admin/proposals/{pid}/approve` | POST | Plugin approval proxy: approve (approver force-injected from platform login identity) |
| `/api/plugins/{id}/admin/proposals/{pid}/reject` | POST | Plugin approval proxy: reject (reason kept from caller) |
| `/api/plugins/{id}/admin/audit` | GET | Plugin approval proxy: audit history (symmetric approve/reject trail, `key`/`limit` query passthrough) |
| `/api/openapi.json` | GET | OpenAPI doc (single source of truth) |

## Session & Execution

| Path | Method | Description |
| --- | --- | --- |
| `/api/sessions` | POST/GET | Create / list sessions |
| `/api/sessions/{id}` | GET/DELETE | Detail / close |
| `/api/sessions/{id}/command` | POST | Submit command |
| `/api/sessions/{id}/io_response` | POST | Respond to external I/O request |
| `/api/sessions/{id}/payload` | POST | Update payload |
| `/api/sessions/{id}/state` | GET | Current state snapshot |
| `/api/sessions/{id}/snapshot` | GET | Full snapshot |
| `/api/sessions/{id}/events` | GET (SSE) | Event stream (includes heartbeat) |
| `/api/sessions/{id}/finished` | GET | Whether session has terminated |
| `/api/sessions/reap` | POST | Reap terminated sessions |
| `/api/command` `/api/payload` `/api/state` `/api/audit` | POST/GET | Single-reactor convenience endpoints (backward-compatible, no session id needed) |

## Time Machine

| Path | Method | Description |
| --- | --- | --- |
| `/api/sessions/{id}/rewind` | GET | Time rewind (`?version=N`) |
| `/api/sessions/{id}/diff` | GET | Point-in-time comparison (`?a=N&b=M`; unreachable version = 400) |
| `/api/sessions/{id}/history` | GET | Version history |
| `/api/sessions/{id}/replay` | GET | Replay |
| `/api/sessions/fork/{parent_id}` | POST | Time-point branch (`?version=N`) |
| `/api/sessions/from/{parent_id}` | POST | Fork from parent session |

## Debug Control

| Path | Method | Description |
| --- | --- | --- |
| `/api/sessions/{id}/debug/phase` | GET | Current phase |
| `/api/sessions/{id}/debug/queue` | GET | Queue state |
| `/api/sessions/{id}/debug/pending_io` | GET | Pending I/O |
| `/api/sessions/{id}/pending_io_count` | GET | Pending I/O count |
| `/api/sessions/{id}/step` | GET | Single-step execution |
| `/api/sessions/{id}/interrupt` | POST | Interrupt reactor |
| `/api/sessions/{id}/abort` | POST | Force abort (**default 404**, requires `--allow-abort` explicit enable) |
| `/api/sessions/{id}/invariants` | GET | Semantic invariant self-check result |
| `/api/sessions/{id}/causal_depth` | GET | Causal depth |

## Audit

| Path | Method | Description |
| --- | --- | --- |
| `/api/sessions/{id}/audit` | GET | Audit report (supports `include_content`) |
| `/api/sessions/{id}/audit/verify` | GET | Verify audit chain integrity |
| `/api/sessions/{id}/audit/export` | GET | Export audit chain JSON (also `/compressed`) |
| `/api/sessions/{id}/audit/import` | POST | Import audit chain (also `/compressed`) |
| `/api/sessions/{id}/audit/auto_verify` | GET | Auto-verify status |
| `/api/sessions/{id}/audit/causal/{fact_id}` | GET | Causal chain trace |
| `/api/audit-archive/sessions` | GET | Audit archive: rebuild historical session list from WAL (read-only) |
| `/api/audit-archive/sessions/{id}/audit` | GET | Audit archive: historical session audit chain |
| `/api/audit/platform-events` | GET | Platform auth event report (read-only derived fact chain) |

## Facts & Shared Facts

| Path | Method | Description |
| --- | --- | --- |
| `/api/sessions/{id}/facts` | GET | Query session facts by prefix |
| `/api/shared/facts` | GET | Shared facts query |
| `/api/shared/facts/version` | GET | Shared facts version |
| `/api/shared/facts/rollup` | POST | Shared facts rollup |
| `/api/shared/facts/{fact_id}/source` `/used_by` | GET | Fact source / consumer trace |
| `/api/sessions/{id}/used_at_startup` | GET | Facts used at startup |

## Platform Auth / Users / Roles

| Path | Method | Description |
| --- | --- | --- |
| `/api/platform/auth/bootstrap` | POST | Bootstrap create admin (409 if users exist) |
| `/api/platform/auth/login` `/logout` | POST | Login / logout |
| `/api/platform/auth/me` `/status` | GET | Current user / auth status |
| `/api/platform/auth/change-password` | POST | Change password |
| `/api/platform/users` `/users/{username}` | GET/POST/PATCH/DELETE | User management |
| `/api/platform/roles` `/roles/{name}` | GET/POST/… | Role management |
| `/api/platform/permissions` | GET | Permission point registry |
| `/api/platform/apps` `/apps/{id}/revoke` | GET/POST | App credential management (issue/list/revoke, manage_apps) |
| `/api/platform/apps/{id}/quota` | POST | Update app quotas (full-overwrite, null = unlimited, manage_apps) |

## Permissions / Rule Packages / Rules

| Path | Method | Description |
| --- | --- | --- |
| `/api/permissions` | GET/POST | Permission list / create |
| `/api/permissions/evaluate` | POST | Permission evaluation |
| `/api/permissions/{id}/submit` `/review` | POST | Submit / review |
| `/api/bundles/import` | POST | Import rule package (6 validations + atomic commit) |
| `/api/bundles/import/dry-run` | POST | Import dry-run (validate only, no commit) |
| `/api/bundles/active` | GET | Currently active rule package |
| `/api/bundles/imports` | GET | Import history |
| `/api/rules` | GET | Current rule set |
| `/api/rules/l2-inventory` | GET | Read-only L2 constraint (meta) rule inventory (`count` + `files[path/title/guard_for]`, sorted by filename, gate-failing files excluded) |
| `/api/rules/validate` | POST | Rule validation |
| `/api/rules/reload` | POST | Hot reload |
| `/api/rules/hit-stats` | GET | Rule hit statistics list (`filter=all/hit/zero`, specify `version`) |
| `/api/rules/hit-stats/{rule_key}` | GET | Per-rule cross-version hit slice (`rule_key`=`{index}@{source}`) |

## Rule Hit Statistics

Runtime hit signals: aggregator consumes audit chain hit-attribution facts, aggregated by `rule-set version × source × rule index`; query responses carry a `rule_ref` attribution projection (`source#same-source ordinal`, e.g. `00_constraint_promoted_xxx.json#0`, out-of-range falls back to `rule_index=N`) — promoted constraint files can be located directly without manual index conversion.

- Stored as process-memory state, **resets to zero after restart**; audit chain WAL is the authoritative full source
- Any change to rule-set content or source produces a new version number; post-reload historical version slices retained (max 8)
- **Zero-hit list** (`filter=zero`) = dead rule candidates — silent-pass rules' data source for cleanup

Prometheus metrics (`/metrics`):

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `evorule_rule_hits_total` | counter | `rule`(index) / `source`(`core_eval` or relative path) / `version` | Rule-structure hit count (direct instruction exec success / selected branch exists non-empty / io_request produces signal) |
| `evorule_rules_zero_hits` | gauge | — | Current version zero-hit rule count |

Example PromQL:

```promql
# Active rule hit rate (by source)
rate(evorule_rule_hits_total[5m])
# Current version dead rule count (zero-hit gauge)
evorule_rules_zero_hits
```

> Note: never-hit rules have **no time series** in `evorule_rule_hits_total` (counter only created on hit); complete dead-rule list via query endpoint `filter=zero`.

Capacity note: audit chain produces +1 hit-attribution fact per command (recordkeeping, does not advance version number); per-entry normal size <1KB; WAL rotation follows `--wal-max-size-mb`.

## Data & Services

| Path | Method | Description |
| --- | --- | --- |
| `/api/knowledge` | GET | Dataset list (execution-side read-only channel) |
| `/api/knowledge/{ds}/entries` `/{entry_id}` | GET | Entry query |
| `/api/services` | GET | Bound services list (native / plugin / registry, 3 sources, with param contracts) |
| `/api/export/pdf` | POST | Server-side PDF export (pure Rust text-mode, Chinese font subset embedded; body limit 32MB) |
| `/api/templates/render` | POST | Deterministic template rendering: context + template → JSON / Markdown / plain text (`{{}}` family syntax, if/for minimal set) |
| `/api/marketplace/templates` | GET/POST | Template marketplace: list / upload |
| `/api/marketplace/templates/{id}` | GET/PATCH/DELETE | Template detail / online edit / delete |
| `/api/marketplace/templates/{id}/download` | GET | Template download |
| `/api/workspaces` family | GET/POST/PATCH/DELETE | Multi-tenant workspace + members + rule CRUD / version / activate / sandbox / publish queue (dual-state nomination dedup gate on submit: same workspace × same constraint-layer target blocked while pending **or** published; passes when `promoted_from` version set changes; `GET /api/publish/queue?workspace_id=` filter) |
| `/metrics` | GET | Prometheus metrics (requires Bearer when `--metrics-auth`) |

## Further Reading

- [Integration Guide](INTEGRATION_GUIDE.md) — I/O handler architecture, session lifecycle, complete audit chain usage, rule-writing practical tips, local dev environment setup
- [Pitfalls & Gotchas Guide](PITFALLS.md) — 15 real integration pitfalls (I/O timeout / SSRF / HTTP header semantics / rule path / TCB type limits etc.), each with symptom, root cause, fix
