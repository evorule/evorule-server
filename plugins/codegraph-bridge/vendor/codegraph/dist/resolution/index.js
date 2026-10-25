"use strict";
/**
 * Reference Resolution Orchestrator
 *
 * Coordinates all reference resolution strategies.
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
var __exportStar = (this && this.__exportStar) || function(m, exports) {
    for (var p in m) if (p !== "default" && !Object.prototype.hasOwnProperty.call(exports, p)) __createBinding(exports, m, p);
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.ReferenceResolver = void 0;
exports.createResolver = createResolver;
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const name_matcher_1 = require("./name-matcher");
const import_resolver_1 = require("./import-resolver");
const frameworks_1 = require("./frameworks");
const path_aliases_1 = require("./path-aliases");
const errors_1 = require("../errors");
const lru_cache_1 = require("./lru-cache");
/**
 * Cache size limits. Each per-resolver cache is bounded so memory
 * stays flat on large codebases (20k+ files). Sizes were chosen to
 * cover the working set for typical resolution batches without
 * exceeding a few hundred MB worst-case. Override via the env var
 * `CODEGRAPH_RESOLVER_CACHE_SIZE` (single integer applied to all
 * caches) when tuning for very large or very small projects.
 */
const DEFAULT_CACHE_LIMIT = 5_000;
function resolveCacheLimit() {
    const raw = process.env.CODEGRAPH_RESOLVER_CACHE_SIZE;
    if (!raw)
        return DEFAULT_CACHE_LIMIT;
    const parsed = Number.parseInt(raw, 10);
    if (Number.isFinite(parsed) && parsed > 0)
        return parsed;
    return DEFAULT_CACHE_LIMIT;
}
// Re-export types
__exportStar(require("./types"), exports);
// Pre-built Sets for O(1) built-in lookups (allocated once, shared across all instances)
const JS_BUILT_INS = new Set([
    'console', 'window', 'document', 'global', 'process',
    'Promise', 'Array', 'Object', 'String', 'Number', 'Boolean',
    'Date', 'Math', 'JSON', 'RegExp', 'Error', 'Map', 'Set',
    'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'fetch', 'require', 'module', 'exports', '__dirname', '__filename',
]);
const REACT_HOOKS = new Set([
    'useState', 'useEffect', 'useContext', 'useReducer', 'useCallback',
    'useMemo', 'useRef', 'useLayoutEffect', 'useImperativeHandle', 'useDebugValue',
]);
const PYTHON_BUILT_INS = new Set([
    'print', 'len', 'range', 'str', 'int', 'float', 'list', 'dict', 'set', 'tuple',
    'open', 'input', 'type', 'isinstance', 'hasattr', 'getattr', 'setattr',
    'super', 'self', 'cls', 'None', 'True', 'False',
]);
const PYTHON_BUILT_IN_TYPES = new Set([
    'list', 'dict', 'set', 'tuple', 'str', 'int', 'float', 'bool',
    'bytes', 'bytearray', 'frozenset', 'object', 'super',
]);
const PYTHON_BUILT_IN_METHODS = new Set([
    'append', 'extend', 'insert', 'remove', 'pop', 'clear', 'sort', 'reverse', 'copy',
    'update', 'keys', 'values', 'items', 'get',
    'add', 'discard', 'union', 'intersection', 'difference',
    'split', 'join', 'strip', 'lstrip', 'rstrip', 'replace', 'lower', 'upper',
    'startswith', 'endswith', 'find', 'index', 'count', 'encode', 'decode',
    'format', 'isdigit', 'isalpha', 'isalnum',
    'read', 'write', 'readline', 'readlines', 'close', 'flush', 'seek',
]);
const GO_STDLIB_PACKAGES = new Set([
    'fmt', 'os', 'io', 'net', 'http', 'log', 'math', 'sort', 'sync',
    'time', 'path', 'bytes', 'strings', 'strconv', 'errors', 'context',
    'json', 'xml', 'csv', 'html', 'template', 'regexp', 'reflect',
    'runtime', 'testing', 'flag', 'bufio', 'crypto', 'encoding',
    'filepath', 'hash', 'mime', 'rand', 'signal', 'sql', 'syscall',
    'unicode', 'unsafe', 'atomic', 'binary', 'debug', 'exec', 'heap',
    'ring', 'scanner', 'tar', 'zip', 'gzip', 'zlib', 'tls', 'url',
    'user', 'pprof', 'trace', 'ast', 'build', 'parser', 'printer',
    'token', 'types', 'cgo', 'plugin', 'race', 'ioutil',
    // Kubernetes-common stdlib aliases
    'utilruntime', 'utilwait', 'utilnet',
]);
const GO_BUILT_INS = new Set([
    'make', 'new', 'len', 'cap', 'append', 'copy', 'delete', 'close',
    'panic', 'recover', 'print', 'println', 'complex', 'real', 'imag',
    'error', 'nil', 'true', 'false', 'iota',
    'int', 'int8', 'int16', 'int32', 'int64',
    'uint', 'uint8', 'uint16', 'uint32', 'uint64', 'uintptr',
    'float32', 'float64', 'complex64', 'complex128',
    'string', 'bool', 'byte', 'rune', 'any',
]);
const PASCAL_UNIT_PREFIXES = [
    'System.', 'Winapi.', 'Vcl.', 'Fmx.', 'Data.', 'Datasnap.',
    'Soap.', 'Xml.', 'Web.', 'REST.', 'FireDAC.', 'IBX.',
    'IdHTTP', 'IdTCP', 'IdSSL',
];
const PASCAL_BUILT_INS = new Set([
    'System', 'SysUtils', 'Classes', 'Types', 'Variants', 'StrUtils',
    'Math', 'DateUtils', 'IOUtils', 'Generics.Collections', 'Generics.Defaults',
    'Rtti', 'TypInfo', 'SyncObjs', 'RegularExpressions',
    'SysInit', 'Windows', 'Messages', 'Graphics', 'Controls', 'Forms',
    'Dialogs', 'StdCtrls', 'ExtCtrls', 'ComCtrls', 'Menus', 'ActnList',
    'WriteLn', 'Write', 'ReadLn', 'Read', 'Inc', 'Dec', 'Ord', 'Chr',
    'Length', 'SetLength', 'High', 'Low', 'Assigned', 'FreeAndNil',
    'Format', 'IntToStr', 'StrToInt', 'FloatToStr', 'StrToFloat',
    'Trim', 'UpperCase', 'LowerCase', 'Pos', 'Copy', 'Delete', 'Insert',
    'Now', 'Date', 'Time', 'DateToStr', 'StrToDate',
    'Raise', 'Exit', 'Break', 'Continue', 'Abort',
    'True', 'False', 'nil', 'Self', 'Result',
    'Create', 'Destroy', 'Free',
    'TObject', 'TComponent', 'TPersistent', 'TInterfacedObject',
    'TList', 'TStringList', 'TStrings', 'TStream', 'TMemoryStream', 'TFileStream',
    'Exception', 'EAbort', 'EConvertError', 'EAccessViolation',
    'IInterface', 'IUnknown',
]);
/**
 * Reference Resolver
 *
 * Orchestrates reference resolution using multiple strategies.
 */
