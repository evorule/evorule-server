/**
 * Reference Resolution Orchestrator
 *
 * Coordinates all reference resolution strategies.
 */
import { UnresolvedReference, Edge } from '../types';
import { QueryBuilder } from '../db/queries';
import { UnresolvedRef, ResolvedRef, ResolutionResult } from './types';
export * from './types';
/**
 * Reference Resolver
 *
 * Orchestrates reference resolution using multiple strategies.
 */
export declare class ReferenceResolver {
    private projectRoot;
    private queries;
    private context;
    private frameworks;
    private nodeCache;
    private fileCache;
    private importMappingCache;
    private reExportCache;
    private nameCache;
    private lowerNameCache;
    private qualifiedNameCache;
    private knownNames;
    private knownFiles;
    private cachesWarmed;
    private projectAliases;
    constructor(projectRoot: string, queries: QueryBuilder);
    /**
     * Initialize the resolver (detect frameworks, etc.)
     */
    initialize(): void;
    /**
     * Pre-build lightweight caches for resolution.
     * Node lookups are now handled by indexed SQLite queries instead of
     * loading all nodes into memory (which caused OOM on large codebases).
     * We cache the set of known symbol names for fast pre-filtering.
     */
    warmCaches(): void;
    /**
     * Clear internal caches
     */
    clearCaches(): void;
    /**
     * Create the resolution context
     */
    private createContext;
    /**
     * Resolve all unresolved references
     */
    resolveAll(unresolvedRefs: UnresolvedReference[], onProgress?: (current: number, total: number) => void): ResolutionResult;
    /**
     * Check if a reference name has any possible match in the codebase.
     * Uses the pre-built knownNames set to skip expensive resolution
     * for names that definitely don't exist as symbols.
     */
    private hasAnyPossibleMatch;
    /**
     * Does `ref.referenceName` match an import declared in its containing
     * file? Used as a pre-filter escape so re-export chain resolution
     * still gets a chance when the name has no project-wide declaration.
     */
    private matchesAnyImport;
    /**
     * Resolve a single reference
     */
    resolveOne(ref: UnresolvedRef): ResolvedRef | null;
    /**
     * Create edges from resolved references
     */
    createEdges(resolved: ResolvedRef[]): Edge[];
    /**
     * Resolve and persist edges to database
     */
    resolveAndPersist(unresolvedRefs: UnresolvedReference[], onProgress?: (current: number, total: number) => void): ResolutionResult;
    /**
     * Resolve and persist in batches to keep memory bounded.
     * Processes unresolved references in chunks, persisting edges and cleaning
     * up resolved refs after each batch to avoid accumulating large arrays.
     */
    resolveAndPersistBatched(onProgress?: (current: number, total: number) => void, batchSize?: number): Promise<ResolutionResult>;
    /**
     * Get detected frameworks
     */
    getDetectedFrameworks(): string[];
    /**
     * Check if reference is to a built-in or external symbol
     */
    private isBuiltInOrExternal;
    /**
     * Get file path from node ID
     */
    private getFilePathFromNodeId;
    /**
     * Get language from node ID
     */
    private getLanguageFromNodeId;
}
/**
 * Create a reference resolver instance
 */
export declare function createResolver(projectRoot: string, queries: QueryBuilder): ReferenceResolver;
//# sourceMappingURL=index.d.ts.map