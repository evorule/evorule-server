/**
 * File Watcher
 *
 * Watches the project directory for file changes and triggers
 * debounced sync operations to keep the code graph up-to-date.
 *
 * Uses Node.js native fs.watch with recursive mode (macOS FSEvents,
 * Windows ReadDirectoryChangesW, Linux inotify on Node 19+).
 */
/**
 * Options for the file watcher
 */
export interface WatchOptions {
    /**
     * Debounce delay in milliseconds.
     * After the last file change, wait this long before triggering sync.
     * Default: 2000ms
     */
    debounceMs?: number;
    /**
     * Callback when a sync completes (for logging/diagnostics).
     */
    onSyncComplete?: (result: {
        filesChanged: number;
        durationMs: number;
    }) => void;
    /**
     * Callback when a sync errors (for logging/diagnostics).
     */
    onSyncError?: (error: Error) => void;
}
/**
 * FileWatcher monitors a project directory for changes and triggers
 * debounced sync operations via a provided callback.
 *
 * Design goals:
 * - Minimal resource usage (native OS file events, no polling)
 * - Debounced to avoid thrashing on rapid saves
 * - Filters to supported source files by extension
 * - Ignores .codegraph/ directory changes
 */
export declare class FileWatcher {
    private watcher;
    private debounceTimer;
    private hasChanges;
    private syncing;
    private stopped;
    private readonly projectRoot;
    private readonly debounceMs;
    private readonly syncFn;
    private readonly onSyncComplete?;
    private readonly onSyncError?;
    constructor(projectRoot: string, syncFn: () => Promise<{
        filesChanged: number;
        durationMs: number;
    }>, options?: WatchOptions);
    /**
     * Start watching for file changes.
     * Returns true if watching started successfully, false otherwise.
     */
    start(): boolean;
    /**
     * Stop watching for file changes.
     */
    stop(): void;
    /**
     * Whether the watcher is currently active.
     */
    isActive(): boolean;
    /**
     * Schedule a debounced sync.
     */
    private scheduleSync;
    /**
     * Flush pending changes by running sync.
     */
    private flush;
}
//# sourceMappingURL=watcher.d.ts.map