"use strict";
/**
 * File Watcher
 *
 * Watches the project directory for file changes and triggers
 * debounced sync operations to keep the code graph up-to-date.
 *
 * Uses Node.js native fs.watch with recursive mode (macOS FSEvents,
 * Windows ReadDirectoryChangesW, Linux inotify on Node 19+).
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
Object.defineProperty(exports, "__esModule", { value: true });
exports.FileWatcher = void 0;
const fs = __importStar(require("fs"));
const extraction_1 = require("../extraction");
const errors_1 = require("../errors");
const utils_1 = require("../utils");
const watch_policy_1 = require("./watch-policy");
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
class FileWatcher {
    watcher = null;
    debounceTimer = null;
    hasChanges = false;
    syncing = false;
    stopped = false;
    projectRoot;
    debounceMs;
    syncFn;
    onSyncComplete;
    onSyncError;
    constructor(projectRoot, syncFn, options = {}) {
        this.projectRoot = projectRoot;
        this.syncFn = syncFn;
        this.debounceMs = options.debounceMs ?? 2000;
        this.onSyncComplete = options.onSyncComplete;
        this.onSyncError = options.onSyncError;
    }
    /**
     * Start watching for file changes.
     * Returns true if watching started successfully, false otherwise.
     */
    start() {
        if (this.watcher)
            return true; // Already watching
        this.stopped = false;
        // Some environments make recursive fs.watch unusable — most notably WSL2
        // /mnt/ drives, where setup blocks long enough to break MCP startup
        // handshakes (issue #199). Skip watching there; callers fall back to
        // manual `codegraph sync` or the git sync hooks.
        const disabledReason = (0, watch_policy_1.watchDisabledReason)(this.projectRoot);
        if (disabledReason) {
            (0, errors_1.logDebug)('File watcher disabled', { reason: disabledReason, projectRoot: this.projectRoot });
            return false;
        }
        try {
            this.watcher = fs.watch(this.projectRoot, { recursive: true }, (_eventType, filename) => {
                if (!filename || this.stopped)
                    return;
                // Normalize path separators
                const normalized = (0, utils_1.normalizePath)(filename);
                // Ignore .codegraph/ directory changes (our own DB writes)
                if (normalized === '.codegraph' ||
                    normalized.startsWith('.codegraph/') ||
                    normalized.startsWith('.codegraph\\')) {
                    return;
                }
                // Only sync changes to files we can actually parse.
                if (!(0, extraction_1.isSourceFile)(normalized)) {
                    return;
                }
                (0, errors_1.logDebug)('File change detected', { file: normalized });
                this.hasChanges = true;
                this.scheduleSync();
            });
            // Handle watcher errors gracefully
            this.watcher.on('error', (err) => {
                (0, errors_1.logWarn)('File watcher error', { error: String(err) });
                // Don't crash — watcher may recover or user can restart
            });
            (0, errors_1.logDebug)('File watcher started', { projectRoot: this.projectRoot, debounceMs: this.debounceMs });
            return true;
        }
        catch (err) {
            // Recursive watch not supported (e.g., Linux < Node 19)
            (0, errors_1.logWarn)('Could not start file watcher — recursive fs.watch not supported on this platform', { error: String(err) });
            return false;
        }
    }
    /**
     * Stop watching for file changes.
     */
    stop() {
        this.stopped = true;
        if (this.debounceTimer) {
            clearTimeout(this.debounceTimer);
            this.debounceTimer = null;
        }
        if (this.watcher) {
            this.watcher.close();
            this.watcher = null;
        }
        this.hasChanges = false;
        (0, errors_1.logDebug)('File watcher stopped');
    }
    /**
     * Whether the watcher is currently active.
     */
    isActive() {
        return this.watcher !== null && !this.stopped;
    }
    /**
     * Schedule a debounced sync.
     */
    scheduleSync() {
        if (this.debounceTimer) {
            clearTimeout(this.debounceTimer);
        }
        this.debounceTimer = setTimeout(() => {
            this.debounceTimer = null;
            this.flush();
        }, this.debounceMs);
    }
    /**
     * Flush pending changes by running sync.
     */
    async flush() {
        // If already syncing, the post-sync check will re-trigger
        if (this.syncing || this.stopped)
            return;
        this.hasChanges = false;
        this.syncing = true;
        try {
            const result = await this.syncFn();
            this.onSyncComplete?.(result);
        }
        catch (err) {
            const error = err instanceof Error ? err : new Error(String(err));
            (0, errors_1.logWarn)('Watch sync failed', { error: error.message });
            this.onSyncError?.(error);
        }
        finally {
            this.syncing = false;
            // If new changes arrived during sync, schedule another
            if (this.hasChanges && !this.stopped) {
                this.scheduleSync();
            }
        }
    }
}
exports.FileWatcher = FileWatcher;
//# sourceMappingURL=watcher.js.map