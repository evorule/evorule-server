// SPDX-License-Identifier: AGPL-3.0-or-later
// codegraph-bridge: HTTP sidecar that exposes @colbymchenry/codegraph
// as an evorule-server external plugin.
//
// Routes:
//   GET  /health              -> liveness probe (plugin_probe.rs contract)
//   POST /services/{name}     -> body = args JSON, dispatched to a codegraph tool
//
// Per-request multi-project: every service accepts `projectPath` in args;
// the bridge lazily opens/caches one CodeGraph instance per project root.

import http from 'node:http';
import path from 'node:path';
import fs from 'node:fs';

const HOST = process.env.CODEGRAPH_BRIDGE_HOST || '127.0.0.1';
const PORT = Number(process.env.CODEGRAPH_BRIDGE_PORT || 9150);
const MAX_INSTANCES = 16;
const MAX_OUTPUT = 15000;
// 调用方令牌（对齐 ai-plugin caller_token 模式）：env 设置后 /services/* 逐
// 请求校验 `Authorization: Bearer <token>`；未设置 = 不武装（现行为不变）。
// server 侧契约 v1.3：plugin.json auth_token 声明后自动携带同名头。
// /health 保持无鉴权（plugin_probe 探活契约）。
const TOKEN = process.env.CODEGRAPH_BRIDGE_TOKEN || null;

// Lazy-loaded (ESM default export)
let CodeGraph = null;
let isInitializedLib = false;

async function loadCodeGraph() {
  if (isInitializedLib) return;
  const mod = await import('./vendor/codegraph/dist/index.js');
  // CJS/ESM interop: tsc emits `exports.default = CodeGraph`; Node wraps it
  // as mod.default.default. Some builds also attach named exports directly.
  CodeGraph = mod.default?.default || mod.default || mod.CodeGraph;
  isInitializedLib = true;
}

// ---- Instance pool -------------------------------------------------------
// key = resolved absolute project root
const pool = new Map(); // resolvedPath -> { cg, lastUsed }

function evictIfNeeded() {
  while (pool.size >= MAX_INSTANCES) {
    // evict least-recently-used
    let oldestKey = null;
    let oldestTime = Infinity;
    for (const [k, v] of pool) {
      if (v.lastUsed < oldestTime) { oldestTime = v.lastUsed; oldestKey = k; }
    }
    if (!oldestKey) break;
    try { pool.get(oldestKey).cg.close(); } catch {}
    pool.delete(oldestKey);
  }
}

// ---- Index lock -----------------------------------------------------------
// Per-project 索引重建互斥：ensure_index（init/indexAll/sync，数十秒级）全程
// 持锁，读路径 getOrOpen 打开前等待同路径在途重建完成——防并发读命中半成品
// 索引（readOnly 不损坏库,但会读到不完整符号集）。重建失败不阻塞后续读
// （等待吞掉错误,打开时自然报真实故障）。
const indexing = new Map(); // resolvedPath -> Promise

async function waitForIndexing(resolved) {
  const job = indexing.get(resolved);
  if (job) {
    try { await job; } catch {}
  }
}

function withIndexLock(resolved, fn) {
  const prev = indexing.get(resolved) || Promise.resolve();
  const job = (async () => {
    await prev;
    return fn();
  })();
  indexing.set(resolved, job);
  return job.finally(() => {
    if (indexing.get(resolved) === job) indexing.delete(resolved);
  });
}

async function getOrOpen(projectPath) {
  if (!projectPath || typeof projectPath !== 'string') {
    throw new Error('projectPath is required (absolute path to a repo with .codegraph/ initialized)');
  }
  const resolved = path.resolve(projectPath);
  await waitForIndexing(resolved);
  if (pool.has(resolved)) {
    const entry = pool.get(resolved);
    entry.lastUsed = Date.now();
    return entry.cg;
  }
  if (!fs.existsSync(resolved)) {
    throw new Error(`projectPath does not exist: ${resolved}`);
  }
  if (!CodeGraph.isInitialized(resolved)) {
    throw new Error(
      `CodeGraph not initialized at ${resolved}. Call service 'codegraph_ensure_index' first.`
    );
  }
  evictIfNeeded();
  const cg = await CodeGraph.open(resolved, { sync: false, readOnly: true });
  pool.set(resolved, { cg, lastUsed: Date.now() });
  return cg;
}

