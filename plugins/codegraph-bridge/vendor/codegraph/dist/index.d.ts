/**
 * CodeGraph
 *
 * A local-first code intelligence system that builds a semantic
 * knowledge graph from any codebase.
 */
import { Node, Edge, FileRecord, ExtractionResult, Subgraph, TraversalOptions, SearchOptions, SearchResult, Context, GraphStats, TaskInput, TaskContext, BuildContextOptions, FindRelevantContextOptions } from './types';
import { IndexProgress, IndexResult, SyncResult } from './extraction';
import { ResolutionResult } from './resolution';
import { WatchOptions } from './sync';
export * from './types';
export { getDatabasePath } from './db';
export { getCodeGraphDir, isInitialized, findNearestCodeGraphRoot, CODEGRAPH_DIR, } from './directory';
export { IndexProgress, IndexResult, SyncResult } from './extraction';
export { detectLanguage, isLanguageSupported, isGrammarLoaded, getSupportedLanguages, initGrammars, loadGrammarsForLanguages, loadAllGrammars } from './extraction';
export { ResolutionResult } from './resolution';
export { CodeGraphError, FileError, ParseError, DatabaseError, SearchError, VectorError, ConfigError, Logger, setLogger, getLogger, silentLogger, defaultLogger, } from './errors';
export { Mutex, FileLock, processInBatches, debounce, throttle, MemoryMonitor } from './utils';
export { FileWatcher, WatchOptions } from './sync';
export { MCPServer } from './mcp';
/**
 * Options for initializing a new CodeGraph project
 */
export interface InitOptions {
    /** Whether to run initial indexing after init */
    index?: boolean;
    /** Progress callback for indexing */
    onProgress?: (progress: IndexProgress) => void;
}
/**
 * Options for opening an existing CodeGraph project
 */
export interface OpenOptions {
    /** Whether to run sync if files have changed */
    sync?: boolean;
    /** Whether to run in read-only mode */
    readOnly?: boolean;
}
/**
 * Options for indexing
 */
export interface IndexOptions {
    /** Progress callback */
    onProgress?: (progress: IndexProgress) => void;
    /** Abort signal for cancellation */
    signal?: AbortSignal;
    /** Enable verbose logging (worker lifecycle, memory, timeouts) */
    verbose?: boolean;
}
/**
 * Main CodeGraph class
 *
 * Provides the primary interface for interacting with the code knowledge graph.
 */
