"use strict";
/**
 * CodeGraph
 *
 * A local-first code intelligence system that builds a semantic
 * knowledge graph from any codebase.
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
exports.CodeGraph = exports.MCPServer = exports.FileWatcher = exports.MemoryMonitor = exports.throttle = exports.debounce = exports.processInBatches = exports.FileLock = exports.Mutex = exports.defaultLogger = exports.silentLogger = exports.getLogger = exports.setLogger = exports.ConfigError = exports.VectorError = exports.SearchError = exports.DatabaseError = exports.ParseError = exports.FileError = exports.CodeGraphError = exports.loadAllGrammars = exports.loadGrammarsForLanguages = exports.initGrammars = exports.getSupportedLanguages = exports.isGrammarLoaded = exports.isLanguageSupported = exports.detectLanguage = exports.CODEGRAPH_DIR = exports.findNearestCodeGraphRoot = exports.isInitialized = exports.getCodeGraphDir = exports.getDatabasePath = void 0;
const path = __importStar(require("path"));
const db_1 = require("./db");
const queries_1 = require("./db/queries");
const directory_1 = require("./directory");
const extraction_1 = require("./extraction");
const resolution_1 = require("./resolution");
const graph_1 = require("./graph");
const context_1 = require("./context");
const utils_1 = require("./utils");
const sync_1 = require("./sync");
// Re-export types for consumers
__exportStar(require("./types"), exports);
var db_2 = require("./db");
Object.defineProperty(exports, "getDatabasePath", { enumerable: true, get: function () { return db_2.getDatabasePath; } });
var directory_2 = require("./directory");
Object.defineProperty(exports, "getCodeGraphDir", { enumerable: true, get: function () { return directory_2.getCodeGraphDir; } });
Object.defineProperty(exports, "isInitialized", { enumerable: true, get: function () { return directory_2.isInitialized; } });
Object.defineProperty(exports, "findNearestCodeGraphRoot", { enumerable: true, get: function () { return directory_2.findNearestCodeGraphRoot; } });
Object.defineProperty(exports, "CODEGRAPH_DIR", { enumerable: true, get: function () { return directory_2.CODEGRAPH_DIR; } });
var extraction_2 = require("./extraction");
Object.defineProperty(exports, "detectLanguage", { enumerable: true, get: function () { return extraction_2.detectLanguage; } });
Object.defineProperty(exports, "isLanguageSupported", { enumerable: true, get: function () { return extraction_2.isLanguageSupported; } });
Object.defineProperty(exports, "isGrammarLoaded", { enumerable: true, get: function () { return extraction_2.isGrammarLoaded; } });
Object.defineProperty(exports, "getSupportedLanguages", { enumerable: true, get: function () { return extraction_2.getSupportedLanguages; } });
Object.defineProperty(exports, "initGrammars", { enumerable: true, get: function () { return extraction_2.initGrammars; } });
Object.defineProperty(exports, "loadGrammarsForLanguages", { enumerable: true, get: function () { return extraction_2.loadGrammarsForLanguages; } });
Object.defineProperty(exports, "loadAllGrammars", { enumerable: true, get: function () { return extraction_2.loadAllGrammars; } });
var errors_1 = require("./errors");
Object.defineProperty(exports, "CodeGraphError", { enumerable: true, get: function () { return errors_1.CodeGraphError; } });
Object.defineProperty(exports, "FileError", { enumerable: true, get: function () { return errors_1.FileError; } });
Object.defineProperty(exports, "ParseError", { enumerable: true, get: function () { return errors_1.ParseError; } });
Object.defineProperty(exports, "DatabaseError", { enumerable: true, get: function () { return errors_1.DatabaseError; } });
Object.defineProperty(exports, "SearchError", { enumerable: true, get: function () { return errors_1.SearchError; } });
Object.defineProperty(exports, "VectorError", { enumerable: true, get: function () { return errors_1.VectorError; } });
Object.defineProperty(exports, "ConfigError", { enumerable: true, get: function () { return errors_1.ConfigError; } });
Object.defineProperty(exports, "setLogger", { enumerable: true, get: function () { return errors_1.setLogger; } });
Object.defineProperty(exports, "getLogger", { enumerable: true, get: function () { return errors_1.getLogger; } });
Object.defineProperty(exports, "silentLogger", { enumerable: true, get: function () { return errors_1.silentLogger; } });
Object.defineProperty(exports, "defaultLogger", { enumerable: true, get: function () { return errors_1.defaultLogger; } });
var utils_2 = require("./utils");
Object.defineProperty(exports, "Mutex", { enumerable: true, get: function () { return utils_2.Mutex; } });
Object.defineProperty(exports, "FileLock", { enumerable: true, get: function () { return utils_2.FileLock; } });
Object.defineProperty(exports, "processInBatches", { enumerable: true, get: function () { return utils_2.processInBatches; } });
Object.defineProperty(exports, "debounce", { enumerable: true, get: function () { return utils_2.debounce; } });
Object.defineProperty(exports, "throttle", { enumerable: true, get: function () { return utils_2.throttle; } });
Object.defineProperty(exports, "MemoryMonitor", { enumerable: true, get: function () { return utils_2.MemoryMonitor; } });
var sync_2 = require("./sync");
Object.defineProperty(exports, "FileWatcher", { enumerable: true, get: function () { return sync_2.FileWatcher; } });
var mcp_1 = require("./mcp");
Object.defineProperty(exports, "MCPServer", { enumerable: true, get: function () { return mcp_1.MCPServer; } });
/**
 * Main CodeGraph class
 *
 * Provides the primary interface for interacting with the code knowledge graph.
 */
