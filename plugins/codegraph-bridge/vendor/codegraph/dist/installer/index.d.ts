/**
 * CodeGraph Interactive Installer
 *
 * Multi-target: writes MCP server config + instructions for the
 * agents the user picks (Claude Code, Cursor, Codex CLI, opencode,
 * Hermes Agent).
 * Defaults to the Claude-only behavior for backwards compatibility
 * when no targets are explicitly chosen and nothing else is detected.
 *
 * Uses @clack/prompts for the interactive UI; `runInstallerWithOptions`
 * is the non-interactive entry point used by the `--target` /
 * `--print-config` CLI flags.
 */
import type { AgentTarget, Location, TargetId, WriteResult } from './targets/types';
export { writeMcpConfig, writePermissions, writeClaudeMd, hasMcpConfig, hasPermissions, hasClaudeMdSection, } from './config-writer';
export type { InstallLocation } from './config-writer';
export interface RunInstallerOptions {
    /** Comma-separated target list, or `auto` / `all` / `none`. */
    target?: string;
    /** Skip the location prompt; use this value directly. */
    location?: Location;
    /** Skip the auto-allow prompt; use this value directly. */
    autoAllow?: boolean;
    /**
     * Skip every confirm and use defaults: location=global,
     * autoAllow=true, target=auto. For scripting / CI.
     */
    yes?: boolean;
}
/**
 * Interactive entry point — preserves the historical UX (`codegraph
 * install` with no args goes through the prompts), but now starts
 * the targets multi-select pre-populated with detected agents.
 */
export declare function runInstaller(): Promise<void>;
export declare function runInstallerWithOptions(opts: RunInstallerOptions): Promise<void>;
export interface RunUninstallerOptions {
    /**
     * Comma-separated target list, or `auto` / `all` / `none`. Defaults
     * to `all` — uninstall sweeps every known agent and reports which
     * ones it actually touched, so the user doesn't have to know where
     * they configured it.
     */
    target?: string;
    /** Skip the location prompt; use this value directly. */
    location?: Location;
    /** Non-interactive: location=global, target=all, no prompts. */
    yes?: boolean;
}
export type UninstallStatus = 'removed' | 'not-configured' | 'unsupported';
/**
 * Per-target outcome of an uninstall sweep. `removed` means we deleted
 * at least one thing; `not-configured` means the agent had no codegraph
 * config at this location (nothing to do); `unsupported` means the
 * agent has no config concept for this location (e.g. Codex is
 * global-only, so a `local` uninstall skips it).
 */
export interface UninstallReport {
    id: TargetId;
    displayName: string;
    status: UninstallStatus;
    /** Absolute paths we actually edited/removed (action === 'removed'). */
    removedPaths: string[];
    /** Verbatim notes from the target (rare for uninstall). */
    notes: string[];
}
/**
 * Pure uninstall sweep — no prompts, no I/O beyond the targets' own
 * file edits. Exposed (and unit-tested) separately from the clack UI in
 * `runUninstaller` so the aggregation logic can be asserted directly.
 *
 * Each target's `uninstall()` is already safe to call when nothing was
 * installed (it returns `not-found` actions), so this is safe to run
 * across every target unconditionally.
 */
export declare function uninstallTargets(targets: readonly AgentTarget[], location: Location): UninstallReport[];
/**
 * Interactive uninstaller — the inverse of `runInstallerWithOptions`.
 * Asks global-vs-local first (unless `--location`/`--yes` is given),
 * then sweeps every agent target (or the `--target` subset) and prints
 * one block per agent so the user sees exactly which providers it hit.
 *
 * Removes only what install wrote (MCP server entry, instructions
 * block, permissions) — never the `.codegraph/` index, which `codegraph
 * uninit` owns.
 */
export declare function runUninstaller(opts: RunUninstallerOptions): Promise<void>;
/**
 * For every target that has a global config and exposes
 * `wireProjectSurfaces`, write its project-local surfaces (e.g.
 * Cursor's `.cursor/rules/codegraph.mdc`). Idempotent — runs
 * silently when there's nothing to write.
 *
 * Called by `codegraph init` so that a user who ran
 * `codegraph install` once globally doesn't have to re-run it per
 * project to get full agent support.
 *
 * Returns the list of `(target, file)` pairs that were created or
 * updated — caller decides how to surface them.
 */
export declare function wireProjectSurfacesForGlobalAgents(): Array<{
    target: AgentTarget;
    file: WriteResult['files'][number];
}>;
/**
 * When the live file watcher will be disabled for this project (e.g. WSL2
 * /mnt drives, or CODEGRAPH_NO_WATCH), the index would silently go stale.
 * Explain that, and offer to keep it fresh automatically via git hooks
 * (commit / pull / checkout) instead of manual `codegraph sync`.
 *
 * No-op on environments where the watcher runs normally, so it's safe to
 * call unconditionally after init.
 */
export declare function offerWatchFallback(clack: typeof import('@clack/prompts'), projectPath: string, opts?: {
    yes?: boolean;
}): Promise<void>;
//# sourceMappingURL=index.d.ts.map