// ---- Symbol resolution (mirrors tools.ts findAllSymbols, simplified) ------
function lastQualifierPart(symbol) {
  const parts = symbol.split(/::|[./]/).filter(p => p.length > 0);
  return parts[parts.length - 1] || symbol;
}

async function findNode(cg, symbol) {
  let results = cg.searchNodes(symbol, { limit: 50 });
  if (results.length === 0 && /[./]|::/.test(symbol)) {
    const tail = lastQualifierPart(symbol);
    if (tail && tail !== symbol) results = cg.searchNodes(tail, { limit: 50 });
  }
  if (results.length === 0) return null;
  const exact = results.filter(r => r.node.name === symbol);
  return (exact[0] || results[0]).node;
}

function trim(s) {
  if (typeof s !== 'string') return s;
  if (s.length <= MAX_OUTPUT) return s;
  return s.slice(0, MAX_OUTPUT) + '\n\n... (truncated)';
}

// ---- Handlers: each takes args object, returns JSON-serializable value ----
const handlers = {
  async codegraph_search(args) {
    const cg = await getOrOpen(args.projectPath);
    const limit = Math.min(Number(args.limit) || 10, 100);
    const results = cg.searchNodes(args.query, {
      limit,
      kinds: args.kind ? [args.kind] : undefined,
    });
    return {
      count: results.length,
      results: results.map(r => ({
        name: r.node.name, kind: r.node.kind,
        file: r.node.filePath, line: r.node.startLine,
        signature: r.node.signature || null,
      })),
    };
  },

  async codegraph_context(args) {
    const cg = await getOrOpen(args.projectPath);
    const ctx = await cg.buildContext(args.task, {
      maxNodes: Number(args.maxNodes) || 20,
      includeCode: args.includeCode !== false,
      format: 'markdown',
    });
    return { context: trim(typeof ctx === 'string' ? ctx : JSON.stringify(ctx)) };
  },

  async codegraph_callers(args) {
    const cg = await getOrOpen(args.projectPath);
    const node = await findNode(cg, args.symbol);
    if (!node) return { found: false, callers: [] };
    const seen = new Set(); const out = [];
    for (const c of cg.getCallers(node.id, 1)) {
      if (!seen.has(c.node.id)) { seen.add(c.node.id); out.push(c.node); }
    }
    const limit = Math.min(Number(args.limit) || 20, 100);
    return {
      found: true, symbol: args.symbol,
      callers: out.slice(0, limit).map(n => ({
        name: n.name, kind: n.kind, file: n.filePath, line: n.startLine,
      })),
    };
  },

  async codegraph_callees(args) {
    const cg = await getOrOpen(args.projectPath);
    const node = await findNode(cg, args.symbol);
    if (!node) return { found: false, callees: [] };
    const seen = new Set(); const out = [];
    for (const c of cg.getCallees(node.id, 1)) {
      if (!seen.has(c.node.id)) { seen.add(c.node.id); out.push(c.node); }
    }
    const limit = Math.min(Number(args.limit) || 20, 100);
    return {
      found: true, symbol: args.symbol,
      callees: out.slice(0, limit).map(n => ({
        name: n.name, kind: n.kind, file: n.filePath, line: n.startLine,
      })),
    };
  },

  async codegraph_impact(args) {
    const cg = await getOrOpen(args.projectPath);
    const node = await findNode(cg, args.symbol);
    if (!node) return { found: false, nodes: [] };
    const depth = Math.min(Number(args.depth) || 2, 10);
    const impact = cg.getImpactRadius(node.id, depth);
    const nodes = [...impact.nodes.values()];
    return {
      found: true, symbol: args.symbol, depth,
      affectedCount: nodes.length,
      nodes: nodes.slice(0, 100).map(n => ({
        name: n.name, kind: n.kind, file: n.filePath, line: n.startLine,
      })),
    };
  },

  async codegraph_node(args) {
    const cg = await getOrOpen(args.projectPath);
    const node = await findNode(cg, args.symbol);
    if (!node) return { found: false };
    const out = {
      found: true, name: node.name, kind: node.kind,
      file: node.filePath, line: node.startLine, endLine: node.endLine,
      signature: node.signature || null, docstring: node.docstring || null,
    };
    if (args.includeCode) out.code = trim(await cg.getCode(node.id));
    return out;
  },

  async codegraph_explore(args) {
    // First version: delegate to buildContext (closest semantic equivalent)
    const cg = await getOrOpen(args.projectPath);
    const ctx = await cg.buildContext(args.query, {
      maxNodes: Math.min(Number(args.maxFiles) || 12, 20),
      includeCode: true,
      format: 'markdown',
    });
    return { context: trim(typeof ctx === 'string' ? ctx : JSON.stringify(ctx)) };
  },

  async codegraph_files(args) {
    const cg = await getOrOpen(args.projectPath);
    const files = cg.getFiles();
    return { count: files.length, files: files.slice(0, 500).map(f => f.path || f) };
  },

  async codegraph_status(args) {
    const cg = await getOrOpen(args.projectPath);
    const stats = cg.getStats();
    return {
      projectRoot: cg.getProjectRoot(),
      fileCount: stats.fileCount, nodeCount: stats.nodeCount, edgeCount: stats.edgeCount,
      journalMode: cg.getJournalMode(),
      backend: cg.getBackend(),
    };
  },

  async codegraph_ensure_index(args) {
    if (!args.projectPath) throw new Error('projectPath is required');
    const resolved = path.resolve(args.projectPath);
    if (!fs.existsSync(resolved)) throw new Error(`projectPath does not exist: ${resolved}`);
    // 全程持 per-project 索引锁（读路径 waitForIndexing 等待;防并发读半成品）
    return withIndexLock(resolved, async () => {
      if (!CodeGraph.isInitialized(resolved) || args.force) {
        if (pool.has(resolved)) { try { pool.get(resolved).cg.close(); } catch {} pool.delete(resolved); }
        if (CodeGraph.isInitialized(resolved) && args.force) {
          // re-index: open existing and indexAll
          const cg = await CodeGraph.open(resolved, { readOnly: false });
          await cg.indexAll({});
          cg.close();
        } else {
          const cg = await CodeGraph.init(resolved, { index: true });
          cg.close();
        }
        return { ok: true, action: args.force ? 'reindexed' : 'initialized+indexed', projectRoot: resolved };
      }
      // already initialized: quick sync
      const cg = await CodeGraph.open(resolved, { sync: false, readOnly: false });
      const syncResult = await cg.sync();
      cg.close();
      return { ok: true, action: 'synced', projectRoot: resolved, ...syncResult };
    });
  },
};

