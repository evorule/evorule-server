# Configuration

Configuration reference for evorule-server: loading priority is **CLI args > env vars (prefix `EVORULE_`) > JSON config file > built-in defaults**. Carried over from the repository README.

## Environment Variables / CLI Args

| Variable | CLI Arg | Default | Description |
| --- | --- | --- | --- |
| `EVORULE_CONFIG` | `--config` | (none) | JSON config file path |
| `EVORULE_ADDR` | `--addr` | `0.0.0.0:18080` | Listen address |
| `EVORULE_AUTH_TOKEN` | `--auth-token` | (empty) | Bearer token (empty = auth off, dev only) |
| `EVORULE_INSECURE_SERVE` | `--insecure-serve` | (empty) | Explicit no-auth acknowledgment (set to `1`; loopback binds only — non-loopback without token is rejected at startup) |
| `EVORULE_SERVICE_TOKEN` | `--service-token` | (empty) | Trusted service pipe token (service identity, can write to protected domains `stable.llm`/`stable.system`; effective independently of the auth switch — in no-auth mode it arms the protected-domain write guard) |
| `EVORULE_CORE_EVAL` | `--core-eval` | `./resources/server_eval.json` | Constitution file path (not hot-reloadable) |
| `EVORULE_RULES_DIR` | `--rules-dir` | `./rules` | Business rules directory (hot-reload monitored) |
| `EVORULE_DB_PATH` | `--db-path` | `./data/evorule.db` | SQLite database path |
| `EVORULE_MEMORY_DIR` | `--memory-dir` | `./data/memory` | Memory handler storage directory |
| `EVORULE_MAX_ROUNDS` | `--max-rounds` | `1000` | Reactor max instruction execution steps |
| `EVORULE_IO_WARN_TIMEOUT_SECS` | `--io-warn-timeout-secs` | `30` | Pending I/O warn threshold in seconds (per-session reactor; a warning fact is emitted when a pending `io_request` exceeds it) |
| `EVORULE_IO_ERROR_TIMEOUT_SECS` | `--io-error-timeout-secs` | `60` | Pending I/O error threshold in seconds (per-session reactor; an exceeding pending `io_request` fails with an explicit timeout error — raise for long-running multi-round application loops) |
| `EVORULE_SESSION_TTL_SECS` | `--session-ttl-secs` | `1800` | Session idle TTL in seconds (reaped when exceeded; must stay ≥ io-error-timeout + 60s margin) |
| `EVORULE_SESSION_REHYDRATE` | `--session-rehydrate` | `true` | Lazy same-id session resurrection: when a request hits an expired/restarted session and its WAL archive exists and passes chain validation, the session is rebuilt under the original id and history is preserved; archive-missing or corrupted still returns 404 |
| `EVORULE_LOG_LEVEL` | `--log-level` | `info` | Tracing level |
| `EVORULE_LOG_FORMAT` | `--log-format` | `plain` | Log format (`plain` / `json`) |
| `EVORULE_LOG_FILE` | `--log-file` | (empty) | Log file path (unset = stderr only) |
| `EVORULE_WAL_DIR` | `--wal-dir` | (empty) | WAL directory (enables persistence when set; unset triggers data-risk warning at startup) |
| `EVORULE_WAL_FSYNC` | `--wal-fsync` | `false` | fsync after each WAL write |
| `EVORULE_WAL_MAX_SIZE_MB` | `--wal-max-size-mb` | `100` | Max WAL file size (0 = no rotation) |
| `EVORULE_AUTO_VERIFY` | `--auto-verify` | `false` | Audit chain real-time verification |
| `EVORULE_NO_RATE_LIMIT` | `--no-rate-limit` | `false` | Disable rate limiting (benchmark only) |
| `EVORULE_ALLOWED_ORIGINS` | `--allowed-origins` | (empty) | CORS allowed Origin list (comma-separated; empty = same-origin only; `*` = all-open, dev only) |
| `EVORULE_WORKSPACE_DB` | `--workspace-db` | `./data/workspace.db` | Workspace metadata DB path (independent of business db_path) |
| `EVORULE_LOG_MAX_DAYS` | `--log-max-days` | `7` | Log file retention days |
| `EVORULE_LOG_MAX_SIZE_MB` | `--log-max-size-mb` | `1024` | Max log directory size (MB) |
| `EVORULE_AUTO_VERIFY_THRESHOLD` | `--auto-verify-threshold` | `1000` | Skip verify when audit entries exceed this (0 = unlimited) |
| `EVORULE_AUTO_VERIFY_INTERVAL` | `--auto-verify-interval` | `1` | Verify every N audit_new operations |
| `EVORULE_SERVICE_REGISTRY` | `--service-registry` | (empty) | service_name→URL mapping file (for call_service/call_external use); **if not configured, registry is empty and all external service calls return `unknown service_name`** |
| `EVORULE_PLUGINS` | `--plugins` | (empty) | Plugin manifest file (unconfigured = all native plugins/services enabled; see [Plugin Manifest](#plugin-manifest)) |
| `EVORULE_STATEMENT_WHITELIST` | `--statement-whitelist` | (empty) | SQL statement template whitelist file (unset = QUERY_DB all returns error) |
| `EVORULE_ALLOW_LOOPBACK` | `--allow-loopback` | `false` | Allow HTTP handler to access loopback addresses (local dev only, production forbidden) |
| `EVORULE_METRICS_AUTH` | `--metrics-auth` | `false` | Enable /metrics endpoint auth (requires Bearer token) |
| `EVORULE_OPENAPI_UI` | `--openapi-ui` | `false` | Mount Swagger UI (`GET /api/docs`; `/api/openapi.json` always available) |
| `EVORULE_ALLOW_ABORT` | `--allow-abort` | `false` | Enable force-abort session endpoint (`POST /api/sessions/{id}/abort`, default 404) |
| `EVORULE_DEMO_AUTH` | `--demo-auth` | `false` | Demo login toggle (on by default in quick-start packages; production recommends off) |
| `EVORULE_WEB_DIR` | `--web-dir` | (empty) | Static frontend hosting dir (SPA fallback index.html; unset = no hosting) |
| `EVORULE_PLUGIN_ADMIN_TOKEN__<ID>` | — | (empty) | Per-external-plugin admin token for the approval proxy (uppercase-underscore id, e.g. `EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG`; unset = proxy returns 503 for that plugin) |
| `EVORULE_DISCIPLINE_GATE` | — | `enforce` | L2 rule-shape discipline gate mode: `enforce` (reject on violation, default since the W1 observation-period exemption was closed 2026-10-14) / `warn` (shadow-enforce fallback, opt-in); invalid values fail closed (startup rejection) — see [Discipline Gate](#discipline-gate-l2-rule-shape-discipline) |

## Discipline Gate (L2 rule-shape discipline)

The server validates rule-set shape discipline (DC-01..DC-09) at two wiring points. Evaluation SSOT = the `evorule-discipline` crate (shares the same judge as evorule-cli); discipline data SSOT = `evorule-tcb` discipline core_eval.

- **Constitution loader** (`EVORULE_CORE_EVAL`): the constitution file previously had no gate at all (neither schema nor tier) — the discipline gate closes that gap. In `enforce` mode a violating constitution file fails startup (fail-fast, same semantics as the existing call_external constitution check).
- **Business rule loader** (`EVORULE_RULES_DIR`, hot-reload included): in `enforce` mode a violating rule file is rejected from loading (fail-soft per file, same semantics as schema/tier gates).

Modes (`EVORULE_DISCIPLINE_GATE`):

- `enforce` (default since 2026-10-14): violations reject loading/startup as described above. The W1 observation-period exemption (warn default) was closed after the review audit found zero observed violations across the observation window.
- `warn` (opt-in fallback, shadow-enforce): violating files still load (behavior unchanged), but every violation is logged at **ERROR** level (file / dc_code / node_type / node_path / reason + "would be rejected under enforce") and counted; after rule loading completes the server prints a one-shot shadow summary (violation file count; 0 = shadow period clean). One-knob rollback for production incidents — no release needed.
- Invalid values: fail closed (startup rejection with a corrective hint — a gate misconfiguration is a deployment incident and must not silently degrade into observation mode).

## JSON Config File

```bash
evorule-server --config evorule.json
```

```json
{
  "server": { "addr": "0.0.0.0:18080", "max_rounds": 1000 },
  "auth": { "token": "<your-bearer-token>" },
  "paths": {
    "core_eval": "./resources/server_eval.json",
    "rules_dir": "./rules",
    "db_path": "./data/evorule.db",
    "memory_dir": "./data/memory",
    "wal_dir": "./data/wal",
    "plugins": "./plugin_manifest.json"
  },
  "log": { "level": "info", "format": "json", "file": "./logs/evorule.log" }
}
```

If file doesn't exist or parse fails, falls back to pure CLI/env-var startup (warn log only, no error).

## Plugin Manifest

Plugins support **deployment-time enable/disable**: declare each plugin's enabled set via manifest file, change manifest + restart to take effect (no runtime hot-start/stop — runtime hot-change compatibility with deterministic audit chain is unproven). Manifest supports two entry types:

- **builtin entries** — in-process native plugins (`plugins/` sub-crates, e.g., `demo-services`, `physics-services`, `indicator-services`, `template-services`): `enabled` + optional `services` subset;
- **external entries** — external plugin packages (independent process + self-contained data + `plugin.json` manifest, any language implementation, install/uninstall with zero host-code changes): `enabled` + `manifest` pointing to plugin-package manifest; spec & development guide in [Plugin Development Guide](PLUGIN_GUIDE.md).

```bash
evorule-server --plugins ./plugin_manifest.json
```

Manifest format (multi-plugin, key = plugin id; builtin `services` omitted = all services for that plugin enabled, explicit list = subset enabled, unlisted plugins all-on; external `manifest` = plugin's plugin.json path, resolved relative to manifest file location):

```json
{
  "plugins": {
    "demo-services": {
      "enabled": true,
      "services": ["inverse_kinematics_solver", "llm_advisor", "robot_move_joints",
                   "shadow_ik_solver", "sampling_service", "rule_sandbox", "config_persist"]
    },
    "physics-services": {
      "enabled": true,
      "services": ["physics_simulate", "physics_energy", "physics_grav_band"]
    },
    "indicator-services": {
      "enabled": true,
      "services": ["indicator_sma", "indicator_ema", "indicator_macd", "indicator_rsi"]
    },
    "template-services": {
      "enabled": true,
      "services": ["template_render"]
    },
    "finance-config": {
      "enabled": true,
      "manifest": "plugins/finance-config/plugin.json"
    }
  }
}
```

Semantic conventions:

| Manifest form | Behavior |
|---|---|
| No `--plugins` configured (default) | All native plugins/services enabled; external plugin packages not loaded (explicit install semantic) — zero-migration for existing deployments |
| `enabled: true` + `services` omitted | All services for that plugin enabled (builtin) |
| `enabled: true` + `services` lists subset | Only listed services enabled; unlisted fall back to HTTP registry (`--service-registry`) |
| `enabled: true` + `manifest` pointing to plugin.json | Load external plugin package, declared services become route entries (same HTTP-fallback pipe as registry entries) |
| `enabled: false` | Don't mount this plugin, `call_service`/`call_external` goes straight to HTTP registry |

Validation is **fail-fast** (startup rejection, never silently ignored): unreadable manifest file, illegal JSON, unknown plugin id, empty `services`, unregistered/duplicate service names → error exit with self-diagnostic guidance (valid service name list, fix path). External entries also have 3 rejection checks: plugin.json unreadable/illegal JSON/id drift/empty service set/base_url non-http(s) refuses load; service name conflicts with builtin/registry/other external packages refuses load.

**Runtime visibility**: `GET /api/health` response includes `plugins` node showing actual mount facts at startup; external plugin nodes additionally carry runtime liveness status from the probe task (`status`: online / offline / no_probe, `last_probe`, `last_ok`, `last_error`) — a plugin going offline emits a `plugin_offline` alert event (recovery records `plugin_online` all-clear), probes with configurable period `--plugin-probe-interval` (default 30s, 0 = off); plugins without a `/health` endpoint report `no_probe` (presented as-is, no alert noise) —

```json
{
  "success": true,
  "message": "ok",
  "plugins": {
    "demo-services": { "enabled": true, "services": ["config_persist"] },
    "physics-services": { "enabled": true, "services": ["physics_energy"] },
    "indicator-services": { "enabled": true, "services": ["indicator_sma"] },
    "template-services": { "enabled": true, "services": ["template_render"] },
    "finance-config": { "enabled": true, "external": true, "services": ["finance_config_get", "finance_config_set"], "status": "online", "last_probe": 1788804515000, "last_ok": 1788804515000 }
  }
}
```

**Adding new native plugin/service** = add items to `NATIVE_SERVICES` declaration table in existing (or new) plugin crate + register declaration pointer in `src/main.rs` `PLUGIN_DEFS` registration table (manifest-parse/mount-chain/health-visibility mechanism code unchanged; router mechanism provided by [`core/plugin-kit`](../core/plugin-kit) public crate, plugins are thin-shell named delegates) — deployer enables/disables per manifest; see [plugins/demo-services/README.md](../plugins/demo-services/README.md), [plugins/physics-services/README.md](../plugins/physics-services/README.md), [plugins/indicator-services/README.md](../plugins/indicator-services/README.md), [plugins/template-services/README.md](../plugins/template-services/README.md).

**Adding new external plugin package** = implement independent HTTP service process + write plugin.json + one-line manifest registration (zero host-code changes, zero recompile, any language possible); spec/call-contract/management-surface/install-uninstall ops detailed in [Plugin Development Guide](PLUGIN_GUIDE.md). Existing non-packaged HTTP services connect directly via `--service-registry` declaration file.
