"use strict";
/**
 * Extraction Orchestrator
 *
 * Coordinates file scanning, parsing, and database storage.
 */
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { "default": mod };
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.loadAllGrammars = exports.loadGrammarsForLanguages = exports.initGrammars = exports.getSupportedLanguages = exports.isGrammarLoaded = exports.isLanguageSupported = exports.isSourceFile = exports.detectLanguage = exports.extractFromSource = exports.ExtractionOrchestrator = void 0;
exports.hashContent = hashContent;
exports.scanDirectory = scanDirectory;
exports.scanDirectoryAsync = scanDirectoryAsync;
const fs = __importStar(require("fs"));
const fsp = __importStar(require("fs/promises"));
const path = __importStar(require("path"));
const crypto = __importStar(require("crypto"));
const child_process_1 = require("child_process");
const tree_sitter_1 = require("./tree-sitter");
const grammars_1 = require("./grammars");
const errors_1 = require("../errors");
const utils_1 = require("../utils");
const ignore_1 = __importDefault(require("ignore"));
const frameworks_1 = require("../resolution/frameworks");
/**
 * Number of files to read in parallel during indexing.
 * File reads are I/O-bound; batching overlaps I/O wait with CPU parse work.
 */
const FILE_IO_BATCH_SIZE = 10;
// PARSER_RESET_INTERVAL moved to parse-worker.ts (runs in worker thread)
/**
 * Maximum time (ms) to wait for a single file to parse in the worker thread.
 * If tree-sitter hangs or WASM runs out of memory, this prevents the entire
 * indexing run from freezing. The worker is restarted after a timeout.
 */
const PARSE_TIMEOUT_MS = 10_000;
/**
 * Number of files to parse before recycling the worker thread.
 * WASM linear memory can grow but NEVER shrink (WebAssembly spec limitation).
 * The only way to reclaim tree-sitter's WASM heap is to destroy the entire
 * V8 isolate by terminating the worker thread and spawning a fresh one.
 * This interval balances memory usage against the cost of reloading grammars.
 */
const WORKER_RECYCLE_INTERVAL = 250;
/**
 * Calculate SHA256 hash of file contents
 */
function hashContent(content) {
    return crypto.createHash('sha256').update(content).digest('hex');
}
/**
 * Skip files larger than this (bytes). Generated bundles, minified JS, and
 * vendored blobs blow the WASM heap and the worker-recycle budget for no useful
 * symbols. 1 MB covers essentially all hand-written source.
 */
const MAX_FILE_SIZE = 1024 * 1024;
/**
 * Collect git-visible files (tracked + untracked, .gitignore-respected) from the
 * git repository rooted at `repoDir`, adding each to `files` with `prefix`
 * prepended so paths stay relative to the original scan root.
 *
 * Recurses into embedded git repositories — nested repos that are NOT submodules
 * (independent clones living inside the workspace, common in CMake "super-repo"
 * layouts). The parent repo's `git ls-files` cannot see into them: tracked output
 * skips them entirely, and untracked output reports them only as an opaque
 * "subdir/" entry (trailing slash) rather than expanding their files. Each
 * embedded repo is its own git boundary, so we re-run `git ls-files` inside it.
 * (See issue #193.)
 */
function collectGitFiles(repoDir, prefix, files) {
    const gitOpts = { cwd: repoDir, encoding: 'utf-8', timeout: 30000, maxBuffer: 50 * 1024 * 1024, stdio: ['pipe', 'pipe', 'pipe'] };
    // Tracked files. --recurse-submodules pulls in files from active submodules,
    // which the index would otherwise represent only as a commit pointer.
    // Without this, monorepos using submodules index 0 files. (See issue #147.)
    // Note: --recurse-submodules only supports -c/--cached and --stage modes — it
    // can't be combined with -o, so untracked files are gathered separately below.
    const tracked = (0, child_process_1.execFileSync)('git', ['ls-files', '-c', '--recurse-submodules'], gitOpts);
    for (const line of tracked.split('\n')) {
        const trimmed = line.trim();
        if (trimmed) {
            files.add((0, utils_1.normalizePath)(prefix + trimmed));
        }
    }
    // Untracked files (submodules manage their own untracked state). Embedded git
    // repos surface here as a single "subdir/" entry that git refuses to descend
    // into — recurse into those as their own repos so their source gets indexed.
    const untracked = (0, child_process_1.execFileSync)('git', ['ls-files', '-o', '--exclude-standard'], gitOpts);
    for (const line of untracked.split('\n')) {
        const trimmed = line.trim();
        if (!trimmed)
            continue;
        if (trimmed.endsWith('/')) {
            // git only emits a trailing-slash directory entry for an embedded repo.
            // Guard with a .git check anyway, and skip anything else exactly as git
            // itself skips it (we never descend into a non-repo opaque dir).
            const childDir = path.join(repoDir, trimmed);
            if (fs.existsSync(path.join(childDir, '.git'))) {
                collectGitFiles(childDir, prefix + trimmed, files);
            }
            continue;
        }
        files.add((0, utils_1.normalizePath)(prefix + trimmed));
    }
}
/**
 * Get all files visible to git (tracked + untracked but not ignored).
 * Respects .gitignore at all levels (root, subdirectories) and descends into
 * embedded (nested, non-submodule) git repos. Returns null on failure
 * (non-git project) so callers can fall back to a filesystem walk.
 */