class CodeGraph {
    db;
    queries;
    projectRoot;
    orchestrator;
    resolver;
    graphManager;
    traverser;
    contextBuilder;
    // Mutex for preventing concurrent indexing operations (in-process)
    indexMutex = new utils_1.Mutex();
    // File lock for preventing concurrent writes across processes (CLI, MCP, git hooks)
    fileLock;
    // File watcher for auto-sync on file changes
    watcher = null;
    constructor(db, queries, projectRoot) {
        this.db = db;
        this.queries = queries;
        this.projectRoot = projectRoot;
        this.fileLock = new utils_1.FileLock(path.join(projectRoot, '.codegraph', 'codegraph.lock'));
        this.orchestrator = new extraction_1.ExtractionOrchestrator(projectRoot, queries);
        this.resolver = (0, resolution_1.createResolver)(projectRoot, queries);
        this.graphManager = new graph_1.GraphQueryManager(queries);
        this.traverser = new graph_1.GraphTraverser(queries);
        this.contextBuilder = (0, context_1.createContextBuilder)(projectRoot, queries, this.traverser);
    }
    // ===========================================================================
    // Lifecycle Methods
    // ===========================================================================
    /**
     * Initialize a new CodeGraph project
     *
     * Creates the .CodeGraph directory, database, and configuration.
     *
     * @param projectRoot - Path to the project root directory
     * @param options - Initialization options
     * @returns A new CodeGraph instance
     */
    static async init(projectRoot, options = {}) {
        await (0, extraction_1.initGrammars)();
        const resolvedRoot = path.resolve(projectRoot);
        // Check if already initialized
        if ((0, directory_1.isInitialized)(resolvedRoot)) {
            throw new Error(`CodeGraph already initialized in ${resolvedRoot}`);
        }
        // Create directory structure
        (0, directory_1.createDirectory)(resolvedRoot);
        // Initialize database
        const dbPath = (0, db_1.getDatabasePath)(resolvedRoot);
        const db = db_1.DatabaseConnection.initialize(dbPath);
        const queries = new queries_1.QueryBuilder(db.getDb());
        const instance = new CodeGraph(db, queries, resolvedRoot);
        // Run initial indexing if requested
        if (options.index) {
            await instance.indexAll({ onProgress: options.onProgress });
        }
        return instance;
    }
    /**
     * Initialize synchronously (without indexing)
     */
    static initSync(projectRoot) {
        const resolvedRoot = path.resolve(projectRoot);
        // Check if already initialized
        if ((0, directory_1.isInitialized)(resolvedRoot)) {
            throw new Error(`CodeGraph already initialized in ${resolvedRoot}`);
        }
        // Create directory structure
        (0, directory_1.createDirectory)(resolvedRoot);
        // Initialize database
        const dbPath = (0, db_1.getDatabasePath)(resolvedRoot);
        const db = db_1.DatabaseConnection.initialize(dbPath);
        const queries = new queries_1.QueryBuilder(db.getDb());
        return new CodeGraph(db, queries, resolvedRoot);
    }
    /**
     * Open an existing CodeGraph project
     *
     * @param projectRoot - Path to the project root directory
     * @param options - Open options
     * @returns A CodeGraph instance
     */
    static async open(projectRoot, options = {}) {
        await (0, extraction_1.initGrammars)();
        const resolvedRoot = path.resolve(projectRoot);
        // Check if initialized
        if (!(0, directory_1.isInitialized)(resolvedRoot)) {
            throw new Error(`CodeGraph not initialized in ${resolvedRoot}. Run init() first.`);
        }
        // Validate directory structure
        const validation = (0, directory_1.validateDirectory)(resolvedRoot);
        if (!validation.valid) {
            throw new Error(`Invalid CodeGraph directory: ${validation.errors.join(', ')}`);
        }
        // Open database
        const dbPath = (0, db_1.getDatabasePath)(resolvedRoot);
        const db = db_1.DatabaseConnection.open(dbPath);
        const queries = new queries_1.QueryBuilder(db.getDb());
        const instance = new CodeGraph(db, queries, resolvedRoot);
        // Sync if requested
        if (options.sync) {
            await instance.sync();
        }
        return instance;
    }
    /**
     * Open synchronously (without sync)
     */
    static openSync(projectRoot) {
        const resolvedRoot = path.resolve(projectRoot);
        // Check if initialized
        if (!(0, directory_1.isInitialized)(resolvedRoot)) {
            throw new Error(`CodeGraph not initialized in ${resolvedRoot}. Run init() first.`);
        }
        // Validate directory structure
        const validation = (0, directory_1.validateDirectory)(resolvedRoot);
        if (!validation.valid) {
            throw new Error(`Invalid CodeGraph directory: ${validation.errors.join(', ')}`);
        }
        // Open database
        const dbPath = (0, db_1.getDatabasePath)(resolvedRoot);
        const db = db_1.DatabaseConnection.open(dbPath);
        const queries = new queries_1.QueryBuilder(db.getDb());
        return new CodeGraph(db, queries, resolvedRoot);
    }
    /**
     * Check if a directory has been initialized as a CodeGraph project
     */
    static isInitialized(projectRoot) {
        return (0, directory_1.isInitialized)(path.resolve(projectRoot));
    }
    /**
     * Close the CodeGraph instance and release resources
     */
    close() {
        this.unwatch();
        // Release file lock if held
        this.fileLock.release();
        this.db.close();
    }
    /**
     * Get the project root directory
     */
    getProjectRoot() {
        return this.projectRoot;
    }
    // ===========================================================================
    // Indexing
    // ===========================================================================
    /**
     * Index all files in the project
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    async indexAll(options = {}) {
        return this.indexMutex.withLock(async () => {
            try {
                this.fileLock.acquire();
            }
            catch {
                return { success: false, filesIndexed: 0, filesSkipped: 0, filesErrored: 0, nodesCreated: 0, edgesCreated: 0, errors: [{ message: 'Could not acquire file lock - another process may be indexing', severity: 'error' }], durationMs: 0 };
            }
            try {
                const result = await this.orchestrator.indexAll(options.onProgress, options.signal, options.verbose);
                // Resolve references to create call/import/extends edges
                if (result.success && result.filesIndexed > 0) {
                    // Get count without loading all refs into memory
                    const unresolvedCount = this.queries.getUnresolvedReferencesCount();
                    options.onProgress?.({
                        phase: 'resolving',
                        current: 0,
                        total: unresolvedCount,
                    });
                    await this.resolveReferencesBatched((current, total) => {
                        options.onProgress?.({
                            phase: 'resolving',
                            current,
                            total,
                        });
                    });
                }
                // Refresh planner stats + checkpoint the WAL after bulk writes.
                // Cheap and non-blocking; never load-bearing for correctness.
                if (result.success && result.filesIndexed > 0) {
                    this.db.runMaintenance();
                }
                return result;
            }
            finally {
                this.fileLock.release();
            }
        });
    }
    /**
     * Index specific files
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    async indexFiles(filePaths) {
        return this.indexMutex.withLock(async () => {
            try {
                this.fileLock.acquire();
            }
            catch {
                return { success: false, filesIndexed: 0, filesSkipped: 0, filesErrored: 0, nodesCreated: 0, edgesCreated: 0, errors: [{ message: 'Could not acquire file lock - another process may be indexing', severity: 'error' }], durationMs: 0 };
            }
            try {
                return this.orchestrator.indexFiles(filePaths);
            }
            finally {
                this.fileLock.release();
            }
        });
    }
    /**
     * Sync with current file state (incremental update)
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    async sync(options = {}) {
        return this.indexMutex.withLock(async () => {
            try {
                this.fileLock.acquire();
            }
            catch {
                return { filesChecked: 0, filesAdded: 0, filesModified: 0, filesRemoved: 0, nodesUpdated: 0, durationMs: 0 };
            }
            try {
                const result = await this.orchestrator.sync(options.onProgress);
                // Resolve references if files were updated
                if (result.filesAdded > 0 || result.filesModified > 0) {
                    if (result.changedFilePaths) {
                        // Scope resolution to changed files (git fast path — bounded set)
                        const unresolvedRefs = this.queries.getUnresolvedReferencesByFiles(result.changedFilePaths);
                        options.onProgress?.({
                            phase: 'resolving',
                            current: 0,
                            total: unresolvedRefs.length,
                        });
                        this.resolver.resolveAndPersist(unresolvedRefs, (current, total) => {
                            options.onProgress?.({
                                phase: 'resolving',
                                current,
                                total,
                            });
                        });
                    }
                    else {
                        // No git info — use batched resolution to avoid OOM
                        const unresolvedCount = this.queries.getUnresolvedReferencesCount();
                        options.onProgress?.({
                            phase: 'resolving',
                            current: 0,
                            total: unresolvedCount,
                        });
                        await this.resolveReferencesBatched((current, total) => {
                            options.onProgress?.({
                                phase: 'resolving',
                                current,
                                total,
                            });
                        });
                    }
                }
                // Refresh planner stats + checkpoint the WAL after bulk writes.
                if (result.filesAdded > 0 || result.filesModified > 0 || result.filesRemoved > 0) {
                    this.db.runMaintenance();
                }
                return result;
            }
            finally {
                this.fileLock.release();
            }
        });
    }
    /**
     * Check if an indexing operation is currently in progress
     */
    isIndexing() {
        return this.indexMutex.isLocked();
    }
    // ===========================================================================
    // File Watching
    // ===========================================================================
    /**
     * Start watching for file changes and auto-syncing.
     *
     * Uses native OS file events (FSEvents on macOS, inotify on Linux 19+,
     * ReadDirectoryChangesW on Windows) with debouncing to avoid thrashing.
     *
     * @param options - Watch options (debounce delay, callbacks)
     * @returns true if watching started successfully
     */
    watch(options = {}) {
        if (this.watcher?.isActive())
            return true;
        this.watcher = new sync_1.FileWatcher(this.projectRoot, async () => {
            const result = await this.sync();
            const filesChanged = result.filesAdded + result.filesModified + result.filesRemoved;
            return { filesChanged, durationMs: result.durationMs };
        }, options);
        return this.watcher.start();
    }
    /**
     * Stop watching for file changes.
     */
    unwatch() {
        if (this.watcher) {
            this.watcher.stop();
            this.watcher = null;
        }
    }
    /**
     * Check if the file watcher is active.
     */
    isWatching() {
        return this.watcher?.isActive() ?? false;
    }
    /**
     * Get files that have changed since last index
     */
    getChangedFiles() {
        return this.orchestrator.getChangedFiles();
    }
    /**
     * Extract nodes and edges from source code (without storing)
     */
    extractFromSource(filePath, source) {
        return (0, extraction_1.extractFromSource)(filePath, source);
    }
    // ===========================================================================
    // Reference Resolution
    // ===========================================================================
    /**
     * Resolve unresolved references and create edges
     *
     * This method takes unresolved references from extraction and attempts
     * to resolve them using multiple strategies:
     * - Framework-specific patterns (React, Express, Laravel)
     * - Import-based resolution
     * - Name-based symbol matching
     */
    resolveReferences(onProgress) {
        // Get all unresolved references from the database
        const unresolvedRefs = this.queries.getUnresolvedReferences();
        return this.resolver.resolveAndPersist(unresolvedRefs, onProgress);
    }
    /**
     * Resolve references in batches to keep memory bounded on large codebases.
     * Processes chunks of unresolved refs, persisting results after each batch.
     */
    async resolveReferencesBatched(onProgress) {
        return this.resolver.resolveAndPersistBatched(onProgress);
    }
    /**
     * Get detected frameworks in the project
     */
    getDetectedFrameworks() {
        return this.resolver.getDetectedFrameworks();
    }
    /**
     * Re-initialize the resolver (useful after adding new files)
     */
    reinitializeResolver() {
        this.resolver.initialize();
    }
    // ===========================================================================
    // Graph Statistics
    // ===========================================================================
    /**
     * Get statistics about the knowledge graph
     */
    getStats() {
        const stats = this.queries.getStats();
        stats.dbSizeBytes = this.db.getSize();
        return stats;
    }
    /**
     * Active SQLite backend for this project's connection (`node-sqlite` — Node's
     * built-in real-SQLite module). Surfaced via `codegraph status` and the
     * `codegraph_status` MCP tool alongside the effective journal mode.
     */
    getBackend() {
        return this.db.getBackend();
    }
    /**
     * The journal mode actually in effect ('wal', 'delete', …). 'wal' means
     * readers never block on a concurrent writer; anything else means they can,
     * which is the precondition for the "database is locked" failures in issue
     * #238. Surfaced via `codegraph status` and the `codegraph_status` MCP tool.
     */
    getJournalMode() {
        return this.db.getJournalMode();
    }
    // ===========================================================================
    // Node Operations
    // ===========================================================================
    /**
     * Get a node by ID
     */
    getNode(id) {
        return this.queries.getNodeById(id);
    }
    /**
     * Get all nodes in a file
     */
    getNodesInFile(filePath) {
        return this.queries.getNodesByFile(filePath);
    }
    /**
     * Get all nodes of a specific kind
     */
    getNodesByKind(kind) {
        return this.queries.getNodesByKind(kind);
    }
    /**
     * Search nodes by text
     */
    searchNodes(query, options) {
        return this.queries.searchNodes(query, options);
    }
    // ===========================================================================
    // Edge Operations
    // ===========================================================================
    /**
     * Get outgoing edges from a node
     */
    getOutgoingEdges(nodeId) {
        return this.queries.getOutgoingEdges(nodeId);
    }
    /**
     * Get incoming edges to a node
     */
    getIncomingEdges(nodeId) {
        return this.queries.getIncomingEdges(nodeId);
    }
    // ===========================================================================
    // File Operations
    // ===========================================================================
    /**
     * Get a file record by path
     */
    getFile(filePath) {
        return this.queries.getFileByPath(filePath);
    }
    /**
     * Get all tracked files
     */
    getFiles() {
        return this.queries.getAllFiles();
    }
    // ===========================================================================
    // Graph Query Methods
    // ===========================================================================
    /**
     * Get the context for a node (ancestors, children, references)
     *
     * Returns comprehensive context about a node including its containment
     * hierarchy, children, incoming/outgoing references, type information,
     * and relevant imports.
     *
     * @param nodeId - ID of the focal node
     * @returns Context object with all related information
     */
    getContext(nodeId) {
        return this.graphManager.getContext(nodeId);
    }
    /**
     * Traverse the graph from a starting node
     *
     * Uses breadth-first search by default. Supports filtering by edge types,
     * node types, and traversal direction.
     *
     * @param startId - Starting node ID
     * @param options - Traversal options
     * @returns Subgraph containing traversed nodes and edges
     */
    traverse(startId, options) {
        return this.traverser.traverseBFS(startId, options);
    }
    /**
     * Get the call graph for a function
     *
     * Returns both callers (functions that call this function) and
     * callees (functions called by this function) up to the specified depth.
     *
     * @param nodeId - ID of the function/method node
     * @param depth - Maximum depth in each direction (default: 2)
     * @returns Subgraph containing the call graph
     */
    getCallGraph(nodeId, depth = 2) {
        return this.traverser.getCallGraph(nodeId, depth);
    }
    /**
     * Get the type hierarchy for a class/interface
     *
     * Returns both ancestors (types this extends/implements) and
     * descendants (types that extend/implement this).
     *
     * @param nodeId - ID of the class/interface node
     * @returns Subgraph containing the type hierarchy
     */
    getTypeHierarchy(nodeId) {
        return this.traverser.getTypeHierarchy(nodeId);
    }
    /**
     * Find all usages of a symbol
     *
     * Returns all nodes that reference the specified symbol through
     * any edge type (calls, references, type_of, etc.).
     *
     * @param nodeId - ID of the symbol node
     * @returns Array of nodes and edges that reference this symbol
     */
    findUsages(nodeId) {
        return this.traverser.findUsages(nodeId);
    }
    /**
     * Get callers of a function/method
     *
     * @param nodeId - ID of the function/method node
     * @param maxDepth - Maximum depth to traverse (default: 1)
     * @returns Array of nodes that call this function
     */
    getCallers(nodeId, maxDepth = 1) {
        return this.traverser.getCallers(nodeId, maxDepth);
    }
    /**
     * Get callees of a function/method
     *
     * @param nodeId - ID of the function/method node
     * @param maxDepth - Maximum depth to traverse (default: 1)
     * @returns Array of nodes called by this function
     */
    getCallees(nodeId, maxDepth = 1) {
        return this.traverser.getCallees(nodeId, maxDepth);
    }
    /**
     * Calculate the impact radius of a node
     *
     * Returns all nodes that could be affected by changes to this node.
     *
     * @param nodeId - ID of the node
     * @param maxDepth - Maximum depth to traverse (default: 3)
     * @returns Subgraph containing potentially impacted nodes
     */
    getImpactRadius(nodeId, maxDepth = 3) {
        return this.traverser.getImpactRadius(nodeId, maxDepth);
    }
    /**
     * Find the shortest path between two nodes
     *
     * @param fromId - Starting node ID
     * @param toId - Target node ID
     * @param edgeKinds - Edge types to consider (all if empty)
     * @returns Array of nodes and edges forming the path, or null if no path exists
     */
    findPath(fromId, toId, edgeKinds) {
        return this.traverser.findPath(fromId, toId, edgeKinds);
    }
    /**
     * Get ancestors of a node in the containment hierarchy
     *
     * @param nodeId - ID of the node
     * @returns Array of ancestor nodes from immediate parent to root
     */
    getAncestors(nodeId) {
        return this.traverser.getAncestors(nodeId);
    }
    /**
     * Get immediate children of a node
     *
     * @param nodeId - ID of the node
     * @returns Array of child nodes
     */
    getChildren(nodeId) {
        return this.traverser.getChildren(nodeId);
    }
    /**
     * Get dependencies of a file
     *
     * @param filePath - Path to the file
     * @returns Array of file paths this file depends on
     */
    getFileDependencies(filePath) {
        return this.graphManager.getFileDependencies(filePath);
    }
    /**
     * Get dependents of a file
     *
     * @param filePath - Path to the file
     * @returns Array of file paths that depend on this file
     */
    getFileDependents(filePath) {
        return this.graphManager.getFileDependents(filePath);
    }
    /**
     * Find circular dependencies in the codebase
     *
     * @returns Array of cycles, each cycle is an array of file paths
     */
    findCircularDependencies() {
        return this.graphManager.findCircularDependencies();
    }
    /**
     * Find dead code (unreferenced symbols)
     *
     * @param kinds - Node kinds to check (default: functions, methods, classes)
     * @returns Array of unreferenced nodes
     */
    findDeadCode(kinds) {
        return this.graphManager.findDeadCode(kinds);
    }
    /**
     * Get complexity metrics for a node
     *
     * @param nodeId - ID of the node
     * @returns Object containing various complexity metrics
     */
    getNodeMetrics(nodeId) {
        return this.graphManager.getNodeMetrics(nodeId);
    }
    // ===========================================================================
    // Context Building
    // ===========================================================================
    /**
     * Get the source code for a node
     *
     * Reads the file and extracts the code between startLine and endLine.
     *
     * @param nodeId - ID of the node
     * @returns Code string or null if not found
     */
    async getCode(nodeId) {
        return this.contextBuilder.getCode(nodeId);
    }
    /**
     * Find relevant subgraph for a query
     *
     * Combines semantic search with graph traversal to find the most
     * relevant nodes and their relationships for a given query.
     *
     * @param query - Natural language query describing the task
     * @param options - Search and traversal options
     * @returns Subgraph of relevant nodes and edges
     */
    async findRelevantContext(query, options) {
        return this.contextBuilder.findRelevantContext(query, options);
    }
    /**
     * Build context for a task
     *
     * Creates comprehensive context by:
     * 1. Running FTS search to find entry points
     * 2. Expanding the graph around entry points
     * 3. Extracting code blocks for key nodes
     * 4. Formatting output for Claude
     *
     * @param input - Task description (string or {title, description})
     * @param options - Build options (maxNodes, includeCode, format, etc.)
     * @returns TaskContext object or formatted string (markdown/JSON)
     */
    async buildContext(input, options) {
        return this.contextBuilder.buildContext(input, options);
    }
    // ===========================================================================
    // Database Management
    // ===========================================================================
    /**
     * Optimize the database (vacuum and analyze)
     */
    optimize() {
        this.db.optimize();
    }
    /**
     * Clear all data from the graph
     */
    clear() {
        this.queries.clear();
    }
    /**
     * Alias for close() for backwards compatibility.
     * @deprecated Use close() instead
     */
    destroy() {
        this.close();
    }
    /**
     * Completely remove CodeGraph from the project.
     * This closes the database and deletes the .CodeGraph directory.
     *
     * WARNING: This permanently deletes all CodeGraph data for the project.
     */
    uninitialize() {
        this.close();
        (0, directory_1.removeDirectory)(this.projectRoot);
    }
}
exports.CodeGraph = CodeGraph;
// Default export
exports.default = CodeGraph;
//# sourceMappingURL=index.js.map