class ReferenceResolver {
    projectRoot;
    queries;
    context;
    frameworks = [];
    // All per-resolver caches are LRU-bounded. Previously these were
    // unbounded Maps that grew with every distinct lookup and OOM'd on
    // codebases with 20k+ files (see issue: unbounded cache growth).
    nodeCache; // per-file node cache
    fileCache; // per-file content cache
    importMappingCache;
    reExportCache;
    nameCache; // name → nodes cache
    lowerNameCache; // lower(name) → nodes cache
    qualifiedNameCache; // qualified_name → nodes cache
    knownNames = null; // all known symbol names for fast pre-filtering
    knownFiles = null;
    cachesWarmed = false;
    // tsconfig/jsconfig path-alias map. `undefined` = not yet computed,
    // `null` = computed and absent. Treated as immutable for the
    // resolver's lifetime; callers re-create the resolver if config changes.
    projectAliases = undefined;
    constructor(projectRoot, queries) {
        this.projectRoot = projectRoot;
        this.queries = queries;
        const limit = resolveCacheLimit();
        // The content cache is heavier (full file text), so we give it a
        // smaller budget than the metadata caches.
        const contentLimit = Math.max(64, Math.floor(limit / 5));
        this.nodeCache = new lru_cache_1.LRUCache(limit);
        this.fileCache = new lru_cache_1.LRUCache(contentLimit);
        this.importMappingCache = new lru_cache_1.LRUCache(limit);
        this.reExportCache = new lru_cache_1.LRUCache(limit);
        this.nameCache = new lru_cache_1.LRUCache(limit);
        this.lowerNameCache = new lru_cache_1.LRUCache(limit);
        this.qualifiedNameCache = new lru_cache_1.LRUCache(limit);
        this.context = this.createContext();
    }
    /**
     * Initialize the resolver (detect frameworks, etc.)
     */
    initialize() {
        this.frameworks = (0, frameworks_1.detectFrameworks)(this.context);
        this.clearCaches();
    }
    /**
     * Pre-build lightweight caches for resolution.
     * Node lookups are now handled by indexed SQLite queries instead of
     * loading all nodes into memory (which caused OOM on large codebases).
     * We cache the set of known symbol names for fast pre-filtering.
     */
    warmCaches() {
        if (this.cachesWarmed)
            return;
        // Only cache the set of known file paths (lightweight string set)
        this.knownFiles = new Set(this.queries.getAllFilePaths());
        // Cache all distinct symbol names for fast pre-filtering (just strings, not full nodes)
        this.knownNames = new Set(this.queries.getAllNodeNames());
        this.cachesWarmed = true;
    }
    /**
     * Clear internal caches
     */
    clearCaches() {
        this.nodeCache.clear();
        this.fileCache.clear();
        this.importMappingCache.clear();
        this.reExportCache.clear();
        this.nameCache.clear();
        this.lowerNameCache.clear();
        this.qualifiedNameCache.clear();
        this.knownNames = null;
        this.knownFiles = null;
        this.cachesWarmed = false;
    }
    /**
     * Create the resolution context
     */
    createContext() {
        return {
            getNodesInFile: (filePath) => {
                if (!this.nodeCache.has(filePath)) {
                    this.nodeCache.set(filePath, this.queries.getNodesByFile(filePath));
                }
                return this.nodeCache.get(filePath);
            },
            getNodesByName: (name) => {
                const cached = this.nameCache.get(name);
                if (cached !== undefined)
                    return cached;
                const result = this.queries.getNodesByName(name);
                this.nameCache.set(name, result);
                return result;
            },
            getNodesByQualifiedName: (qualifiedName) => {
                const cached = this.qualifiedNameCache.get(qualifiedName);
                if (cached !== undefined)
                    return cached;
                const result = this.queries.getNodesByQualifiedNameExact(qualifiedName);
                this.qualifiedNameCache.set(qualifiedName, result);
                return result;
            },
            getNodesByKind: (kind) => {
                return this.queries.getNodesByKind(kind);
            },
            fileExists: (filePath) => {
                // Check pre-built known files set first (O(1))
                if (this.knownFiles) {
                    const normalized = filePath.replace(/\\/g, '/');
                    if (this.knownFiles.has(filePath) || this.knownFiles.has(normalized)) {
                        return true;
                    }
                }
                // Fall back to filesystem for files not yet indexed
                const fullPath = path.join(this.projectRoot, filePath);
                try {
                    return fs.existsSync(fullPath);
                }
                catch (error) {
                    (0, errors_1.logDebug)('Error checking file existence', { filePath, error: String(error) });
                    return false;
                }
            },
            readFile: (filePath) => {
                if (this.fileCache.has(filePath)) {
                    return this.fileCache.get(filePath);
                }
                const fullPath = path.join(this.projectRoot, filePath);
                try {
                    const content = fs.readFileSync(fullPath, 'utf-8');
                    this.fileCache.set(filePath, content);
                    return content;
                }
                catch (error) {
                    (0, errors_1.logDebug)('Failed to read file for resolution', { filePath, error: String(error) });
                    this.fileCache.set(filePath, null);
                    return null;
                }
            },
            getProjectRoot: () => this.projectRoot,
            getAllFiles: () => {
                return this.queries.getAllFilePaths();
            },
            listDirectories: (relativePath) => {
                const target = relativePath === '.' || relativePath === ''
                    ? this.projectRoot
                    : path.join(this.projectRoot, relativePath);
                try {
                    return fs
                        .readdirSync(target, { withFileTypes: true })
                        .filter((entry) => entry.isDirectory())
                        .map((entry) => entry.name);
                }
                catch (error) {
                    (0, errors_1.logDebug)('Failed to list directory for resolution', {
                        relativePath,
                        error: String(error),
                    });
                    return [];
                }
            },
            getNodesByLowerName: (lowerName) => {
                const cached = this.lowerNameCache.get(lowerName);
                if (cached !== undefined)
                    return cached;
                const result = this.queries.getNodesByLowerName(lowerName);
                this.lowerNameCache.set(lowerName, result);
                return result;
            },
            getImportMappings: (filePath, language) => {
                const cacheKey = filePath;
                const cached = this.importMappingCache.get(cacheKey);
                if (cached)
                    return cached;
                const content = this.context.readFile(filePath);
                if (!content) {
                    this.importMappingCache.set(cacheKey, []);
                    return [];
                }
                const mappings = (0, import_resolver_1.extractImportMappings)(filePath, content, language);
                this.importMappingCache.set(cacheKey, mappings);
                return mappings;
            },
            getProjectAliases: () => {
                if (this.projectAliases === undefined) {
                    this.projectAliases = (0, path_aliases_1.loadProjectAliases)(this.projectRoot);
                }
                return this.projectAliases;
            },
            getReExports: (filePath, language) => {
                const cached = this.reExportCache.get(filePath);
                if (cached)
                    return cached;
                const content = this.context.readFile(filePath);
                if (!content) {
                    this.reExportCache.set(filePath, []);
                    return [];
                }
                const reExports = (0, import_resolver_1.extractReExports)(content, language);
                this.reExportCache.set(filePath, reExports);
                return reExports;
            },
        };
    }
    /**
     * Resolve all unresolved references
     */
    resolveAll(unresolvedRefs, onProgress) {
        // Pre-load all nodes into memory for fast lookups
        this.warmCaches();
        const resolved = [];
        const unresolved = [];
        const byMethod = {};
        // Convert to our internal format, using denormalized fields when available
        const refs = unresolvedRefs.map((ref) => ({
            fromNodeId: ref.fromNodeId,
            referenceName: ref.referenceName,
            referenceKind: ref.referenceKind,
            line: ref.line,
            column: ref.column,
            filePath: ref.filePath || this.getFilePathFromNodeId(ref.fromNodeId),
            language: ref.language || this.getLanguageFromNodeId(ref.fromNodeId),
        }));
        const total = refs.length;
        let lastReportedPercent = -1;
        for (let i = 0; i < refs.length; i++) {
            const ref = refs[i]; // Array index is guaranteed to be in bounds
            const result = this.resolveOne(ref);
            if (result) {
                resolved.push(result);
                byMethod[result.resolvedBy] = (byMethod[result.resolvedBy] || 0) + 1;
            }
            else {
                unresolved.push(ref);
            }
            // Report progress every 1% to avoid too many updates
            if (onProgress) {
                const currentPercent = Math.floor((i / total) * 100);
                if (currentPercent > lastReportedPercent) {
                    lastReportedPercent = currentPercent;
                    onProgress(i + 1, total);
                }
            }
        }
        // Final progress report
        if (onProgress && total > 0) {
            onProgress(total, total);
        }
        return {
            resolved,
            unresolved,
            stats: {
                total: refs.length,
                resolved: resolved.length,
                unresolved: unresolved.length,
                byMethod,
            },
        };
    }
    /**
     * Check if a reference name has any possible match in the codebase.
     * Uses the pre-built knownNames set to skip expensive resolution
     * for names that definitely don't exist as symbols.
     */
    hasAnyPossibleMatch(name) {
        if (!this.knownNames)
            return true; // no pre-filter available
        // Direct name match
        if (this.knownNames.has(name))
            return true;
        // For qualified names like "obj.method" or "Class::method", check the parts
        const dotIdx = name.indexOf('.');
        if (dotIdx > 0) {
            const receiver = name.substring(0, dotIdx);
            const member = name.substring(dotIdx + 1);
            if (this.knownNames.has(receiver) || this.knownNames.has(member))
                return true;
            // Also check capitalized receiver (instance-method resolution)
            const capitalized = receiver.charAt(0).toUpperCase() + receiver.slice(1);
            if (this.knownNames.has(capitalized))
                return true;
        }
        const colonIdx = name.indexOf('::');
        if (colonIdx > 0) {
            const receiver = name.substring(0, colonIdx);
            const member = name.substring(colonIdx + 2);
            if (this.knownNames.has(receiver) || this.knownNames.has(member))
                return true;
        }
        // For path-like references (e.g., "snippets/drawer-menu.liquid"), check the filename
        const slashIdx = name.lastIndexOf('/');
        if (slashIdx > 0) {
            const fileName = name.substring(slashIdx + 1);
            if (this.knownNames.has(fileName))
                return true;
        }
        return false;
    }
    /**
     * Does `ref.referenceName` match an import declared in its containing
     * file? Used as a pre-filter escape so re-export chain resolution
     * still gets a chance when the name has no project-wide declaration.
     */
    matchesAnyImport(ref) {
        const imports = this.context.getImportMappings(ref.filePath, ref.language);
        if (imports.length === 0)
            return false;
        for (const imp of imports) {
            if (imp.localName === ref.referenceName ||
                ref.referenceName.startsWith(imp.localName + '.')) {
                return true;
            }
        }
        return false;
    }
    /**
     * Resolve a single reference
     */
    resolveOne(ref) {
        // Skip built-in/external references
        if (this.isBuiltInOrExternal(ref)) {
            return null;
        }
        // Fast pre-filter: skip if no symbol with this name exists anywhere
        // AND the name doesn't match a local import. The import escape is
        // necessary because re-export rename chains (`import { login }
        // from './barrel'` where the barrel has `export { signIn as login }
        // from './auth'`) intentionally call a name that has no
        // declaration anywhere — only the renamed upstream symbol does.
        if (!this.hasAnyPossibleMatch(ref.referenceName) && !this.matchesAnyImport(ref)) {
            return null;
        }
        const candidates = [];
        // Strategy 1: Try framework-specific resolution
        for (const framework of this.frameworks) {
            const result = framework.resolve(ref, this.context);
            if (result) {
                if (result.confidence >= 0.9)
                    return result; // High confidence, return immediately
                candidates.push(result);
            }
        }
        // Strategy 2: Try import-based resolution
        const importResult = (0, import_resolver_1.resolveViaImport)(ref, this.context);
        if (importResult) {
            if (importResult.confidence >= 0.9)
                return importResult;
            candidates.push(importResult);
        }
        // Strategy 3: Try name matching
        const nameResult = (0, name_matcher_1.matchReference)(ref, this.context);
        if (nameResult) {
            candidates.push(nameResult);
        }
        if (candidates.length === 0)
            return null;
        // Return highest confidence candidate
        return candidates.reduce((best, curr) => curr.confidence > best.confidence ? curr : best);
    }
    /**
     * Create edges from resolved references
     */
    createEdges(resolved) {
        return resolved.map((ref) => {
            let kind = ref.original.referenceKind;
            // Promote "extends" to "implements" when a class/struct targets an interface
            if (kind === 'extends') {
                const targetNode = this.queries.getNodeById(ref.targetNodeId);
                if (targetNode && (targetNode.kind === 'interface' || targetNode.kind === 'protocol')) {
                    const sourceNode = this.queries.getNodeById(ref.original.fromNodeId);
                    if (sourceNode && sourceNode.kind !== 'interface' && sourceNode.kind !== 'protocol') {
                        kind = 'implements';
                    }
                }
            }
            // Promote "calls" to "instantiates" when the resolved target is a
            // class/struct. Languages without a `new` keyword (Python, Ruby)
            // express instantiation as `Foo()` — extraction can't tell that
            // apart from a function call without symbol info, but resolution
            // can: if `Foo` resolves to a class, the call IS an instantiation.
            if (kind === 'calls') {
                const targetNode = this.queries.getNodeById(ref.targetNodeId);
                if (targetNode && (targetNode.kind === 'class' || targetNode.kind === 'struct')) {
                    kind = 'instantiates';
                }
            }
            return {
                source: ref.original.fromNodeId,
                target: ref.targetNodeId,
                kind,
                line: ref.original.line,
                column: ref.original.column,
                metadata: {
                    confidence: ref.confidence,
                    resolvedBy: ref.resolvedBy,
                },
            };
        });
    }
    /**
     * Resolve and persist edges to database
     */
    resolveAndPersist(unresolvedRefs, onProgress) {
        const result = this.resolveAll(unresolvedRefs, onProgress);
        // Create edges from resolved references
        const edges = this.createEdges(result.resolved);
        // Insert edges into database
        if (edges.length > 0) {
            this.queries.insertEdges(edges);
        }
        // Clean up resolved refs from unresolved_refs table so metrics are accurate
        if (result.resolved.length > 0) {
            this.queries.deleteSpecificResolvedReferences(result.resolved.map((r) => ({
                fromNodeId: r.original.fromNodeId,
                referenceName: r.original.referenceName,
                referenceKind: r.original.referenceKind,
            })));
        }
        return result;
    }
    /**
     * Resolve and persist in batches to keep memory bounded.
     * Processes unresolved references in chunks, persisting edges and cleaning
     * up resolved refs after each batch to avoid accumulating large arrays.
     */
    async resolveAndPersistBatched(onProgress, batchSize = 5000) {
        this.warmCaches();
        const total = this.queries.getUnresolvedReferencesCount();
        let processed = 0;
        const aggregateStats = {
            total: 0,
            resolved: 0,
            unresolved: 0,
            byMethod: {},
        };
        // Process in batches. We always read from offset 0 because resolved refs
        // are deleted after each batch, shifting the remaining rows forward.
        while (true) {
            const batch = this.queries.getUnresolvedReferencesBatch(0, batchSize);
            if (batch.length === 0)
                break;
            const result = this.resolveAll(batch);
            // Persist edges immediately
            const edges = this.createEdges(result.resolved);
            if (edges.length > 0) {
                this.queries.insertEdges(edges);
            }
            // Clean up resolved refs so they don't appear in the next batch
            if (result.resolved.length > 0) {
                this.queries.deleteSpecificResolvedReferences(result.resolved.map((r) => ({
                    fromNodeId: r.original.fromNodeId,
                    referenceName: r.original.referenceName,
                    referenceKind: r.original.referenceKind,
                })));
            }
            // Delete unresolvable refs from this batch to avoid re-processing them
            if (result.unresolved.length > 0) {
                this.queries.deleteSpecificResolvedReferences(result.unresolved.map((r) => ({
                    fromNodeId: r.fromNodeId,
                    referenceName: r.referenceName,
                    referenceKind: r.referenceKind,
                })));
            }
            // Aggregate stats
            aggregateStats.total += result.stats.total;
            aggregateStats.resolved += result.stats.resolved;
            aggregateStats.unresolved += result.stats.unresolved;
            for (const [method, count] of Object.entries(result.stats.byMethod)) {
                aggregateStats.byMethod[method] = (aggregateStats.byMethod[method] || 0) + count;
            }
            processed += batch.length;
            onProgress?.(processed, total);
            // Yield so progress UI can render between batches
            await new Promise(resolve => setImmediate(resolve));
            // If nothing was resolved or removed in this batch, we'd loop forever
            // on the same rows. Break to avoid infinite loop.
            if (result.resolved.length === 0 && result.unresolved.length === batch.length) {
                break;
            }
        }
        return {
            resolved: [],
            unresolved: [],
            stats: aggregateStats,
        };
    }
    /**
     * Get detected frameworks
     */
    getDetectedFrameworks() {
        return this.frameworks.map((f) => f.name);
    }
    /**
     * Check if reference is to a built-in or external symbol
     */
    isBuiltInOrExternal(ref) {
        const name = ref.referenceName;
        const isJsTs = ref.language === 'typescript' || ref.language === 'javascript'
            || ref.language === 'tsx' || ref.language === 'jsx';
        // JavaScript/TypeScript built-ins
        if (isJsTs && JS_BUILT_INS.has(name)) {
            return true;
        }
        // Common JS/TS library calls (console.log, Math.floor, JSON.parse)
        if (isJsTs && (name.startsWith('console.') || name.startsWith('Math.') || name.startsWith('JSON.'))) {
            return true;
        }
        // React hooks from React itself
        if (isJsTs && REACT_HOOKS.has(name)) {
            return true;
        }
        // Python built-ins (bare calls only — dotted calls like console.print are method calls)
        if (ref.language === 'python' && PYTHON_BUILT_INS.has(name)) {
            return true;
        }
        // Python built-in method calls (e.g., list.extend, dict.update)
        if (ref.language === 'python') {
            const dotIdx = name.indexOf('.');
            if (dotIdx > 0) {
                const receiver = name.substring(0, dotIdx);
                const method = name.substring(dotIdx + 1);
                // Filter calls on built-in types (list.append, dict.update, etc.)
                if (PYTHON_BUILT_IN_TYPES.has(receiver)) {
                    return true;
                }
                // Filter built-in methods on non-class receivers
                // (e.g., items.append where items is a local list variable)
                // But allow if the capitalized receiver matches a known codebase class
                if (PYTHON_BUILT_IN_METHODS.has(method)) {
                    const capitalized = receiver.charAt(0).toUpperCase() + receiver.slice(1);
                    if (!this.knownNames?.has(capitalized)) {
                        return true;
                    }
                }
            }
            if (PYTHON_BUILT_IN_METHODS.has(name)) {
                return true;
            }
        }
        // Go standard library packages — refs like "fmt.Println", "http.ListenAndServe", etc.
        if (ref.language === 'go') {
            const dotIdx = name.indexOf('.');
            if (dotIdx > 0) {
                const pkg = name.substring(0, dotIdx);
                if (GO_STDLIB_PACKAGES.has(pkg)) {
                    return true;
                }
            }
            if (GO_BUILT_INS.has(name)) {
                return true;
            }
        }
        // Pascal/Delphi built-ins and standard library units
        if (ref.language === 'pascal') {
            if (PASCAL_UNIT_PREFIXES.some((p) => name.startsWith(p))) {
                return true;
            }
            if (PASCAL_BUILT_INS.has(name)) {
                return true;
            }
        }
        return false;
    }
    /**
     * Get file path from node ID
     */
    getFilePathFromNodeId(nodeId) {
        const node = this.queries.getNodeById(nodeId);
        return node?.filePath || '';
    }
    /**
     * Get language from node ID
     */
    getLanguageFromNodeId(nodeId) {
        const node = this.queries.getNodeById(nodeId);
        return node?.language || 'unknown';
    }
}
exports.ReferenceResolver = ReferenceResolver;
/**
 * Create a reference resolver instance
 */
function createResolver(projectRoot, queries) {
    const resolver = new ReferenceResolver(projectRoot, queries);
    resolver.initialize();
    return resolver;
}
//# sourceMappingURL=index.js.map