// ---- HTTP server ---------------------------------------------------------
function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', c => chunks.push(c));
    req.on('end', () => {
      if (!chunks.length) return resolve({});
      try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); }
      catch (e) { reject(new Error('invalid JSON body: ' + e.message)); }
    });
    req.on('error', reject);
  });
}

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === 'GET' && req.url === '/health') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      return res.end(JSON.stringify({ status: 'ok', instances: pool.size }));
    }
    const m = req.method === 'POST' && req.url.match(/^\/services\/([\w-]+)\/?$/);
    if (m) {
      // 武装态逐请求校验（fail-visible 401;未武装 = 现行为不变）
      if (TOKEN) {
        const present = req.headers['authorization'] || '';
        if (present !== `Bearer ${TOKEN}`) {
          res.writeHead(401, { 'Content-Type': 'application/json' });
          return res.end(JSON.stringify({ error: 'unauthorized (Bearer token mismatch)' }));
        }
      }
      const name = m[1];
      const handler = handlers[name];
      if (!handler) {
        res.writeHead(404, { 'Content-Type': 'application/json' });
        return res.end(JSON.stringify({ error: `unknown service: ${name}` }));
      }
      const args = await readBody(req);
      const result = await handler(args);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      return res.end(JSON.stringify(result));
    }
    res.writeHead(404, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'not found' }));
  } catch (e) {
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: e.message || String(e) }));
  }
});

await loadCodeGraph();
server.listen(PORT, HOST, () => {
  console.error(`[codegraph-bridge] listening on http://${HOST}:${PORT}`);
});
