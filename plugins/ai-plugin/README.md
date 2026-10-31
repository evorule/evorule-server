# ai-plugin

Local plugin executable for evo-agent: LLM relay endpoints, a web search relay, and browser automation execution. Runs as a standalone process next to the local evorule server.

## Run

```
evorule-ai-plugin [--config <ai-plugin.json>]
```

Config lookup order: `--config` path → `<exe directory>/ai-plugin.json` → `<working directory>/ai-plugin.json`. See [`config.example.json`](config.example.json) for a full annotated template.

## Configuration

Required (startup fails fast without them):

| Field | Semantics |
|---|---|
| `server_base_url` | Base URL of the local evorule server (http/https) |
| `llm_endpoint` | LLM provider endpoint (http/https) |
| `llm_model` | Default model name for the chat relay |
| LLM credential | Exactly one of: env `EVORULE_AI_PLUGIN_LLM_API_KEY` (recommended) or `llm_api_key` in config |

Optional:

| Field | Default | Semantics |
|---|---|---|
| `listen_addr` | `127.0.0.1:9130` | HTTP bind address |
| `server_auth_token` | unset | When set, callers must present this Bearer token on plugin endpoints |
| `llm_temperature` | `0.2` | Must be within [-2.0, 2.0] |
| `llm_execute_url` + `model_allowlist_path` | unset | Extended executor endpoint (full URL form); when set, the model allowlist file is mandatory (fail-closed) |
| `llm_providers` | unset | Multi-provider routing for `/llm/execute`; models must be covered by the allowlist material |
| `web_search_provider` | unset | `brave` or `tavily`; unset = web search relay disabled |

## Web search relay

`POST /tools/web_search` with `{"query": "...", "count": 5}` (`count` defaults to 5, capped at 10).

- Provider whitelist: `brave`, `tavily`. Unset provider → requests fail with 500 and a self-diagnosis message naming the missing configuration.
- Credentials resolve from environment first — `EVORULE_AI_PLUGIN_BRAVE_API_KEY` / `EVORULE_AI_PLUGIN_TAVILY_API_KEY` — then fall back to the `brave_api_key` / `tavily_api_key` config keys. Environment variables are recommended: secrets stay off disk.
- Keys are held only by this plugin process; callers never hold them. Secrets never appear in logs or error messages.
- Responses are normalized to `{status, provider, query, result_count, results: [{title, url, description}]}`.
- Upstream failures surface as 502 with the cause: 401 (invalid key), 429 (rate limited), or other upstream errors.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Liveness probe |
| POST | `/services/ai_plugin_chat` | Chat relay |
| POST | `/llm/execute` | LLM execution (single endpoint or provider-routed) |
| POST | `/llm/execute-stream` | Streaming LLM execution |
| GET | `/llm/models` | Model listing |
| POST | `/tools/web_search` | Web search relay |
