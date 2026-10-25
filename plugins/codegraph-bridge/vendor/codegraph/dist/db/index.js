"use strict";
/**
 * Database Layer
 *
 * Handles SQLite database initialization and connection management.
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
exports.DATABASE_FILENAME = exports.DatabaseConnection = void 0;
exports.getDatabasePath = getDatabasePath;
const sqlite_adapter_1 = require("./sqlite-adapter");
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const migrations_1 = require("./migrations");
/**
 * Apply connection-level PRAGMAs. Shared by `initialize` and `open` so the two
 * paths can't drift.
 *
 * `busy_timeout` is set FIRST, before any pragma that might touch the database
 * file (notably `journal_mode`). If another process holds a write lock at open
 * time, the later pragmas — and the connection's first query — then wait out
 * the lock instead of throwing "database is locked" immediately. See issue #238.
 *
 * The 5s window (was 120s) rides out a normal incremental sync; the old
 * 2-minute wait presented as a frozen, hung agent. With WAL, reads never block
 * on a writer, so this timeout only governs cross-process write contention
 * (e.g. the git-hook `codegraph sync` running while the MCP server writes).
 */
function configureConnection(db) {
    db.pragma('busy_timeout = 5000'); // MUST be first — see above
    db.pragma('foreign_keys = ON');
    db.pragma('journal_mode = WAL'); // node:sqlite supports WAL on every platform
    db.pragma('synchronous = NORMAL'); // safe with WAL mode
    db.pragma('cache_size = -64000'); // 64 MB page cache
    db.pragma('temp_store = MEMORY'); // temp tables in memory
    db.pragma('mmap_size = 268435456'); // 256 MB memory-mapped I/O
}
/**
 * Database connection wrapper with lifecycle management
 */
class DatabaseConnection {
    db;
    dbPath;
    backend;
    constructor(db, dbPath, backend) {
        this.db = db;
        this.dbPath = dbPath;
        this.backend = backend;
    }
    /**
     * Initialize a new database at the given path
     */
    static initialize(dbPath) {
        // Ensure parent directory exists
        const dir = path.dirname(dbPath);
        if (!fs.existsSync(dir)) {
            fs.mkdirSync(dir, { recursive: true });
        }
        // Create and configure database
        const { db, backend } = (0, sqlite_adapter_1.createDatabase)(dbPath);
        configureConnection(db);
        // Run schema initialization
        const schemaPath = path.join(__dirname, 'schema.sql');
        const schema = fs.readFileSync(schemaPath, 'utf-8');
        db.exec(schema);
        // Record current schema version so migrations aren't re-applied on open
        const currentVersion = (0, migrations_1.getCurrentVersion)(db);
        if (currentVersion < migrations_1.CURRENT_SCHEMA_VERSION) {
            db.prepare('INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (?, ?, ?)').run(migrations_1.CURRENT_SCHEMA_VERSION, Date.now(), 'Initial schema includes all migrations');
        }
        return new DatabaseConnection(db, dbPath, backend);
    }
    /**
     * Open an existing database
     */
    static open(dbPath) {
        if (!fs.existsSync(dbPath)) {
            throw new Error(`Database not found: ${dbPath}`);
        }
        const { db, backend } = (0, sqlite_adapter_1.createDatabase)(dbPath);
        configureConnection(db);
        // Check and run migrations if needed
        const conn = new DatabaseConnection(db, dbPath, backend);
        const currentVersion = (0, migrations_1.getCurrentVersion)(db);
        if (currentVersion < migrations_1.CURRENT_SCHEMA_VERSION) {
            (0, migrations_1.runMigrations)(db, currentVersion);
        }
        return conn;
    }
    /**
     * Get the underlying database instance
     */
    getDb() {
        return this.db;
    }
    /**
     * Get the SQLite backend serving this connection. Per-instance so
     * MCP cross-project queries report the right backend even when
     * multiple project DBs are open in the same process.
     */
    getBackend() {
        return this.backend;
    }
    /**
     * Get database file path
     */
    getPath() {
        return this.dbPath;
    }
    /**
     * The journal mode actually in effect (e.g. 'wal', 'delete').
     *
     * SQLite silently keeps the prior mode if WAL can't be enabled — e.g. on
     * filesystems without shared-memory support (some network/virtualized mounts,
     * WSL2 /mnt), and always on the wasm backend. So the effective mode can differ
     * from what `configureConnection` requested. Surfaced in `codegraph status` so
     * a "database is locked" report is triageable: 'wal' ⇒ readers never block on a
     * writer; anything else ⇒ they can. See issue #238.
     */
    getJournalMode() {
        const raw = this.db.pragma('journal_mode');
        const row = Array.isArray(raw) ? raw[0] : raw;
        const mode = row && typeof row === 'object'
            ? row.journal_mode
            : row;
        return String(mode ?? '').toLowerCase();
    }
    /**
     * Get current schema version
     */
    getSchemaVersion() {
        const row = this.db
            .prepare('SELECT version, applied_at, description FROM schema_versions ORDER BY version DESC LIMIT 1')
            .get();
        if (!row)
            return null;
        return {
            version: row.version,
            appliedAt: row.applied_at,
            description: row.description ?? undefined,
        };
    }
    /**
     * Execute a function within a transaction
     */
    transaction(fn) {
        return this.db.transaction(fn)();
    }
    /**
     * Get database file size in bytes
     */
    getSize() {
        const stats = fs.statSync(this.dbPath);
        return stats.size;
    }
    /**
     * Optimize database (vacuum and analyze)
     */
    optimize() {
        this.db.exec('VACUUM');
        this.db.exec('ANALYZE');
    }
    /**
     * Lightweight, non-blocking maintenance to run after bulk writes
     * (indexAll, sync). Two operations:
     *
     *   - `PRAGMA optimize` — incremental ANALYZE; SQLite only re-analyzes
     *     tables whose row counts changed materially since the last
     *     ANALYZE. Without it, the query planner has no statistics on the
     *     freshly-bulk-loaded tables and can pick suboptimal indexes.
     *
     *   - `PRAGMA wal_checkpoint(PASSIVE)` — fold pending WAL pages back
     *     into the main database file so the WAL file doesn't grow
     *     unboundedly between automatic checkpoints (auto-fires at 1000
     *     pages by default; large indexAll runs blow past that).
     *
     * Both operations are silently swallowed on failure — they're a
     * best-effort optimization, never load-bearing for correctness.
     */
    runMaintenance() {
        try {
            this.db.exec('PRAGMA optimize');
        }
        catch {
            // ignore
        }
        try {
            this.db.exec('PRAGMA wal_checkpoint(PASSIVE)');
        }
        catch {
            // ignore (e.g., not in WAL mode)
        }
    }
    /**
     * Close the database connection
     */
    close() {
        this.db.close();
    }
    /**
     * Check if the database connection is open
     */
    isOpen() {
        return this.db.open;
    }
}
exports.DatabaseConnection = DatabaseConnection;
/**
 * Default database filename
 */
exports.DATABASE_FILENAME = 'codegraph.db';
/**
 * Get the default database path for a project
 */
function getDatabasePath(projectRoot) {
    return path.join(projectRoot, '.codegraph', exports.DATABASE_FILENAME);
}
//# sourceMappingURL=index.js.map