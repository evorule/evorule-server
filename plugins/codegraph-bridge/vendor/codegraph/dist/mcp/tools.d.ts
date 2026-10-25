/**
 * MCP Tool Definitions
 *
 * Defines the tools exposed by the CodeGraph MCP server.
 */
import CodeGraph from '../index';
/**
 * Calculate the recommended number of codegraph_explore calls based on project size.
 * Larger codebases need more exploration calls to cover their surface area,
 * but smaller ones should use fewer to avoid unnecessary overhead.
 */
export declare function getExploreBudget(fileCount: number): number;
/**
 * Adaptive output budget for `codegraph_explore`, scaled to project size.
 *
 * Smaller codebases get a tighter total cap, fewer default files, smaller
 * per-file cap, and tighter clustering — so a focused query on a 100-file
 * project doesn't dump a whole file's worth of source into the agent's
 * context. Larger codebases keep the generous defaults because the
 * agent's native discovery cost (grep + find + many Reads) genuinely
 * dwarfs a fat explore call at that scale.
 *
 * Meta-text (relationships map, "additional relevant files" list,
 * completeness signal, budget note) is gated off for tiny projects
 * where one rich call is the whole story and the extra prose is just
 * overhead.
 *
 * Tier breakpoints mirror `getExploreBudget` so a project sits in the
 * same tier across both knobs.
 */
export interface ExploreOutputBudget {
    /** Hard cap on total output characters. */
    maxOutputChars: number;
    /** Default `maxFiles` when the caller didn't specify one. */
    defaultMaxFiles: number;
    /** Cap on contiguous source returned per file (across all its clusters). */
    maxCharsPerFile: number;
    /** Cluster gap threshold in lines — tighter clustering on small projects. */
    gapThreshold: number;
    /** Max symbols listed in the per-file header (`#### path — sym(kind), ...`). */
    maxSymbolsInFileHeader: number;
    /** Max edges shown per relationship kind in the Relationships section. */
    maxEdgesPerRelationshipKind: number;
    /** Include the "Relationships" section. */
    includeRelationships: boolean;
    /** Include the "Additional relevant files (not shown)" trailing list. */
    includeAdditionalFiles: boolean;
    /** Include the "Complete source code is included above…" reminder. */
    includeCompletenessSignal: boolean;
    /** Include the explore-budget reminder at the end. */
    includeBudgetNote: boolean;
}
export declare function getExploreOutputBudget(fileCount: number): ExploreOutputBudget;
/**
 * MCP Tool definition
 */
export interface ToolDefinition {
    name: string;
    description: string;
    inputSchema: {
        type: 'object';
        properties: Record<string, PropertySchema>;
        required?: string[];
    };
}
interface PropertySchema {
    type: string;
    description: string;
    enum?: string[];
    default?: unknown;
}
/**
 * Tool execution result
 */
export interface ToolResult {
    content: Array<{
        type: 'text';
        text: string;
    }>;
    isError?: boolean;
}
/**
 * All CodeGraph MCP tools
 *
 * Designed for minimal context usage - use codegraph_context as the primary tool,
 * and only use other tools for targeted follow-up queries.
 *
 * All tools support cross-project queries via the optional `projectPath` parameter.
 */
export declare const tools: ToolDefinition[];
/**
 * Tool handler that executes tools against a CodeGraph instance
 *
 * Supports cross-project queries via the projectPath parameter.
 * Other projects are opened on-demand and cached for performance.
 */
