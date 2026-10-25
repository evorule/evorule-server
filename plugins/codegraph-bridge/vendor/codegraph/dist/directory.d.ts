/**
 * Directory Management
 *
 * Manages the .codegraph/ directory structure for CodeGraph data.
 */
/**
 * CodeGraph directory name
 */
export declare const CODEGRAPH_DIR = ".codegraph";
/**
 * Get the .codegraph directory path for a project
 */
export declare function getCodeGraphDir(projectRoot: string): string;
/**
 * Check if a project has been initialized with CodeGraph
 * Requires both .codegraph/ directory AND codegraph.db to exist
 */
export declare function isInitialized(projectRoot: string): boolean;
/**
 * Find the nearest parent directory containing .codegraph/
 *
 * Walks up from the given path to find a CodeGraph-initialized project,
 * similar to how git finds .git/ directories.
 *
 * @param startPath - Directory to start searching from
 * @returns The project root containing .codegraph/, or null if not found
 */
export declare function findNearestCodeGraphRoot(startPath: string): string | null;
/**
 * Create the .codegraph directory structure
 * Note: Only throws if codegraph.db already exists, not just if .codegraph/ exists.
 */
export declare function createDirectory(projectRoot: string): void;
/**
 * Remove the .codegraph directory
 */
export declare function removeDirectory(projectRoot: string): void;
/**
 * Get all files in the .codegraph directory
 */
export declare function listDirectoryContents(projectRoot: string): string[];
/**
 * Get the total size of the .codegraph directory in bytes
 */
export declare function getDirectorySize(projectRoot: string): number;
/**
 * Ensure a subdirectory exists within .codegraph
 */
export declare function ensureSubdirectory(projectRoot: string, subdirName: string): string;
/**
 * Check if the .codegraph directory has valid structure
 */
export declare function validateDirectory(projectRoot: string): {
    valid: boolean;
    errors: string[];
};
//# sourceMappingURL=directory.d.ts.map