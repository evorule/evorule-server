"use strict";
/**
 * CodeGraph Utilities
 *
 * Common utility functions for memory management, concurrency, batching,
 * and security validation.
 *
 * @module utils
 *
 * @example
 * ```typescript
 * import { Mutex, processInBatches, MemoryMonitor, validatePathWithinRoot } from 'codegraph';
 *
 * // Use mutex for concurrent safety
 * const mutex = new Mutex();
 * await mutex.withLock(async () => {
 *   await performCriticalOperation();
 * });
 *
 * // Process items in batches to manage memory
 * const results = await processInBatches(items, 100, async (item) => {
 *   return await processItem(item);
 * });
 *
 * // Monitor memory usage
 * const monitor = new MemoryMonitor(512, (usage) => {
 *   console.warn(`Memory usage exceeded 512MB: ${usage / 1024 / 1024}MB`);
 * });
 * monitor.start();
 * ```
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
exports.MemoryMonitor = exports.Mutex = exports.FileLock = void 0;
exports.validatePathWithinRoot = validatePathWithinRoot;
exports.validateProjectPath = validateProjectPath;
exports.isPathWithinRoot = isPathWithinRoot;
exports.isPathWithinRootReal = isPathWithinRootReal;
exports.safeJsonParse = safeJsonParse;
exports.clamp = clamp;
exports.normalizePath = normalizePath;
exports.processInBatches = processInBatches;
exports.readFileInChunks = readFileInChunks;
exports.debounce = debounce;
exports.throttle = throttle;
exports.estimateSize = estimateSize;
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
// ============================================================
// SECURITY UTILITIES
// ============================================================
/**
 * Sensitive system directories that should never be used as project roots.
 * Checked on all platforms; non-applicable paths are harmlessly skipped.
 */
const SENSITIVE_PATHS = new Set([
    '/', '/etc', '/usr', '/bin', '/sbin', '/var', '/tmp', '/dev', '/proc', '/sys',
    '/root', '/boot', '/lib', '/lib64', '/opt',
    'c:\\', 'c:\\windows', 'c:\\windows\\system32',
]);
/**
 * Validate that a resolved file path stays within the project root.
 * Prevents path traversal attacks (e.g. node.filePath = "../../etc/passwd").
 *
 * @param projectRoot - The project root directory
 * @param filePath - The relative file path to validate
 * @returns The resolved absolute path, or null if it escapes the root
 */
function validatePathWithinRoot(projectRoot, filePath) {
    const resolved = path.resolve(projectRoot, filePath);
    const normalizedRoot = path.resolve(projectRoot);
    if (!resolved.startsWith(normalizedRoot + path.sep) && resolved !== normalizedRoot) {
        return null;
    }
    return resolved;
}
/**
 * Validate that a path is a safe project root directory.
 *
 * Rejects sensitive system directories and ensures the path is
 * a real, existing directory. Used at MCP and API entry points
 * to prevent arbitrary directory access.
 *
 * @param dirPath - The path to validate
 * @returns An error message if invalid, or null if valid
 */
function validateProjectPath(dirPath) {
    const resolved = path.resolve(dirPath);
    // Block sensitive system directories
    if (SENSITIVE_PATHS.has(resolved) || SENSITIVE_PATHS.has(resolved.toLowerCase())) {
        return `Refusing to operate on sensitive system directory: ${resolved}`;
    }
    // Also block common sensitive home subdirectories
    const homeDir = require('os').homedir();
    const sensitiveHomeDirs = ['.ssh', '.gnupg', '.aws', '.config'];
    for (const dir of sensitiveHomeDirs) {
        const sensitivePath = path.join(homeDir, dir);
        if (resolved === sensitivePath || resolved.startsWith(sensitivePath + path.sep)) {
            return `Refusing to operate on sensitive directory: ${resolved}`;
        }
    }
    // Verify it's a real directory
    try {
        const stats = fs.statSync(resolved);
        if (!stats.isDirectory()) {
            return `Path is not a directory: ${resolved}`;
        }
    }
    catch {
        return `Path does not exist or is not accessible: ${resolved}`;
    }
    return null;
}
/**
 * Check if a file path resolves to a location within the given root directory.
 *
 * Prevents path traversal attacks by ensuring the resolved absolute path
 * starts with the resolved root path. Handles '..' sequences, symlink-like
 * relative paths, and platform-specific separators.
 *
 * @param filePath - The path to check (can be relative or absolute)
 * @param rootDir - The root directory that filePath must stay within
 * @returns true if filePath resolves to a location within rootDir
 */
function isPathWithinRoot(filePath, rootDir) {
    const resolvedPath = path.resolve(rootDir, filePath);
    const resolvedRoot = path.resolve(rootDir);
    return resolvedPath.startsWith(resolvedRoot + path.sep) || resolvedPath === resolvedRoot;
}
/**
 * Like isPathWithinRoot but also resolves symlinks via fs.realpathSync.
 *
 * This catches symlink escapes where the logical path appears to be within
 * root but the real path on disk points elsewhere. Falls back to logical
 * path checking if realpath resolution fails (e.g. broken symlink).
 */