export declare class ToolHandler {
    private cg;
    private projectCache;
    private defaultProjectHint;
    constructor(cg: CodeGraph | null);
    /**
     * Update the default CodeGraph instance (e.g. after lazy initialization)
     */
    setDefaultCodeGraph(cg: CodeGraph): void;
    /**
     * Record the directory the server tried to resolve the default project from.
     * Used only to make the "no default project" error actionable.
     */
    setDefaultProjectHint(searchedPath: string): void;
    /**
     * Whether a default CodeGraph instance is available
     */
    hasDefaultCodeGraph(): boolean;
    /**
     * Get tool definitions with dynamic descriptions based on project size.
     * The codegraph_explore tool description includes a budget recommendation
     * scaled to the number of indexed files.
     */
    getTools(): ToolDefinition[];
    /**
     * Get CodeGraph instance for a project
     *
     * If projectPath is provided, opens that project's CodeGraph (cached).
     * Otherwise returns the default CodeGraph instance.
     *
     * Walks up parent directories to find the nearest .codegraph/ folder,
     * similar to how git finds .git/ directories.
     */
    private getCodeGraph;
    /**
     * Close all cached project connections
     */
    closeAll(): void;
    /**
     * Validate that a value is a non-empty string within length bounds.
     *
     * The `maxLength` cap protects against MCP clients that ship huge
     * payloads (10MB+ query strings either by accident or maliciously).
     * Without this, a single oversized input can pin the FTS5 index or
     * exhaust memory before any real work runs.
     */
    private validateString;
    /**
     * Validate an optional path-like string input. Returns the value if
     * valid (or undefined), or a ToolResult with the error.
     */
    private validateOptionalPath;
    /**
     * Execute a tool by name
     */
    execute(toolName: string, args: Record<string, unknown>): Promise<ToolResult>;
    /**
     * Handle codegraph_search
     */
    private handleSearch;
    /**
     * Handle codegraph_context
     */
    private handleContext;
    /**
     * Heuristic to detect if a query looks like a feature request
     */
    private looksLikeFeatureRequest;
    /**
     * Handle codegraph_callers
     */
    private handleCallers;
    /**
     * Handle codegraph_callees
     */
    private handleCallees;
    /**
     * Handle codegraph_impact
     */
    private handleImpact;
    /**
     * Handle codegraph_explore — deep exploration in a single call
     *
     * Strategy: find relevant symbols via graph traversal, group by file,
     * then read contiguous file sections covering all symbols per file.
     * This replaces multiple codegraph_node + Read calls.
     *
     * Output size is adaptive to project file count via
     * `getExploreOutputBudget` — see #185 for why a fixed 35k cap was a
     * tax on small projects while earning its keep on large ones.
     */
    private handleExplore;
    /**
     * Handle codegraph_node
     */
    private handleNode;
    /**
     * Handle codegraph_status
     */
    private handleStatus;
    /**
     * Handle codegraph_files - get project file structure from the index
     */
    private handleFiles;
    /**
     * Convert glob pattern to regex
     */
    private globToRegex;
    /**
     * Format files as a flat list
     */
    private formatFilesFlat;
    /**
     * Format files grouped by language
     */
    private formatFilesGrouped;
    /**
     * Format files as a tree structure
     */
    private formatFilesTree;
    /**
     * Find a symbol by name, handling disambiguation when multiple matches exist.
     * Returns the best match and a note about alternatives if any.
     */
    /**
     * Check if a node matches a symbol query.
     *
     * Accepts simple names (`run`) and three flavors of qualifier:
     *   - dotted     `Session.request`         (TS/JS/Python)
     *   - colon-pair `stage_apply::run`        (Rust, C++, Ruby)
     *   - slash      `configurator/stage_apply` (path-ish)
     *
     * Multi-level qualifiers compose: `crate::configurator::stage_apply::run`
     * works. Rust path prefixes (`crate`, `super`, `self`) are stripped so
     * the canonical `crate::module::symbol` form resolves.
     *
     * Resolution order, last part must always equal `node.name`:
     *   1. Suffix-match against `qualifiedName` (handles class-scoped methods
     *      where the extractor builds the qualified name from the AST stack)
     *   2. File-path containment (handles file-derived modules in Rust/
     *      Python — `stage_apply::run` matches a `run` in `stage_apply.rs`)
     */
    private matchesSymbol;
    private findSymbol;
    /**
     * Find ALL symbols matching a name. Used by callers/callees/impact to aggregate
     * results across all matching symbols (e.g., multiple classes with an `execute` method).
     */
    private findAllSymbols;
    /**
     * Truncate output if it exceeds the maximum length
     */
    private truncateOutput;
    private formatSearchResults;
    private formatNodeList;
    private formatImpact;
    /**
     * Build a compact structural outline of a container symbol from its
     * indexed children (methods, fields, properties, …) — name, kind,
     * line number, and signature — so the agent gets the shape of a class
     * without the full source of every method. Returns '' when the container
     * has no indexed children, so the caller can fall back to full source.
     */
    private buildContainerOutline;
    private formatNodeDetails;
    private formatTaskContext;
    private textResult;
    private errorResult;
}
export {};
//# sourceMappingURL=tools.d.ts.map