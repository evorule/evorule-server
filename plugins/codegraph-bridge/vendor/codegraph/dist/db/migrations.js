"use strict";
/**
 * Database Migrations
 *
 * Schema versioning and migration support.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.CURRENT_SCHEMA_VERSION = void 0;
exports.getCurrentVersion = getCurrentVersion;
exports.runMigrations = runMigrations;
exports.needsMigration = needsMigration;
exports.getPendingMigrations = getPendingMigrations;
exports.getMigrationHistory = getMigrationHistory;
/**
 * Current schema version
 */
exports.CURRENT_SCHEMA_VERSION = 4;
/**
 * All migrations in order
 *
 * Note: Version 1 is the initial schema, handled by schema.sql
 * Future migrations go here.
 */
const migrations = [
    {
        version: 2,
        description: 'Add project metadata, provenance tracking, and unresolved ref context',
        up: (db) => {
            db.exec(`
        CREATE TABLE IF NOT EXISTS project_metadata (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL,
          updated_at INTEGER NOT NULL
        );
        ALTER TABLE unresolved_refs ADD COLUMN file_path TEXT NOT NULL DEFAULT '';
        ALTER TABLE unresolved_refs ADD COLUMN language TEXT NOT NULL DEFAULT 'unknown';
        ALTER TABLE edges ADD COLUMN provenance TEXT DEFAULT NULL;
        CREATE INDEX IF NOT EXISTS idx_unresolved_file_path ON unresolved_refs(file_path);
        CREATE INDEX IF NOT EXISTS idx_edges_provenance ON edges(provenance);
      `);
        },
    },
    {
        version: 3,
        description: 'Add lower(name) expression index for memory-efficient case-insensitive lookups',
        up: (db) => {
            db.exec(`
        CREATE INDEX IF NOT EXISTS idx_nodes_lower_name ON nodes(lower(name));
      `);
        },
    },
    {
        version: 4,
        description: 'Drop redundant idx_edges_source / idx_edges_target (covered by source_kind / target_kind composites)',
        up: (db) => {
            db.exec(`
        DROP INDEX IF EXISTS idx_edges_source;
        DROP INDEX IF EXISTS idx_edges_target;
      `);
        },
    },
];
/**
 * Get the current schema version from the database
 */
function getCurrentVersion(db) {
    try {
        const row = db
            .prepare('SELECT MAX(version) as version FROM schema_versions')
            .get();
        return row?.version ?? 0;
    }
    catch {
        // Table doesn't exist yet
        return 0;
    }
}
/**
 * Record a migration as applied
 */
function recordMigration(db, version, description) {
    db.prepare('INSERT INTO schema_versions (version, applied_at, description) VALUES (?, ?, ?)').run(version, Date.now(), description);
}
/**
 * Run all pending migrations
 */
function runMigrations(db, fromVersion) {
    const pending = migrations.filter((m) => m.version > fromVersion);
    if (pending.length === 0) {
        return;
    }
    // Sort by version
    pending.sort((a, b) => a.version - b.version);
    // Run each migration in a transaction
    for (const migration of pending) {
        db.transaction(() => {
            migration.up(db);
            recordMigration(db, migration.version, migration.description);
        })();
    }
}
/**
 * Check if the database needs migration
 */
function needsMigration(db) {
    const current = getCurrentVersion(db);
    return current < exports.CURRENT_SCHEMA_VERSION;
}
/**
 * Get list of pending migrations
 */
function getPendingMigrations(db) {
    const current = getCurrentVersion(db);
    return migrations
        .filter((m) => m.version > current)
        .sort((a, b) => a.version - b.version);
}
/**
 * Get migration history from database
 */
function getMigrationHistory(db) {
    const rows = db
        .prepare('SELECT version, applied_at, description FROM schema_versions ORDER BY version')
        .all();
    return rows.map((row) => ({
        version: row.version,
        appliedAt: row.applied_at,
        description: row.description,
    }));
}
//# sourceMappingURL=migrations.js.map