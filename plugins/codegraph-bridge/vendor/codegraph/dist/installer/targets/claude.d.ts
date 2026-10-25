/**
 * Claude Code target. Writes:
 *
 *   - MCP server entry to `~/.claude.json` (global = user scope, loads
 *     in every project) or `./.mcp.json` (local = project scope, the
 *     file Claude Code actually reads for a single project). See the
 *     scope table at https://code.claude.com/docs/en/mcp.
 *   - Permissions to `~/.claude/settings.json` (global) or
 *     `./.claude/settings.json` (local), gated on `autoAllow`.
 *   - Instructions to `~/.claude/CLAUDE.md` (global) or
 *     `./.claude/CLAUDE.md` (local).
 *
 * Earlier versions wrote the local MCP entry to `./.claude.json` — a
 * file Claude Code never reads — so the server silently never loaded
 * until the user manually renamed it to `.mcp.json` (issue #207). We
 * now write `./.mcp.json` and migrate any stale `./.claude.json` entry
 * out of the way on install and uninstall.
 */
import { AgentTarget, Location, WriteResult } from './types';
/**
 * Per-file write helpers, exported so the legacy `config-writer.ts`
 * shim can call only the named operation (writeMcpConfig writes ONLY
 * the MCP entry, etc.) instead of `claudeTarget.install()` which
 * writes all three files. Without this split the shims silently
 * cause side effects callers don't expect.
 */
export declare function writeMcpEntry(loc: Location): WriteResult['files'][number];
/**
 * Remove stale codegraph auto-sync hooks from Claude `settings.json`.
 *
 * Surgical at the individual-command level: only entries matching
 * `isLegacyCodegraphHookCommand` are dropped, so a sibling hook sharing
 * a matcher group (or the Stop event) with ours survives. We prune a
 * matcher group only once its `hooks` array is empty, an event only
 * once it has no groups left, and `hooks` itself only once every event
 * is gone — and none of that runs unless we actually removed a
 * codegraph command, so a settings.json with no legacy hooks is left
 * byte-for-byte untouched and reported `unchanged`.
 *
 * Exported so it can be unit-tested directly and reused by both
 * `install` (an upgrade self-heals) and `uninstall`.
 */
export declare function cleanupLegacyHooks(loc: Location): WriteResult['files'][number];
export declare function writePermissionsEntry(loc: Location): WriteResult['files'][number];
export declare function writeInstructionsEntry(loc: Location): WriteResult['files'][number];
export declare const claudeTarget: AgentTarget;
//# sourceMappingURL=claude.d.ts.map