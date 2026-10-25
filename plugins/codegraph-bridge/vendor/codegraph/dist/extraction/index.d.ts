/**
 * Extraction Orchestrator
 *
 * Coordinates file scanning, parsing, and database storage.
 */
import * as fs from 'fs';
import { ExtractionResult, ExtractionError } from '../types';
import { QueryBuilder } from '../db/queries';
/**
 * Progress callback for indexing operations
 */
export interface IndexProgress {
    phase: 'scanning' | 'parsing' | 'storing' | 'resolving';
    current: number;
    total: number;
    currentFile?: string;
}
/**
 * Result of an indexing operation
 */
export interface IndexResult {
    success: boolean;
    filesIndexed: number;
    filesSkipped: number;
    filesErrored: number;
    nodesCreated: number;
    edgesCreated: number;
    errors: ExtractionError[];
    durationMs: number;
}
/**
 * Result of a sync operation
 */
export interface SyncResult {
    filesChecked: number;
    filesAdded: number;
    filesModified: number;
    filesRemoved: number;
    nodesUpdated: number;
    durationMs: number;
    changedFilePaths?: string[];
}
/**
 * Calculate SHA256 hash of file contents
 */
export declare function hashContent(content: string): string;
/**
 * Recursively scan a directory for source files.
 *
 * In git repos, uses `git ls-files` (inherently respects .gitignore at all
 * levels), then keeps files with a supported source extension. For non-git
 * projects, falls back to a filesystem walk that parses .gitignore itself.
 */
export declare function scanDirectory(rootDir: string, onProgress?: (current: number, file: string) => void): string[];
/**
 * Async variant of scanDirectory that yields to the event loop periodically,
 * allowing worker threads to receive and render progress messages.
 */
export declare function scanDirectoryAsync(rootDir: string, onProgress?: (current: number, file: string) => void): Promise<string[]>;
/**
 * Extraction orchestrator
 */
export declare class ExtractionOrchestrator {
    private rootDir;
    private queries;
    /**
     * Names of frameworks detected for this project, populated by indexAll().
     * Passed to extractFromSource so framework-specific extractors (route nodes,
     * middleware, etc.) run after the tree-sitter pass. Cleared if detection
     * hasn't run yet so single-file re-index paths can detect on the spot.
     */
    private detectedFrameworkNames;
    constructor(rootDir: string, queries: QueryBuilder);
    /**
     * Build a filesystem-backed ResolutionContext sufficient for framework
     * detection. Graph-query methods (getNodesByName etc.) return empty because
     * the DB hasn't been populated yet, but detect() only uses readFile,
     * fileExists, and getAllFiles, so that's fine.
     */
    private buildDetectionContext;
    /**
     * Detect frameworks on demand using the current scanned files (or a fresh
     * scan if none are provided). Cached on the orchestrator so repeat calls
     * inside a single run don't re-scan.
     */
    private ensureDetectedFrameworks;
    /**
     * Index all files in the project
     */
    indexAll(onProgress?: (progress: IndexProgress) => void, signal?: AbortSignal, verbose?: boolean): Promise<IndexResult>;
    /**
     * Index specific files
     */
    indexFiles(filePaths: string[]): Promise<IndexResult>;
    /**
     * Index a single file
     */
    indexFile(relativePath: string): Promise<ExtractionResult>;
    /**
     * Index a single file with pre-read content and stats.
     * Used by the parallel batch reader to avoid redundant file I/O.
     */
    indexFileWithContent(relativePath: string, content: string, stats: fs.Stats): Promise<ExtractionResult>;
    /**
     * Store extraction result in database
     */
    private storeExtractionResult;
    /**
     * Sync with current file state.
     * Uses git status as a fast path when available, falling back to full scan.
     */
    sync(onProgress?: (progress: IndexProgress) => void): Promise<SyncResult>;
    /**
     * Get files that have changed since last index.
     * Uses git status as a fast path when available, falling back to full scan.
     */
    getChangedFiles(): {
        added: string[];
        modified: string[];
        removed: string[];
    };
}
export { extractFromSource } from './tree-sitter';
export { detectLanguage, isSourceFile, isLanguageSupported, isGrammarLoaded, getSupportedLanguages, initGrammars, loadGrammarsForLanguages, loadAllGrammars } from './grammars';
//# sourceMappingURL=index.d.ts.map