export declare class CodeGraph {
    private db;
    private queries;
    private projectRoot;
    private orchestrator;
    private resolver;
    private graphManager;
    private traverser;
    private contextBuilder;
    private indexMutex;
    private fileLock;
    private watcher;
    private constructor();
    /**
     * Initialize a new CodeGraph project
     *
     * Creates the .CodeGraph directory, database, and configuration.
     *
     * @param projectRoot - Path to the project root directory
     * @param options - Initialization options
     * @returns A new CodeGraph instance
     */
    static init(projectRoot: string, options?: InitOptions): Promise<CodeGraph>;
    /**
     * Initialize synchronously (without indexing)
     */
    static initSync(projectRoot: string): CodeGraph;
    /**
     * Open an existing CodeGraph project
     *
     * @param projectRoot - Path to the project root directory
     * @param options - Open options
     * @returns A CodeGraph instance
     */
    static open(projectRoot: string, options?: OpenOptions): Promise<CodeGraph>;
    /**
     * Open synchronously (without sync)
     */
    static openSync(projectRoot: string): CodeGraph;
    /**
     * Check if a directory has been initialized as a CodeGraph project
     */
    static isInitialized(projectRoot: string): boolean;
    /**
     * Close the CodeGraph instance and release resources
     */
    close(): void;
    /**
     * Get the project root directory
     */
    getProjectRoot(): string;
    /**
     * Index all files in the project
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    indexAll(options?: IndexOptions): Promise<IndexResult>;
    /**
     * Index specific files
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    indexFiles(filePaths: string[]): Promise<IndexResult>;
    /**
     * Sync with current file state (incremental update)
     *
     * Uses a mutex to prevent concurrent indexing operations.
     */
    sync(options?: IndexOptions): Promise<SyncResult>;
    /**
     * Check if an indexing operation is currently in progress
     */
    isIndexing(): boolean;
    /**
     * Start watching for file changes and auto-syncing.
     *
     * Uses native OS file events (FSEvents on macOS, inotify on Linux 19+,
     * ReadDirectoryChangesW on Windows) with debouncing to avoid thrashing.
     *
     * @param options - Watch options (debounce delay, callbacks)
     * @returns true if watching started successfully
     */
    watch(options?: WatchOptions): boolean;
    /**
     * Stop watching for file changes.
     */
    unwatch(): void;
    /**
     * Check if the file watcher is active.
     */
    isWatching(): boolean;
    /**
     * Get files that have changed since last index
     */
    getChangedFiles(): {
        added: string[];
        modified: string[];
        removed: string[];
    };
    /**
     * Extract nodes and edges from source code (without storing)
     */
    extractFromSource(filePath: string, source: string): ExtractionResult;
    /**
     * Resolve unresolved references and create edges
     *
     * This method takes unresolved references from extraction and attempts
     * to resolve them using multiple strategies:
     * - Framework-specific patterns (React, Express, Laravel)
     * - Import-based resolution
     * - Name-based symbol matching
     */
    resolveReferences(onProgress?: (current: number, total: number) => void): ResolutionResult;
    /**
     * Resolve references in batches to keep memory bounded on large codebases.
     * Processes chunks of unresolved refs, persisting results after each batch.
     */
    resolveReferencesBatched(onProgress?: (current: number, total: number) => void): Promise<ResolutionResult>;
    /**
     * Get detected frameworks in the project
     */
    getDetectedFrameworks(): string[];
    /**
     * Re-initialize the resolver (useful after adding new files)
     */
    reinitializeResolver(): void;
    /**
     * Get statistics about the knowledge graph
     */
    getStats(): GraphStats;
    /**
     * Active SQLite backend for this project's connection (`node-sqlite` — Node's
     * built-in real-SQLite module). Surfaced via `codegraph status` and the
     * `codegraph_status` MCP tool alongside the effective journal mode.
     */
    getBackend(): import('./db').SqliteBackend;
    /**
     * The journal mode actually in effect ('wal', 'delete', …). 'wal' means
     * readers never block on a concurrent writer; anything else means they can,
     * which is the precondition for the "database is locked" failures in issue
     * #238. Surfaced via `codegraph status` and the `codegraph_status` MCP tool.
     */
    getJournalMode(): string;
    /**
     * Get a node by ID
     */
    getNode(id: string): Node | null;
    /**
     * Get all nodes in a file
     */
    getNodesInFile(filePath: string): Node[];
    /**
     * Get all nodes of a specific kind
     */
    getNodesByKind(kind: Node['kind']): Node[];
    /**
     * Search nodes by text
     */
    searchNodes(query: string, options?: SearchOptions): SearchResult[];
    /**
     * Get outgoing edges from a node
     */
    getOutgoingEdges(nodeId: string): Edge[];
    /**
     * Get incoming edges to a node
     */
    getIncomingEdges(nodeId: string): Edge[];
    /**
     * Get a file record by path
     */
    getFile(filePath: string): FileRecord | null;
    /**
     * Get all tracked files
     */
    getFiles(): FileRecord[];
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
    getContext(nodeId: string): Context;
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
    traverse(startId: string, options?: TraversalOptions): Subgraph;
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
    getCallGraph(nodeId: string, depth?: number): Subgraph;
    /**
     * Get the type hierarchy for a class/interface
     *
     * Returns both ancestors (types this extends/implements) and
     * descendants (types that extend/implement this).
     *
     * @param nodeId - ID of the class/interface node
     * @returns Subgraph containing the type hierarchy
     */
    getTypeHierarchy(nodeId: string): Subgraph;
    /**
     * Find all usages of a symbol
     *
     * Returns all nodes that reference the specified symbol through
     * any edge type (calls, references, type_of, etc.).
     *
     * @param nodeId - ID of the symbol node
     * @returns Array of nodes and edges that reference this symbol
     */
    findUsages(nodeId: string): Array<{
        node: Node;
        edge: Edge;
    }>;
    /**
     * Get callers of a function/method
     *
     * @param nodeId - ID of the function/method node
     * @param maxDepth - Maximum depth to traverse (default: 1)
     * @returns Array of nodes that call this function
     */
    getCallers(nodeId: string, maxDepth?: number): Array<{
        node: Node;
        edge: Edge;
    }>;
    /**
     * Get callees of a function/method
     *
     * @param nodeId - ID of the function/method node
     * @param maxDepth - Maximum depth to traverse (default: 1)
     * @returns Array of nodes called by this function
     */
    getCallees(nodeId: string, maxDepth?: number): Array<{
        node: Node;
        edge: Edge;
    }>;
    /**
     * Calculate the impact radius of a node
     *
     * Returns all nodes that could be affected by changes to this node.
     *
     * @param nodeId - ID of the node
     * @param maxDepth - Maximum depth to traverse (default: 3)
     * @returns Subgraph containing potentially impacted nodes
     */
    getImpactRadius(nodeId: string, maxDepth?: number): Subgraph;
    /**
     * Find the shortest path between two nodes
     *
     * @param fromId - Starting node ID
     * @param toId - Target node ID
     * @param edgeKinds - Edge types to consider (all if empty)
     * @returns Array of nodes and edges forming the path, or null if no path exists
     */
    findPath(fromId: string, toId: string, edgeKinds?: Edge['kind'][]): Array<{
        node: Node;
        edge: Edge | null;
    }> | null;
    /**
     * Get ancestors of a node in the containment hierarchy
     *
     * @param nodeId - ID of the node
     * @returns Array of ancestor nodes from immediate parent to root
     */
    getAncestors(nodeId: string): Node[];
    /**
     * Get immediate children of a node
     *
     * @param nodeId - ID of the node
     * @returns Array of child nodes
     */
    getChildren(nodeId: string): Node[];
    /**
     * Get dependencies of a file
     *
     * @param filePath - Path to the file
     * @returns Array of file paths this file depends on
     */
    getFileDependencies(filePath: string): string[];
    /**
     * Get dependents of a file
     *
     * @param filePath - Path to the file
     * @returns Array of file paths that depend on this file
     */
    getFileDependents(filePath: string): string[];
    /**
     * Find circular dependencies in the codebase
     *
     * @returns Array of cycles, each cycle is an array of file paths
     */
    findCircularDependencies(): string[][];
    /**
     * Find dead code (unreferenced symbols)
     *
     * @param kinds - Node kinds to check (default: functions, methods, classes)
     * @returns Array of unreferenced nodes
     */
    findDeadCode(kinds?: Node['kind'][]): Node[];
    /**
     * Get complexity metrics for a node
     *
     * @param nodeId - ID of the node
     * @returns Object containing various complexity metrics
     */
    getNodeMetrics(nodeId: string): {
        incomingEdgeCount: number;
        outgoingEdgeCount: number;
        callCount: number;
        callerCount: number;
        childCount: number;
        depth: number;
    };
    /**
     * Get the source code for a node
     *
     * Reads the file and extracts the code between startLine and endLine.
     *
     * @param nodeId - ID of the node
     * @returns Code string or null if not found
     */
    getCode(nodeId: string): Promise<string | null>;
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
    findRelevantContext(query: string, options?: FindRelevantContextOptions): Promise<Subgraph>;
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
    buildContext(input: TaskInput, options?: BuildContextOptions): Promise<TaskContext | string>;
    /**
     * Optimize the database (vacuum and analyze)
     */
    optimize(): void;
    /**
     * Clear all data from the graph
     */
    clear(): void;
    /**
     * Alias for close() for backwards compatibility.
     * @deprecated Use close() instead
     */
    destroy(): void;
    /**
     * Completely remove CodeGraph from the project.
     * This closes the database and deletes the .CodeGraph directory.
     *
     * WARNING: This permanently deletes all CodeGraph data for the project.
     */
    uninitialize(): void;
}
export default CodeGraph;
//# sourceMappingURL=index.d.ts.map