function getGitVisibleFiles(rootDir) {
    try {
        // Check if the project directory is gitignored by a parent repo.
        // When rootDir lives inside a parent git repo that ignores it,
        // `git ls-files` returns nothing — fall back to filesystem walk.
        const gitRoot = (0, child_process_1.execFileSync)('git', ['rev-parse', '--show-toplevel'], { cwd: rootDir, encoding: 'utf-8', timeout: 5000, stdio: ['pipe', 'pipe', 'pipe'] }).trim();
        if (path.resolve(gitRoot) !== path.resolve(rootDir)) {
            try {
                // git check-ignore exits 0 if the path IS ignored, 1 if not
                (0, child_process_1.execFileSync)('git', ['check-ignore', '-q', path.resolve(rootDir)], { cwd: rootDir, encoding: 'utf-8', timeout: 5000, stdio: ['pipe', 'pipe', 'pipe'] });
                // Directory is gitignored by parent repo — fall back to filesystem walk
                return null;
            }
            catch {
                // Not ignored — safe to use git ls-files
            }
        }
        const files = new Set();
        collectGitFiles(rootDir, '', files);
        return files;
    }
    catch {
        return null;
    }
}
/**
 * Use `git status` to detect changed files instead of scanning every file.
 * Returns null on failure so callers fall back to full scan.
 */
function getGitChangedFiles(rootDir) {
    try {
        const output = (0, child_process_1.execFileSync)('git', ['status', '--porcelain', '--no-renames'], { cwd: rootDir, encoding: 'utf-8', timeout: 10000, stdio: ['pipe', 'pipe', 'pipe'] });
        const modified = [];
        const added = [];
        const deleted = [];
        for (const line of output.split('\n')) {
            if (line.length < 4)
                continue; // Minimum: "XY file"
            const statusCode = line.substring(0, 2);
            const filePath = (0, utils_1.normalizePath)(line.substring(3));
            // Skip non-source files (git status already omits .gitignored paths).
            if (!(0, grammars_1.isSourceFile)(filePath))
                continue;
            if (statusCode === '??') {
                added.push(filePath);
            }
            else if (statusCode.includes('D')) {
                deleted.push(filePath);
            }
            else {
                // M, MM, AM, A (staged), etc. — treat as modified
                modified.push(filePath);
            }
        }
        return { modified, added, deleted };
    }
    catch {
        return null;
    }
}
/**
 * Recursively scan a directory for source files.
 *
 * In git repos, uses `git ls-files` (inherently respects .gitignore at all
 * levels), then keeps files with a supported source extension. For non-git
 * projects, falls back to a filesystem walk that parses .gitignore itself.
 */
function scanDirectory(rootDir, onProgress) {
    // Fast path: use git to get all visible files (respects .gitignore everywhere)
    const gitFiles = getGitVisibleFiles(rootDir);
    if (gitFiles) {
        const files = [];
        let count = 0;
        for (const filePath of gitFiles) {
            if ((0, grammars_1.isSourceFile)(filePath)) {
                files.push(filePath);
                count++;
                onProgress?.(count, filePath);
            }
        }
        return files;
    }
    // Fallback: walk filesystem for non-git projects
    return scanDirectoryWalk(rootDir, onProgress);
}
/**
 * Async variant of scanDirectory that yields to the event loop periodically,
 * allowing worker threads to receive and render progress messages.
 */
async function scanDirectoryAsync(rootDir, onProgress) {
    const gitFiles = getGitVisibleFiles(rootDir);
    if (gitFiles) {
        const files = [];
        let count = 0;
        for (const filePath of gitFiles) {
            if ((0, grammars_1.isSourceFile)(filePath)) {
                files.push(filePath);
                count++;
                onProgress?.(count, filePath);
                // Yield every 100 files so worker threads can render progress
                if (count % 100 === 0) {
                    await new Promise(r => setImmediate(r));
                }
            }
        }
        return files;
    }
    return scanDirectoryWalk(rootDir, onProgress);
}
/**
 * Filesystem walk fallback for non-git projects.
 */
function scanDirectoryWalk(rootDir, onProgress) {
    const files = [];
    let count = 0;
    const visitedDirs = new Set();
    const loadIgnore = (dir) => {
        try {
            const giPath = path.join(dir, '.gitignore');
            if (fs.existsSync(giPath)) {
                return { dir, ig: (0, ignore_1.default)().add(fs.readFileSync(giPath, 'utf-8')) };
            }
        }
        catch {
            // Unreadable .gitignore — treat as absent.
        }
        return null;
    };
    const isIgnored = (fullPath, isDir, matchers) => {
        for (const { dir, ig } of matchers) {
            let rel = (0, utils_1.normalizePath)(path.relative(dir, fullPath));
            if (!rel || rel.startsWith('..'))
                continue; // not under this matcher's dir
            if (isDir)
                rel += '/'; // dir-only rules (e.g. `build/`) only match with the slash
            if (ig.ignores(rel))
                return true;
        }
        return false;
    };
    function walk(dir, matchers) {
        let realDir;
        try {
            realDir = fs.realpathSync(dir);
        }
        catch {
            (0, errors_1.logDebug)('Skipping unresolvable directory', { dir });
            return;
        }
        if (visitedDirs.has(realDir)) {
            (0, errors_1.logDebug)('Skipping already-visited directory (symlink cycle)', { dir, realDir });
            return;
        }
        visitedDirs.add(realDir);
        // This directory's own .gitignore (if present) applies to everything below it.
        const own = loadIgnore(dir);
        const active = own ? [...matchers, own] : matchers;
        let entries;
        try {
            entries = fs.readdirSync(dir, { withFileTypes: true });
        }
        catch (error) {
            (0, errors_1.logDebug)('Skipping unreadable directory', { dir, error: String(error) });
            return;
        }
        for (const entry of entries) {
            // Never descend into git internals or our own data directory.
            if (entry.name === '.git' || entry.name === '.codegraph')
                continue;
            const fullPath = path.join(dir, entry.name);
            const relativePath = (0, utils_1.normalizePath)(path.relative(rootDir, fullPath));
            if (entry.isSymbolicLink()) {
                try {
                    const realTarget = fs.realpathSync(fullPath);
                    const stat = fs.statSync(realTarget);
                    if (stat.isDirectory()) {
                        if (!isIgnored(fullPath, true, active)) {
                            walk(fullPath, active);
                        }
                    }
                    else if (stat.isFile()) {
                        if (!isIgnored(fullPath, false, active) && (0, grammars_1.isSourceFile)(relativePath)) {
                            files.push(relativePath);
                            count++;
                            onProgress?.(count, relativePath);
                        }
                    }
                }
                catch {
                    (0, errors_1.logDebug)('Skipping broken symlink', { path: fullPath });
                }
                continue;
            }
            if (entry.isDirectory()) {
                if (!isIgnored(fullPath, true, active)) {
                    walk(fullPath, active);
                }
            }
            else if (entry.isFile()) {
                if (!isIgnored(fullPath, false, active) && (0, grammars_1.isSourceFile)(relativePath)) {
                    files.push(relativePath);
                    count++;
                    onProgress?.(count, relativePath);
                }
            }
        }
    }
    walk(rootDir, []);
    return files;
}
/**
 * Extraction orchestrator
 */