function isPathWithinRootReal(filePath, rootDir) {
    // First do the cheap logical check
    if (!isPathWithinRoot(filePath, rootDir)) {
        return false;
    }
    // Then verify with realpath to catch symlink escapes
    try {
        const realPath = fs.realpathSync(path.resolve(rootDir, filePath));
        const realRoot = fs.realpathSync(rootDir);
        return realPath.startsWith(realRoot + path.sep) || realPath === realRoot;
    }
    catch {
        // If realpath fails (broken symlink, permissions), fall back to logical check
        return true;
    }
}
/**
 * Safely parse JSON with a fallback value.
 * Prevents crashes from corrupted database metadata.
 */
function safeJsonParse(value, fallback) {
    try {
        return JSON.parse(value);
    }
    catch {
        return fallback;
    }
}
/**
 * Clamp a numeric value to a range.
 * Used to enforce sane limits on MCP tool inputs.
 */
function clamp(value, min, max) {
    return Math.max(min, Math.min(max, value));
}
/**
 * Normalize a file path to use forward slashes.
 * Fixes Windows backslash paths so glob matching works consistently.
 */
function normalizePath(filePath) {
    return filePath.replace(/\\/g, '/');
}
/**
 * Cross-process file lock using a lock file with PID tracking.
 *
 * Prevents multiple processes (e.g., git hooks, CLI, MCP server) from
 * writing to the same database simultaneously.
 */
class FileLock {
    lockPath;
    held = false;
    /** Locks older than this are considered stale regardless of PID status */
    static STALE_TIMEOUT_MS = 2 * 60 * 1000; // 2 minutes
    constructor(lockPath) {
        this.lockPath = lockPath;
    }
    /**
     * Acquire the lock. Throws if the lock is held by another live process.
     */
    acquire() {
        // Check for existing lock
        if (fs.existsSync(this.lockPath)) {
            try {
                const content = fs.readFileSync(this.lockPath, 'utf-8').trim();
                const pid = parseInt(content, 10);
                const stat = fs.statSync(this.lockPath);
                const lockAge = Date.now() - stat.mtimeMs;
                // Treat locks older than the timeout as stale, regardless of PID
                if (lockAge < FileLock.STALE_TIMEOUT_MS && !isNaN(pid) && this.isProcessAlive(pid)) {
                    throw new Error(`CodeGraph database is locked by another process (PID ${pid}). ` +
                        `If this is stale, run 'codegraph unlock' or delete ${this.lockPath}`);
                }
                // Stale lock (dead process or timed out) - remove it
                fs.unlinkSync(this.lockPath);
            }
            catch (err) {
                if (err instanceof Error && err.message.includes('locked by another')) {
                    throw err;
                }
                // Other errors reading lock file - try to remove it
                try {
                    fs.unlinkSync(this.lockPath);
                }
                catch { /* ignore */ }
            }
        }
        // Write our PID to the lock file using exclusive create flag
        try {
            fs.writeFileSync(this.lockPath, String(process.pid), { flag: 'wx' });
            this.held = true;
        }
        catch (err) {
            if (err.code === 'EEXIST') {
                // Race condition: another process grabbed the lock between our check and write
                throw new Error('CodeGraph database is locked by another process. ' +
                    `If this is stale, run 'codegraph unlock' or delete ${this.lockPath}`);
            }
            throw err;
        }
    }
    /**
     * Release the lock
     */
    release() {
        if (!this.held)
            return;
        try {
            // Only remove if we still own it (check PID)
            const content = fs.readFileSync(this.lockPath, 'utf-8').trim();
            if (parseInt(content, 10) === process.pid) {
                fs.unlinkSync(this.lockPath);
            }
        }
        catch {
            // Lock file already gone - that's fine
        }
        this.held = false;
    }
    /**
     * Execute a function while holding the lock
     */
    withLock(fn) {
        this.acquire();
        try {
            return fn();
        }
        finally {
            this.release();
        }
    }
    /**
     * Execute an async function while holding the lock
     */
    async withLockAsync(fn) {
        this.acquire();
        try {
            return await fn();
        }
        finally {
            this.release();
        }
    }
    /**
     * Check if a process is still running
     */
    isProcessAlive(pid) {
        try {
            process.kill(pid, 0);
            return true;
        }
        catch {
            return false;
        }
    }
}
exports.FileLock = FileLock;
/**
 * Process items in batches to manage memory
 *
 * @param items - Array of items to process
 * @param batchSize - Number of items per batch
 * @param processor - Function to process each item
 * @param onBatchComplete - Optional callback after each batch
 * @returns Array of results
 */
async function processInBatches(items, batchSize, processor, onBatchComplete) {
    const results = [];
    for (let i = 0; i < items.length; i += batchSize) {
        const batch = items.slice(i, Math.min(i + batchSize, items.length));
        const batchResults = await Promise.all(batch.map((item, idx) => processor(item, i + idx)));
        results.push(...batchResults);
        if (onBatchComplete) {
            onBatchComplete(Math.min(i + batchSize, items.length), items.length);
        }
        // Allow GC between batches
        if (global.gc) {
            global.gc();
        }
    }
    return results;
}
/**
 * Simple mutex lock for preventing concurrent operations
 */
