"use strict";
/**
 * Parse Worker
 *
 * Runs tree-sitter parsing in a separate thread so the main thread
 * stays unblocked and the UI animation renders smoothly.
 */
Object.defineProperty(exports, "__esModule", { value: true });
const worker_threads_1 = require("worker_threads");
const tree_sitter_1 = require("./tree-sitter");
const grammars_1 = require("./grammars");
// Emscripten prints `Aborted()` (and a follow-up RuntimeError diag
// line) directly to stderr when WASM aborts — before the JS catch
// runs. Worker stderr is inherited by the parent, so each crash leaks
// a noise line to the user's terminal even though the JS layer
// already handles the failure cleanly. Filter these specific lines
// out at the source. Real diagnostic output (anything we log
// ourselves) goes through console.* / parentPort and is unaffected.
//
// Caveats deliberately accepted:
//   - Per-call match: each `write()` call is matched in isolation.
//     If Emscripten ever splits `Aborted(` across two write()s (it
//     doesn't today — synchronous abort prints the whole line at
//     once via libc puts) the first fragment would leak. Buffering
//     across calls would add complexity for a hypothetical case.
//   - Substring exactness: the prefix `Aborted(` is the literal
//     Emscripten signature. Any user code that legitimately writes
//     a stderr line starting with that prefix would also be filtered;
//     in practice no real diagnostic does.
{
    const realWrite = process.stderr.write.bind(process.stderr);
    process.stderr.write = ((chunk, encoding, cb) => {
        const s = typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf-8');
        if (s.startsWith('Aborted(') ||
            s.includes('Build with -sASSERTIONS for more info')) {
            // Honour the Writable stream contract: callbacks must always
            // fire even when the write is suppressed, or upstream code
            // waiting on the drain signal would hang. Both overload forms
            // are handled (`(chunk, cb)` and `(chunk, encoding, cb)`).
            if (typeof encoding === 'function')
                encoding();
            else if (cb)
                cb();
            return true;
        }
        return realWrite(chunk, encoding, cb);
    });
}
const PARSER_RESET_INTERVAL = 5000;
const parseCounts = new Map();
worker_threads_1.parentPort.on('message', async (msg) => {
    if (msg.type === 'load-grammars') {
        await (0, grammars_1.loadGrammarsForLanguages)(msg.languages);
        worker_threads_1.parentPort.postMessage({ type: 'grammars-loaded' });
    }
    else if (msg.type === 'parse') {
        const { id, filePath, content, frameworkNames } = msg;
        try {
            const language = (0, grammars_1.detectLanguage)(filePath, content);
            const result = (0, tree_sitter_1.extractFromSource)(filePath, content, language, frameworkNames);
            // Periodic parser reset to reclaim WASM heap memory
            const count = (parseCounts.get(language) ?? 0) + 1;
            parseCounts.set(language, count);
            if (count % PARSER_RESET_INTERVAL === 0) {
                (0, grammars_1.resetParser)(language);
            }
            worker_threads_1.parentPort.postMessage({ type: 'parse-result', id, result });
        }
        catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            // WASM memory errors leave the module in a corrupted state — all
            // subsequent parses would also fail (cascading failures). Crash the
            // worker so the main thread spawns a fresh one with a clean heap.
            if (message.includes('memory access out of bounds') || message.includes('out of memory')) {
                process.exit(1);
            }
            worker_threads_1.parentPort.postMessage({
                type: 'parse-result',
                id,
                result: {
                    nodes: [],
                    edges: [],
                    unresolvedReferences: [],
                    errors: [{ message: `Parse worker error: ${message}`, filePath: filePath, severity: 'error', code: 'parse_error' }],
                    durationMs: 0,
                },
            });
        }
    }
    else if (msg.type === 'shutdown') {
        worker_threads_1.parentPort.postMessage({ type: 'shutdown-ack' });
    }
});
//# sourceMappingURL=parse-worker.js.map