#!/usr/bin/env node
// evorule MCP adapter — zero-dependency Model Context Protocol server
// (stdio transport, newline-delimited JSON-RPC) exposing evorule-server
// native computation services as two tools:
//
//   list_services {query?, family?}  — live catalog with contracts
//   invoke_service {service, args}   — deterministic compute call
//
// Requires Node >= 18 (global fetch). Configuration:
//   EVORULE_BASE (default http://127.0.0.1:18080)
//
// Client configuration example (common mcpServers section of an MCP host):
//   {"mcpServers": {"evorule": {
//     "command": "node",
//     "args": ["<repo>/mcp/evorule-mcp.mjs"],
//     "env": {"EVORULE_BASE": "http://127.0.0.1:18080"}
//   }}}
//
// The services themselves are documented in
// skills/evorule-deterministic-compute/SKILL.md.

import { createInterface } from "node:readline";

const EVORULE_BASE = (process.env.EVORULE_BASE ?? "http://127.0.0.1:18080").replace(/\/+$/, "");
const SERVER_INFO = { name: "evorule-mcp", version: "1.0.0" };
const PROTOCOL_FALLBACK = "2024-11-05";

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

function textResult(id, text, isError = false) {
  const content = [{ type: "text", text }];
  const result = isError ? { content, isError: true } : { content };
  send({ jsonrpc: "2.0", id, result });
}

async function fetchServices() {
  const res = await fetch(`${EVORULE_BASE}/api/services`);
  if (!res.ok) throw new Error(`GET /api/services -> HTTP ${res.status}`);
  const body = await res.json();
  // 响应形态：裸数组（历史版本曾为 {list:[...]}，两者兼容处理）。
  return Array.isArray(body) ? body : (body.list ?? []);
}

function formatCatalog(entries, query, family) {
  let list = entries;
  if (family) {
    const f = family.toLowerCase();
    list = list.filter((s) => (s.plugin ?? "").toLowerCase().includes(f));
  }
  if (query) {
    const q = query.toLowerCase();
    list = list.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        (s.description ?? "").toLowerCase().includes(q),
    );
  }
  if (list.length === 0) return "No services match. Use list_services without filters to see the full catalog.";
  const lines = list.map((s) => {
    const head = `- ${s.name} [${s.plugin ?? "?"}] — ${s.description ?? "(no description)"}`;
    if (!s.parameters) return head;
    const contract =
      typeof s.parameters === "string" ? s.parameters : JSON.stringify(s.parameters);
    return `${head}\n  contract: ${contract}`;
  });
  return `${list.length} service(s):\n\n${lines.join("\n")}`;
}

async function toolListServices(args) {
  const entries = await fetchServices();
  return formatCatalog(entries, args?.query, args?.family);
}

async function toolInvokeService(args) {
  const service = args?.service;
  if (!service || typeof service !== "string") {
    throw new Error("Missing required argument: service (string)");
  }
  const invokeArgs = args?.args && typeof args.args === "object" ? args.args : {};
  const res = await fetch(`${EVORULE_BASE}/api/services/${encodeURIComponent(service)}/invoke`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(invokeArgs),
  });
  const body = await res.text();
  if (!res.ok) {
    return {
      text: `Service ${service} failed (HTTP ${res.status}). The server message is fail-visible with self-diagnosis; surface it to the user verbatim:\n${body}`,
      isError: true,
    };
  }
  let pretty = body;
  try {
    pretty = JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    // keep raw text
  }
  return `Service ${service} result (status field inside describes determinism):\n${pretty}`;
}

const TOOLS = [
  {
    name: "list_services",
    description:
      "List evorule deterministic computation services with their parameter contracts. Filter by substring query or plugin family. Always consult the contract before invoking an unfamiliar service.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Substring filter over service name and description" },
        family: { type: "string", description: "Plugin family filter (e.g. gnss, numeric, grav, fluid, quantum, metrology, checksum, cny)" },
      },
    },
  },
  {
    name: "invoke_service",
    description:
      "Invoke an evorule deterministic computation service. Results are exact and bit-replayable (envelope: status=determined, determinism=bitwise_replayable). Numbers must be string-encoded decimals or a/b fractions — never JSON numbers. Use list_services first to read the service's parameter contract.",
    inputSchema: {
      type: "object",
      properties: {
        service: { type: "string", description: "Service name, e.g. gnss_range_error" },
        args: { type: "object", description: "Arguments per the service's parameters contract" },
      },
      required: ["service"],
    },
  },
];

async function callTool(name, args) {
  switch (name) {
    case "list_services":
      return { text: await toolListServices(args) };
    case "invoke_service":
      return await toolInvokeService(args);
    default:
      return null;
  }
}

async function handle(id, method, params) {
  if (method === "initialize") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        protocolVersion: params?.protocolVersion ?? PROTOCOL_FALLBACK,
        capabilities: { tools: { listChanged: false } },
        serverInfo: SERVER_INFO,
      },
    });
    return;
  }
  if (method === "ping") {
    send({ jsonrpc: "2.0", id, result: {} });
    return;
  }
  if (method === "tools/list") {
    send({ jsonrpc: "2.0", id, result: { tools: TOOLS } });
    return;
  }
  if (method === "tools/call") {
    const name = params?.name;
    try {
      const out = await callTool(name, params?.arguments);
      if (out === null) {
        textResult(id, `Unknown tool: ${name}`, true);
      } else if (typeof out === "string") {
        textResult(id, out);
      } else {
        textResult(id, out.text, out.isError ?? false);
      }
    } catch (err) {
      textResult(
        id,
        `evorule adapter error (is the server running at ${EVORULE_BASE}?): ${err?.message ?? err}`,
        true,
      );
    }
    return;
  }
  if (id !== undefined) {
    send({ jsonrpc: "2.0", id, error: { code: -32601, message: `Method not found: ${method}` } });
  }
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let msg;
  try {
    msg = JSON.parse(trimmed);
  } catch {
    return; // not JSON — ignore
  }
  const { id, method, params } = msg ?? {};
  if (!method) return;
  // notifications (no id) get no response; initialized etc. fall through to handle()
  handle(id, method, params).catch((err) => {
    if (id !== undefined) {
      send({
        jsonrpc: "2.0",
        id,
        error: { code: -32603, message: `Internal error: ${err?.message ?? err}` },
      });
    }
  });
});
rl.on("close", () => process.exit(0));