class Mutex {
    locked = false;
    waitQueue = [];
    /**
     * Acquire the lock
     *
     * @returns A release function to call when done
     */
    async acquire() {
        while (this.locked) {
            await new Promise((resolve) => {
                this.waitQueue.push(resolve);
            });
        }
        this.locked = true;
        return () => {
            this.locked = false;
            const next = this.waitQueue.shift();
            if (next) {
                next();
            }
        };
    }
    /**
     * Execute a function while holding the lock
     */
    async withLock(fn) {
        const release = await this.acquire();
        try {
            return await fn();
        }
        finally {
            release();
        }
    }
    /**
     * Check if the lock is currently held
     */
    isLocked() {
        return this.locked;
    }
}
exports.Mutex = Mutex;
/**
 * Chunked file reader for large files
 *
 * Reads a file in chunks to avoid loading entire file into memory.
 */
async function* readFileInChunks(filePath, chunkSize = 64 * 1024) {
    const fs = await Promise.resolve().then(() => __importStar(require('fs')));
    const fd = fs.openSync(filePath, 'r');
    const buffer = Buffer.alloc(chunkSize);
    try {
        let bytesRead;
        while ((bytesRead = fs.readSync(fd, buffer, 0, chunkSize, null)) > 0) {
            yield buffer.toString('utf-8', 0, bytesRead);
        }
    }
    finally {
        fs.closeSync(fd);
    }
}
/**
 * Debounce a function
 *
 * @param fn - Function to debounce
 * @param delay - Delay in milliseconds
 * @returns Debounced function
 */
function debounce(fn, delay) {
    let timeoutId = null;
    return (...args) => {
        if (timeoutId) {
            clearTimeout(timeoutId);
        }
        timeoutId = setTimeout(() => {
            fn(...args);
            timeoutId = null;
        }, delay);
    };
}
/**
 * Throttle a function
 *
 * @param fn - Function to throttle
 * @param limit - Minimum time between calls in milliseconds
 * @returns Throttled function
 */
function throttle(fn, limit) {
    let lastCall = 0;
    let timeoutId = null;
    return (...args) => {
        const now = Date.now();
        const remaining = limit - (now - lastCall);
        if (remaining <= 0) {
            if (timeoutId) {
                clearTimeout(timeoutId);
                timeoutId = null;
            }
            lastCall = now;
            fn(...args);
        }
        else if (!timeoutId) {
            timeoutId = setTimeout(() => {
                lastCall = Date.now();
                timeoutId = null;
                fn(...args);
            }, remaining);
        }
    };
}
/**
 * Estimate memory usage of an object (rough approximation)
 *
 * @param obj - Object to measure
 * @returns Approximate size in bytes
 */
function estimateSize(obj) {
    const seen = new WeakSet();
    function sizeOf(value) {
        if (value === null || value === undefined) {
            return 0;
        }
        switch (typeof value) {
            case 'boolean':
                return 4;
            case 'number':
                return 8;
            case 'string':
                return 2 * value.length;
            case 'object':
                if (seen.has(value)) {
                    return 0;
                }
                seen.add(value);
                if (Array.isArray(value)) {
                    return value.reduce((acc, item) => acc + sizeOf(item), 0);
                }
                return Object.entries(value).reduce((acc, [key, val]) => acc + sizeOf(key) + sizeOf(val), 0);
            default:
                return 0;
        }
    }
    return sizeOf(obj);
}
/**
 * Memory monitor for tracking usage during operations
 */
class MemoryMonitor {
    checkInterval = null;
    peakUsage = 0;
    threshold;
    onThresholdExceeded;
    constructor(thresholdMB = 500, onThresholdExceeded) {
        this.threshold = thresholdMB * 1024 * 1024;
        this.onThresholdExceeded = onThresholdExceeded;
    }
    /**
     * Start monitoring memory usage
     */
    start(intervalMs = 1000) {
        this.stop();
        this.peakUsage = 0;
        this.checkInterval = setInterval(() => {
            const usage = process.memoryUsage().heapUsed;
            if (usage > this.peakUsage) {
                this.peakUsage = usage;
            }
            if (usage > this.threshold && this.onThresholdExceeded) {
                this.onThresholdExceeded(usage);
            }
        }, intervalMs);
    }
    /**
     * Stop monitoring
     */
    stop() {
        if (this.checkInterval) {
            clearInterval(this.checkInterval);
            this.checkInterval = null;
        }
    }
    /**
     * Get peak memory usage in bytes
     */
    getPeakUsage() {
        return this.peakUsage;
    }
    /**
     * Get current memory usage in bytes
     */
    getCurrentUsage() {
        return process.memoryUsage().heapUsed;
    }
}
exports.MemoryMonitor = MemoryMonitor;
//# sourceMappingURL=utils.js.map