/**
 * Backwards-compat shim — original Claude-only writer functions.
 *
 * The installer now uses the multi-target architecture in
 * `./targets/`. This file is preserved so existing imports (the test
 * suite, downstream tooling) keep working unchanged. Each function
 * delegates to the Claude target. New code should import the target
 * registry from `./targets/registry` directly.
 *
 * @deprecated Use `targets/registry.ts` and the `AgentTarget`
 *   abstraction instead.
 */
export type InstallLocation = 'global' | 'local';
/**
 * Each shim calls ONLY the named per-file helper — writeMcpConfig
 * writes only the MCP JSON, writePermissions only settings.json,
 * writeClaudeMd only CLAUDE.md. The full multi-file install lives
 * in `claudeTarget.install()` which the new orchestrator uses.
 */
export declare function writeMcpConfig(location: InstallLocation): void;
export declare function writePermissions(location: InstallLocation): void;
export declare function writeClaudeMd(location: InstallLocation): {
    created: boolean;
    updated: boolean;
};
export declare function hasMcpConfig(location: InstallLocation): boolean;
export declare function hasPermissions(location: InstallLocation): boolean;
export declare function hasClaudeMdSection(location: InstallLocation): boolean;
//# sourceMappingURL=config-writer.d.ts.map