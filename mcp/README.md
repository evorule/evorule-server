# evorule MCP adapter

Zero-dependency Model Context Protocol (MCP) server that exposes
evorule-server's deterministic computation services to any MCP client
(MCP host applications (desktop AI assistants and similar)).

- Transport: stdio (newline-delimited JSON-RPC), per the MCP standard
- Requires: Node >= 18 — nothing to install, no npm packages
- Tools: `list_services` (live catalog with contracts), `invoke_service`
  (deterministic compute call)

## Client configuration

MCP host applications (config example below uses the common `mcpServers` section):

```json
{
  "mcpServers": {
    "evorule": {
      "command": "node",
      "args": ["<path-to-evorule-server>/mcp/evorule-mcp.mjs"],
      "env": { "EVORULE_BASE": "http://127.0.0.1:18080" }
    }
  }
}
```

Any other MCP host: point a stdio server at `node evorule-mcp.mjs`
with `EVORULE_BASE` set to the evorule-server base URL (default
`http://127.0.0.1:18080`).

## Usage pattern

1. `list_services` — browse the catalog (optionally filter by
   `query` substring or `family`, e.g. `"gnss"`); each entry carries
   the service description and its `parameters` JSON-Schema contract.
2. Read the contract. Numbers are string-encoded decimals or `a/b`
   fractions — never JSON numbers. Contracts requiring `scale` and
   `rounding` expect both.
3. `invoke_service` with `{"service": "<name>", "args": {...}}`.
   Results carry `status: "determined"` and
   `determinism: "bitwise_replayable"` — same input, same output,
   bit for bit, independently re-computable.

Failures are fail-visible with self-diagnosing messages; surface them
verbatim. See
`skills/evorule-deterministic-compute/SKILL.md` for the full service
families, worked examples, and usage guidance.

## npm packaging

This directory carries a `package.json` (zero runtime dependencies, the
`bin` field points at `evorule-mcp.mjs`), so once published to the npm
registry clients can simply run `npx evorule-mcp` instead of cloning
this repository. Publishing is a maintainer action:
`cd mcp && npm publish`.