class ExtractionOrchestrator {
    rootDir;
    queries;
    /**
     * Names of frameworks detected for this project, populated by indexAll().
     * Passed to extractFromSource so framework-specific extractors (route nodes,
     * middleware, etc.) run after the tree-sitter pass. Cleared if detection
     * hasn't run yet so single-file re-index paths can detect on the spot.
     */
    detectedFrameworkNames = null;
    constructor(rootDir, queries) {
        this.rootDir = rootDir;
        this.queries = queries;
    }
    /**
     * Build a filesystem-backed ResolutionContext sufficient for framework
     * detection. Graph-query methods (getNodesByName etc.) return empty because
     * the DB hasn't been populated yet, but detect() only uses readFile,
     * fileExists, and getAllFiles, so that's fine.
     */
    buildDetectionContext(files) {
        const rootDir = this.rootDir;
        return {
            getNodesInFile: () => [],
            getNodesByName: () => [],
            getNodesByQualifiedName: () => [],
            getNodesByKind: () => [],
            getNodesByLowerName: () => [],
            getImportMappings: () => [],
            getAllFiles: () => files,
            getProjectRoot: () => rootDir,
            fileExists: (relativePath) => {
                const full = (0, utils_1.validatePathWithinRoot)(rootDir, relativePath);
                if (!full)
                    return false;
                try {
                    return fs.existsSync(full);
                }
                catch {
                    return false;
                }
            },
            readFile: (relativePath) => {
                const full = (0, utils_1.validatePathWithinRoot)(rootDir, relativePath);
                if (!full)
                    return null;
                try {
                    return fs.readFileSync(full, 'utf-8');
                }
                catch {
                    return null;
                }
            },
        };
    }
    /**
     * Detect frameworks on demand using the current scanned files (or a fresh
     * scan if none are provided). Cached on the orchestrator so repeat calls
     * inside a single run don't re-scan.
     */
    ensureDetectedFrameworks(files) {
        if (this.detectedFrameworkNames !== null)
            return this.detectedFrameworkNames;
        const fileList = files ?? scanDirectory(this.rootDir);
        const context = this.buildDetectionContext(fileList);
        this.detectedFrameworkNames = (0, frameworks_1.detectFrameworks)(context).map((r) => r.name);
        return this.detectedFrameworkNames;
    }
    /**
     * Index all files in the project
     */
    async indexAll(onProgress, signal, verbose) {
        await (0, grammars_1.initGrammars)();
        const startTime = Date.now();
        const errors = [];
        let filesIndexed = 0;
        let filesSkipped = 0;
        let filesErrored = 0;
        let totalNodes = 0;
        let totalEdges = 0;
        const log = verbose
            ? (msg) => { console.log(`[worker] ${msg}`); }
            : (_msg) => { };
        // Phase 1: Scan for files
        onProgress?.({
            phase: 'scanning',
            current: 0,
            total: 0,
        });
        const files = await scanDirectoryAsync(this.rootDir, (current, file) => {
            onProgress?.({
                phase: 'scanning',
                current,
                total: 0,
                currentFile: file,
            });
        });
        // Detect frameworks once per indexAll run using the scanned file list.
        // Names are passed to each parse call so framework-specific extractors
        // (route nodes, middleware, etc.) run after the tree-sitter pass.
        // Framework detection is reset each run so adding e.g. requirements.txt
        // between runs is picked up without restarting the process.
        this.detectedFrameworkNames = null;
        const frameworkNames = this.ensureDetectedFrameworks(files);
        if (signal?.aborted) {
            return {
                success: false,
                filesIndexed: 0,
                filesSkipped: 0,
                filesErrored: 0,
                nodesCreated: 0,
                edgesCreated: 0,
                errors: [{ message: 'Aborted', severity: 'error' }],
                durationMs: Date.now() - startTime,
            };
        }
        // Phase 2: Parse files in a worker thread (keeps main thread unblocked for UI)
        const total = files.length;
        let processed = 0;
        // Emit parsing phase immediately so the progress bar appears during worker setup.
        // The yield lets the shimmer worker flush the phase transition to stdout before
        // the main thread starts synchronous grammar detection work.
        onProgress?.({
            phase: 'parsing',
            current: 0,
            total,
        });
        await new Promise(resolve => setImmediate(resolve));
        // Detect needed languages and load grammars in the parse worker
        const neededLanguages = [...new Set(files.map((f) => (0, grammars_1.detectLanguage)(f)))];
        // .h files default to 'c' but may be C++ — ensure cpp grammar is loaded when c is needed
        if (neededLanguages.includes('c') && !neededLanguages.includes('cpp')) {
            neededLanguages.push('cpp');
        }
        // Try to use a worker thread for parsing (keeps main thread unblocked for UI).
        // Falls back to in-process parsing if the compiled worker is unavailable (e.g. tests).
        const parseWorkerPath = path.join(__dirname, 'parse-worker.js');
        const useWorker = fs.existsSync(parseWorkerPath);
        let WorkerClass = null;
        if (useWorker) {
            const { Worker } = await Promise.resolve().then(() => __importStar(require('worker_threads')));
            WorkerClass = Worker;
        }
        else {
            // In-process fallback: load grammars locally
            await (0, grammars_1.loadGrammarsForLanguages)(neededLanguages);
        }
        // --- Worker lifecycle management ---
        // The worker can crash (OOM in WASM) or hang on pathological files.
        // We track pending parse promises and handle both cases:
        //   - Timeout: terminate + restart the worker, reject the timed-out request
        //   - Crash: reject all pending promises, restart for remaining files
        let parseWorker = null;
        let nextId = 0;
        let workerParseCount = 0;
        const pendingParses = new Map();
        function rejectAllPending(reason) {
            for (const [id, pending] of pendingParses) {
                clearTimeout(pending.timer);
                pendingParses.delete(id);
                pending.reject(new Error(reason));
            }
        }
        function attachWorkerHandlers(w) {
            w.on('message', (msg) => {
                if (msg.type === 'parse-result' && msg.id !== undefined) {
                    const pending = pendingParses.get(msg.id);
                    if (pending) {
                        clearTimeout(pending.timer);
                        pendingParses.delete(msg.id);
                        pending.resolve(msg.result);
                    }
                }
            });
            w.on('error', (err) => {
                (0, errors_1.logWarn)('Parse worker error', { error: err.message });
                rejectAllPending(`Worker error: ${err.message}`);
            });
            w.on('exit', (code) => {
                if (code !== 0 && pendingParses.size > 0) {
                    (0, errors_1.logWarn)('Parse worker exited unexpectedly', { code });
                    rejectAllPending(`Worker exited with code ${code}`);
                }
                // Clear reference so we know to respawn, reset count so
                // the fresh worker gets a full cycle before recycling.
                if (parseWorker === w) {
                    parseWorker = null;
                    workerParseCount = 0;
                }
            });
        }
        async function ensureWorker() {
            if (parseWorker)
                return parseWorker;
            log('Spawning new parse worker...');
            parseWorker = new WorkerClass(parseWorkerPath);
            attachWorkerHandlers(parseWorker);
            // Load grammars in the new worker
            await new Promise((resolve, reject) => {
                parseWorker.once('message', (msg) => {
                    if (msg.type === 'grammars-loaded')
                        resolve();
                    else
                        reject(new Error(`Unexpected message: ${msg.type}`));
                });
                parseWorker.postMessage({ type: 'load-grammars', languages: neededLanguages });
            });
            return parseWorker;
        }
        if (WorkerClass) {
            await ensureWorker();
        }
        /**
         * Recycle the worker thread to reclaim WASM memory.
         * Terminates the current worker and clears the reference so
         * ensureWorker() will spawn a fresh one on the next call.
         */
        function recycleWorker() {
            if (!parseWorker)
                return;
            log(`Recycling worker after ${workerParseCount} parses (heap: ${Math.round(process.memoryUsage().rss / 1024 / 1024)}MB RSS)`);
            const w = parseWorker;
            parseWorker = null;
            workerParseCount = 0;
            // Fire-and-forget: worker.terminate() can hang if WASM is stuck
            w.terminate().catch(() => { });
        }
        async function requestParse(filePath, content) {
            if (!WorkerClass) {
                // In-process fallback
                return (0, tree_sitter_1.extractFromSource)(filePath, content, (0, grammars_1.detectLanguage)(filePath, content), frameworkNames);
            }
            // Recycle the worker before the next parse if we've hit the threshold.
            // This destroys the WASM linear memory (which can grow but never shrink)
            // and starts a fresh worker with a clean heap.
            if (workerParseCount >= WORKER_RECYCLE_INTERVAL) {
                await recycleWorker();
            }
            const worker = await ensureWorker();
            const id = nextId++;
            workerParseCount++;
            // Scale timeout for large files: base 10s + 10s per 100KB
            const timeoutMs = PARSE_TIMEOUT_MS + Math.floor(content.length / 100_000) * 10_000;
            return new Promise((resolve, reject) => {
                const timer = setTimeout(() => {
                    pendingParses.delete(id);
                    log(`TIMEOUT: ${filePath} exceeded ${timeoutMs}ms — killing worker`);
                    // Reject FIRST — worker.terminate() can hang if WASM is stuck
                    parseWorker = null;
                    workerParseCount = 0;
                    reject(new Error(`Parse timed out after ${timeoutMs}ms`));
                    // Fire-and-forget: kill the stuck worker in the background
                    worker.terminate().catch(() => { });
                }, timeoutMs);
                pendingParses.set(id, { resolve, reject, timer });
                worker.postMessage({ type: 'parse', id, filePath, content, frameworkNames });
            });
        }
        for (let i = 0; i < files.length; i += FILE_IO_BATCH_SIZE) {
            if (signal?.aborted) {
                if (parseWorker)
                    parseWorker.terminate().catch(() => { });
                return {
                    success: false,
                    filesIndexed,
                    filesSkipped,
                    filesErrored,
                    nodesCreated: totalNodes,
                    edgesCreated: totalEdges,
                    errors: [{ message: 'Aborted', severity: 'error' }, ...errors],
                    durationMs: Date.now() - startTime,
                };
            }
            const batch = files.slice(i, i + FILE_IO_BATCH_SIZE);
            // Read files in parallel (with path validation before any I/O)
            const fileContents = await Promise.all(batch.map(async (fp) => {
                try {
                    const fullPath = (0, utils_1.validatePathWithinRoot)(this.rootDir, fp);
                    if (!fullPath) {
                        (0, errors_1.logWarn)('Path traversal blocked in batch reader', { filePath: fp });
                        return { filePath: fp, content: null, stats: null, error: new Error('Path traversal blocked') };
                    }
                    const content = await fsp.readFile(fullPath, 'utf-8');
                    const stats = await fsp.stat(fullPath);
                    return { filePath: fp, content, stats, error: null };
                }
                catch (err) {
                    return { filePath: fp, content: null, stats: null, error: err };
                }
            }));
            // Send to worker for parsing, store results on main thread
            for (const { filePath, content, stats, error } of fileContents) {
                if (signal?.aborted) {
                    if (parseWorker)
                        parseWorker.terminate().catch(() => { });
                    return {
                        success: false,
                        filesIndexed,
                        filesSkipped,
                        filesErrored,
                        nodesCreated: totalNodes,
                        edgesCreated: totalEdges,
                        errors: [{ message: 'Aborted', severity: 'error' }, ...errors],
                        durationMs: Date.now() - startTime,
                    };
                }
                // Report progress before parsing (show current file being worked on)
                onProgress?.({
                    phase: 'parsing',
                    current: processed,
                    total,
                    currentFile: filePath,
                });
                if (error || content === null || stats === null) {
                    processed++;
                    filesErrored++;
                    errors.push({
                        message: `Failed to read file: ${error instanceof Error ? error.message : String(error)}`,
                        filePath,
                        severity: 'error',
                        code: 'read_error',
                    });
                    continue;
                }
                // Honour MAX_FILE_SIZE. Without this check, vendored generated
                // headers, minified bundles, and other multi-MB files get indexed,
                // wasting WASM heap and the worker recycle budget on inputs with no
                // useful symbols. The single-file extractFile path already enforces
                // this; the bulk path used to silently skip the check.
                if (stats.size > MAX_FILE_SIZE) {
                    processed++;
                    filesSkipped++;
                    errors.push({
                        message: `File exceeds max size (${stats.size} > ${MAX_FILE_SIZE})`,
                        filePath,
                        severity: 'warning',
                        code: 'size_exceeded',
                    });
                    onProgress?.({ phase: 'parsing', current: processed, total });
                    continue;
                }
                // Parse in worker thread (main thread stays unblocked).
                // Wrapped in try/catch to handle worker timeouts and crashes gracefully.
                let result;
                try {
                    result = await requestParse(filePath, content);
                }
                catch (parseErr) {
                    processed++;
                    filesErrored++;
                    errors.push({
                        message: parseErr instanceof Error ? parseErr.message : String(parseErr),
                        filePath,
                        severity: 'error',
                        code: 'parse_error',
                    });
                    continue;
                }
                processed++;
                // Store in database on main thread (SQLite is not thread-safe)
                if (result.nodes.length > 0 || result.errors.length === 0) {
                    const language = (0, grammars_1.detectLanguage)(filePath, content);
                    this.storeExtractionResult(filePath, content, language, stats, result);
                }
                if (result.errors.length > 0) {
                    for (const err of result.errors) {
                        if (!err.filePath)
                            err.filePath = filePath;
                    }
                    errors.push(...result.errors);
                }
                if (result.nodes.length > 0) {
                    filesIndexed++;
                    totalNodes += result.nodes.length;
                    totalEdges += result.edges.length;
                }
                else if (result.errors.some((e) => e.severity === 'error')) {
                    filesErrored++;
                }
                else {
                    filesSkipped++;
                }
            }
        }
        // Report 100% so the progress bar doesn't hang at 99%
        onProgress?.({
            phase: 'parsing',
            current: total,
            total,
        });
        // Yield so the shimmer worker's buffered stdout writes can flush.
        // Worker thread stdout is proxied through the main thread's event loop,
        // so synchronous work here blocks the animation from rendering.
        await new Promise(resolve => setImmediate(resolve));
        // Retry pass: files that failed due to WASM memory corruption may succeed
        // on a fresh worker with a clean heap. Recycle before each attempt so
        // every file gets the absolute cleanest WASM state possible.
        const retryableErrors = errors.filter((e) => e.code === 'parse_error' && e.filePath &&
            (e.message.includes('Worker exited') || e.message.includes('memory access out of bounds')));
        if (retryableErrors.length > 0 && WorkerClass) {
            log(`Retrying ${retryableErrors.length} files that failed due to WASM memory errors...`);
            const stillFailing = [];
            for (const errEntry of retryableErrors) {
                const filePath = errEntry.filePath;
                if (signal?.aborted)
                    break;
                // Fresh worker for every retry — maximum WASM headroom
                recycleWorker();
                let content;
                try {
                    const fullPath = (0, utils_1.validatePathWithinRoot)(this.rootDir, filePath);
                    if (!fullPath)
                        continue;
                    content = await fsp.readFile(fullPath, 'utf-8');
                }
                catch {
                    continue;
                }
                let result;
                try {
                    result = await requestParse(filePath, content);
                }
                catch {
                    stillFailing.push(errEntry);
                    continue;
                }
                if (result.nodes.length > 0 || result.errors.length === 0) {
                    const language = (0, grammars_1.detectLanguage)(filePath, content);
                    const stats = await fsp.stat(path.join(this.rootDir, filePath));
                    this.storeExtractionResult(filePath, content, language, stats, result);
                    const idx = errors.indexOf(errEntry);
                    if (idx >= 0)
                        errors.splice(idx, 1);
                    filesErrored--;
                    filesIndexed++;
                    totalNodes += result.nodes.length;
                    totalEdges += result.edges.length;
                    log(`Retry OK: ${filePath} (${result.nodes.length} nodes)`);
                }
            }
            // Last resort: for files that still crash on a clean worker, strip
            // comment-only lines to reduce WASM memory pressure. Many compiler
            // test files are 90%+ comments (CHECK directives) that don't contribute
            // code nodes but consume parser memory.
            if (stillFailing.length > 0) {
                log(`${stillFailing.length} files still failing — retrying with comments stripped...`);
                for (const errEntry of stillFailing) {
                    const filePath = errEntry.filePath;
                    if (signal?.aborted)
                        break;
                    recycleWorker();
                    let fullContent;
                    try {
                        const fullPath = (0, utils_1.validatePathWithinRoot)(this.rootDir, filePath);
                        if (!fullPath)
                            continue;
                        fullContent = await fsp.readFile(fullPath, 'utf-8');
                    }
                    catch {
                        continue;
                    }
                    // Strip lines that are entirely comments (preserving line numbers
                    // by replacing with empty lines so node positions stay correct)
                    const stripped = fullContent
                        .split('\n')
                        .map(line => /^\s*\/\//.test(line) ? '' : line)
                        .join('\n');
                    let result;
                    try {
                        result = await requestParse(filePath, stripped);
                    }
                    catch {
                        continue;
                    }
                    if (result.nodes.length > 0 || result.errors.length === 0) {
                        const language = (0, grammars_1.detectLanguage)(filePath, fullContent);
                        const stats = await fsp.stat(path.join(this.rootDir, filePath));
                        this.storeExtractionResult(filePath, fullContent, language, stats, result);
                        const idx = errors.indexOf(errEntry);
                        if (idx >= 0)
                            errors.splice(idx, 1);
                        filesErrored--;
                        filesIndexed++;
                        totalNodes += result.nodes.length;
                        totalEdges += result.edges.length;
                        log(`Retry (stripped) OK: ${filePath} (${result.nodes.length} nodes)`);
                    }
                }
            }
        }
        // Shut down parse worker and clear any pending timers
        rejectAllPending('Indexing complete');
        if (parseWorker) {
            parseWorker.terminate().catch(() => { });
        }
        return {
            success: filesIndexed > 0 || errors.filter((e) => e.severity === 'error').length === 0,
            filesIndexed,
            filesSkipped,
            filesErrored,
            nodesCreated: totalNodes,
            edgesCreated: totalEdges,
            errors,
            durationMs: Date.now() - startTime,
        };
    }
    /**
     * Index specific files
     */
    async indexFiles(filePaths) {
        const startTime = Date.now();
        const errors = [];
        let filesIndexed = 0;
        let filesSkipped = 0;
        let filesErrored = 0;
        let totalNodes = 0;
        let totalEdges = 0;
        for (const filePath of filePaths) {
            const result = await this.indexFile(filePath);
            if (result.errors.length > 0) {
                errors.push(...result.errors);
            }
            if (result.nodes.length > 0) {
                filesIndexed++;
                totalNodes += result.nodes.length;
                totalEdges += result.edges.length;
            }
            else if (result.errors.some((e) => e.severity === 'error')) {
                filesErrored++;
            }
            else {
                filesSkipped++;
            }
        }
        return {
            success: filesIndexed > 0 || errors.filter((e) => e.severity === 'error').length === 0,
            filesIndexed,
            filesSkipped,
            filesErrored,
            nodesCreated: totalNodes,
            edgesCreated: totalEdges,
            errors,
            durationMs: Date.now() - startTime,
        };
    }
    /**
     * Index a single file
     */
    async indexFile(relativePath) {
        const fullPath = (0, utils_1.validatePathWithinRoot)(this.rootDir, relativePath);
        if (!fullPath) {
            return {
                nodes: [],
                edges: [],
                unresolvedReferences: [],
                errors: [{ message: `Path traversal blocked: ${relativePath}`, filePath: relativePath, severity: 'error', code: 'path_traversal' }],
                durationMs: 0,
            };
        }
        // Read file content and stats
        let content;
        let stats;
        try {
            stats = await fsp.stat(fullPath);
            content = await fsp.readFile(fullPath, 'utf-8');
        }
        catch (error) {
            return {
                nodes: [],
                edges: [],
                unresolvedReferences: [],
                errors: [
                    {
                        message: `Failed to read file: ${error instanceof Error ? error.message : String(error)}`,
                        filePath: relativePath,
                        severity: 'error',
                        code: 'read_error',
                    },
                ],
                durationMs: 0,
            };
        }
        return this.indexFileWithContent(relativePath, content, stats);
    }
    /**
     * Index a single file with pre-read content and stats.
     * Used by the parallel batch reader to avoid redundant file I/O.
     */
    async indexFileWithContent(relativePath, content, stats) {
        // Prevent path traversal
        const fullPath = (0, utils_1.validatePathWithinRoot)(this.rootDir, relativePath);
        if (!fullPath) {
            (0, errors_1.logWarn)('Path traversal blocked in indexFileWithContent', { relativePath });
            return {
                nodes: [],
                edges: [],
                unresolvedReferences: [],
                errors: [{ message: 'Path traversal blocked', filePath: relativePath, severity: 'error', code: 'path_traversal' }],
                durationMs: 0,
            };
        }
        // Check file size
        if (stats.size > MAX_FILE_SIZE) {
            return {
                nodes: [],
                edges: [],
                unresolvedReferences: [],
                errors: [
                    {
                        message: `File exceeds max size (${stats.size} > ${MAX_FILE_SIZE})`,
                        filePath: relativePath,
                        severity: 'warning',
                        code: 'size_exceeded',
                    },
                ],
                durationMs: 0,
            };
        }
        // Detect language
        const language = (0, grammars_1.detectLanguage)(relativePath, content);
        if (!(0, grammars_1.isLanguageSupported)(language)) {
            return {
                nodes: [],
                edges: [],
                unresolvedReferences: [],
                errors: [],
                durationMs: 0,
            };
        }
        // Extract from source. Use cached framework names if indexAll has run,
        // otherwise detect on the spot so single-file re-index paths still emit
        // route nodes / middleware / etc.
        const frameworkNames = this.ensureDetectedFrameworks();
        const result = (0, tree_sitter_1.extractFromSource)(relativePath, content, language, frameworkNames);
        // Store in database
        if (result.nodes.length > 0 || result.errors.length === 0) {
            this.storeExtractionResult(relativePath, content, language, stats, result);
        }
        return result;
    }
    /**
     * Store extraction result in database
     */
    storeExtractionResult(filePath, content, language, stats, result) {
        const contentHash = hashContent(content);
        // Check if file already exists and hasn't changed
        const existingFile = this.queries.getFileByPath(filePath);
        if (existingFile && existingFile.contentHash === contentHash) {
            return; // No changes
        }
        // Delete existing data for this file
        if (existingFile) {
            this.queries.deleteFile(filePath);
        }
        // Filter out nodes with missing required fields before insertion.
        // This prevents FK violations when edges reference nodes that would
        // be silently skipped by insertNode() (see issue #42).
        const validNodes = result.nodes.filter((n) => n.id && n.kind && n.name && n.filePath && n.language);
        // Insert nodes
        if (validNodes.length > 0) {
            this.queries.insertNodes(validNodes);
        }
        // Filter edges to only reference nodes that were actually inserted
        if (result.edges.length > 0) {
            const insertedIds = new Set(validNodes.map((n) => n.id));
            const validEdges = result.edges.filter((e) => insertedIds.has(e.source) && insertedIds.has(e.target));
            if (validEdges.length > 0) {
                this.queries.insertEdges(validEdges);
            }
        }
        // Insert unresolved references in batch with denormalized filePath/language
        if (result.unresolvedReferences.length > 0) {
            const insertedIds = new Set(validNodes.map((n) => n.id));
            const refsWithContext = result.unresolvedReferences
                .filter((ref) => insertedIds.has(ref.fromNodeId))
                .map((ref) => ({
                ...ref,
                filePath: ref.filePath ?? filePath,
                language: ref.language ?? language,
            }));
            if (refsWithContext.length > 0) {
                this.queries.insertUnresolvedRefsBatch(refsWithContext);
            }
        }
        // Insert file record
        const fileRecord = {
            path: filePath,
            contentHash,
            language,
            size: stats.size,
            modifiedAt: stats.mtimeMs,
            indexedAt: Date.now(),
            nodeCount: result.nodes.length,
            errors: result.errors.length > 0 ? result.errors : undefined,
        };
        this.queries.upsertFile(fileRecord);
    }
    /**
     * Sync with current file state.
     * Uses git status as a fast path when available, falling back to full scan.
     */
    async sync(onProgress) {
        await (0, grammars_1.initGrammars)(); // Initialize WASM runtime (grammars loaded lazily below)
        const startTime = Date.now();
        let filesChecked = 0;
        let filesAdded = 0;
        let filesModified = 0;
        let filesRemoved = 0;
        let nodesUpdated = 0;
        const changedFilePaths = [];
        onProgress?.({
            phase: 'scanning',
            current: 0,
            total: 0,
        });
        const filesToIndex = [];
        const gitChanges = getGitChangedFiles(this.rootDir);
        if (gitChanges) {
            // === Git fast path ===
            // Only inspect the files git reports as changed instead of scanning everything.
            filesChecked = gitChanges.modified.length + gitChanges.added.length + gitChanges.deleted.length;
            // Handle deleted files
            for (const filePath of gitChanges.deleted) {
                const tracked = this.queries.getFileByPath(filePath);
                if (tracked) {
                    this.queries.deleteFile(filePath);
                    filesRemoved++;
                }
            }
            // Handle modified + added files — read + hash only these. Untracked
            // (`??`) files stay untracked in git even after we index them, so they
            // can't be trusted as "new": re-hash and compare against the DB exactly
            // like modified files. Otherwise every sync re-indexes them and status
            // reports them as pending forever. (See issue #206.)
            for (const filePath of [...gitChanges.modified, ...gitChanges.added]) {
                const fullPath = path.join(this.rootDir, filePath);
                let content;
                try {
                    content = fs.readFileSync(fullPath, 'utf-8');
                }
                catch (error) {
                    (0, errors_1.logDebug)('Skipping unreadable file during sync', { filePath, error: String(error) });
                    continue;
                }
                const contentHash = hashContent(content);
                const tracked = this.queries.getFileByPath(filePath);
                if (!tracked) {
                    filesToIndex.push(filePath);
                    changedFilePaths.push(filePath);
                    filesAdded++;
                }
                else if (tracked.contentHash !== contentHash) {
                    filesToIndex.push(filePath);
                    changedFilePaths.push(filePath);
                    filesModified++;
                }
            }
        }
        else {
            // === Fallback: full scan (non-git project or git failure) ===
            const currentFiles = new Set(scanDirectory(this.rootDir));
            filesChecked = currentFiles.size;
            // Build Map for O(1) lookups instead of .find() per file
            const trackedFiles = this.queries.getAllFiles();
            const trackedMap = new Map();
            for (const f of trackedFiles) {
                trackedMap.set(f.path, f);
            }
            // Find files to remove (in DB but not on disk)
            for (const tracked of trackedFiles) {
                if (!currentFiles.has(tracked.path)) {
                    this.queries.deleteFile(tracked.path);
                    filesRemoved++;
                }
            }
            // Find files to add or update
            for (const filePath of currentFiles) {
                const fullPath = path.join(this.rootDir, filePath);
                let content;
                try {
                    content = fs.readFileSync(fullPath, 'utf-8');
                }
                catch (error) {
                    (0, errors_1.logDebug)('Skipping unreadable file during sync', { filePath, error: String(error) });
                    continue;
                }
                const contentHash = hashContent(content);
                const tracked = trackedMap.get(filePath);
                if (!tracked) {
                    filesToIndex.push(filePath);
                    changedFilePaths.push(filePath);
                    filesAdded++;
                }
                else if (tracked.contentHash !== contentHash) {
                    filesToIndex.push(filePath);
                    changedFilePaths.push(filePath);
                    filesModified++;
                }
            }
        }
        // Load only grammars needed for changed files
        if (filesToIndex.length > 0) {
            const neededLanguages = [...new Set(filesToIndex.map((f) => (0, grammars_1.detectLanguage)(f)))];
            // .h files default to 'c' but may be C++ — ensure cpp grammar is loaded
            if (neededLanguages.includes('c') && !neededLanguages.includes('cpp')) {
                neededLanguages.push('cpp');
            }
            await (0, grammars_1.loadGrammarsForLanguages)(neededLanguages);
        }
        // Index changed files
        const total = filesToIndex.length;
        for (let i = 0; i < filesToIndex.length; i++) {
            const filePath = filesToIndex[i];
            onProgress?.({
                phase: 'parsing',
                current: i + 1,
                total,
                currentFile: filePath,
            });
            const result = await this.indexFile(filePath);
            nodesUpdated += result.nodes.length;
        }
        return {
            filesChecked,
            filesAdded,
            filesModified,
            filesRemoved,
            nodesUpdated,
            durationMs: Date.now() - startTime,
            changedFilePaths: changedFilePaths.length > 0 ? changedFilePaths : undefined,
        };
    }
    /**
     * Get files that have changed since last index.
     * Uses git status as a fast path when available, falling back to full scan.
     */
    getChangedFiles() {
        const gitChanges = getGitChangedFiles(this.rootDir);
        if (gitChanges) {
            // === Git fast path ===
            const added = [];
            const modified = [];
            const removed = [];
            // Deleted files — only report if tracked in DB
            for (const filePath of gitChanges.deleted) {
                const tracked = this.queries.getFileByPath(filePath);
                if (tracked) {
                    removed.push(filePath);
                }
            }
            // Modified + added files — read + hash, compare with DB. Untracked (`??`)
            // files stay untracked in git even after indexing, so they must be
            // hash-compared like modified files instead of always counting as added —
            // otherwise status reports them as pending forever. (See issue #206.)
            for (const filePath of [...gitChanges.modified, ...gitChanges.added]) {
                const fullPath = path.join(this.rootDir, filePath);
                let content;
                try {
                    content = fs.readFileSync(fullPath, 'utf-8');
                }
                catch (error) {
                    (0, errors_1.logDebug)('Skipping unreadable file while detecting changes', { filePath, error: String(error) });
                    continue;
                }
                const contentHash = hashContent(content);
                const tracked = this.queries.getFileByPath(filePath);
                if (!tracked) {
                    added.push(filePath);
                }
                else if (tracked.contentHash !== contentHash) {
                    modified.push(filePath);
                }
            }
            return { added, modified, removed };
        }
        // === Fallback: full scan (non-git project or git failure) ===
        const currentFiles = new Set(scanDirectory(this.rootDir));
        const trackedFiles = this.queries.getAllFiles();
        // Build Map for O(1) lookups
        const trackedMap = new Map();
        for (const f of trackedFiles) {
            trackedMap.set(f.path, f);
        }
        const added = [];
        const modified = [];
        const removed = [];
        // Find removed files
        for (const tracked of trackedFiles) {
            if (!currentFiles.has(tracked.path)) {
                removed.push(tracked.path);
            }
        }
        // Find added and modified files
        for (const filePath of currentFiles) {
            const fullPath = path.join(this.rootDir, filePath);
            let content;
            try {
                content = fs.readFileSync(fullPath, 'utf-8');
            }
            catch (error) {
                (0, errors_1.logDebug)('Skipping unreadable file while detecting changes', { filePath, error: String(error) });
                continue;
            }
            const contentHash = hashContent(content);
            const tracked = trackedMap.get(filePath);
            if (!tracked) {
                added.push(filePath);
            }
            else if (tracked.contentHash !== contentHash) {
                modified.push(filePath);
            }
        }
        return { added, modified, removed };
    }
}
exports.ExtractionOrchestrator = ExtractionOrchestrator;
// Re-export useful types and functions
var tree_sitter_2 = require("./tree-sitter");
Object.defineProperty(exports, "extractFromSource", { enumerable: true, get: function () { return tree_sitter_2.extractFromSource; } });
var grammars_2 = require("./grammars");
Object.defineProperty(exports, "detectLanguage", { enumerable: true, get: function () { return grammars_2.detectLanguage; } });
Object.defineProperty(exports, "isSourceFile", { enumerable: true, get: function () { return grammars_2.isSourceFile; } });
Object.defineProperty(exports, "isLanguageSupported", { enumerable: true, get: function () { return grammars_2.isLanguageSupported; } });
Object.defineProperty(exports, "isGrammarLoaded", { enumerable: true, get: function () { return grammars_2.isGrammarLoaded; } });
Object.defineProperty(exports, "getSupportedLanguages", { enumerable: true, get: function () { return grammars_2.getSupportedLanguages; } });
Object.defineProperty(exports, "initGrammars", { enumerable: true, get: function () { return grammars_2.initGrammars; } });
Object.defineProperty(exports, "loadGrammarsForLanguages", { enumerable: true, get: function () { return grammars_2.loadGrammarsForLanguages; } });
Object.defineProperty(exports, "loadAllGrammars", { enumerable: true, get: function () { return grammars_2.loadAllGrammars; } });
//# sourceMappingURL=index.js.map