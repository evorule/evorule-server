"use strict";
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
exports.hasClaudeMdSection = exports.hasPermissions = exports.hasMcpConfig = exports.writeClaudeMd = exports.writePermissions = exports.writeMcpConfig = void 0;
exports.runInstaller = runInstaller;
exports.runInstallerWithOptions = runInstallerWithOptions;
exports.uninstallTargets = uninstallTargets;
exports.runUninstaller = runUninstaller;
exports.wireProjectSurfacesForGlobalAgents = wireProjectSurfacesForGlobalAgents;
exports.offerWatchFallback = offerWatchFallback;
const child_process_1 = require("child_process");
const path = __importStar(require("path"));
const fs = __importStar(require("fs"));
const registry_1 = require("./targets/registry");
const glyphs_1 = require("../ui/glyphs");
// Import the lightweight submodules directly (not the ../sync barrel, which
// re-exports FileWatcher and would transitively pull in ../extraction — the
// installer must stay importable even when native modules can't load).
const watch_policy_1 = require("../sync/watch-policy");
const git_hooks_1 = require("../sync/git-hooks");
// Backwards-compat: keep these named exports — downstream code may
// import them. The shim in `config-writer.ts` continues to re-export
// them too.
var config_writer_1 = require("./config-writer");
Object.defineProperty(exports, "writeMcpConfig", { enumerable: true, get: function () { return config_writer_1.writeMcpConfig; } });
Object.defineProperty(exports, "writePermissions", { enumerable: true, get: function () { return config_writer_1.writePermissions; } });
Object.defineProperty(exports, "writeClaudeMd", { enumerable: true, get: function () { return config_writer_1.writeClaudeMd; } });
Object.defineProperty(exports, "hasMcpConfig", { enumerable: true, get: function () { return config_writer_1.hasMcpConfig; } });
Object.defineProperty(exports, "hasPermissions", { enumerable: true, get: function () { return config_writer_1.hasPermissions; } });
Object.defineProperty(exports, "hasClaudeMdSection", { enumerable: true, get: function () { return config_writer_1.hasClaudeMdSection; } });
// Dynamic import helper — tsc compiles import() to require() in CJS mode,
// which fails for ESM-only packages. This bypasses the transformation.
// eslint-disable-next-line @typescript-eslint/no-implied-eval
const importESM = new Function('specifier', 'return import(specifier)');
function formatNumber(n) {
    return n.toLocaleString();
}
function getVersion() {
    try {
        const packageJsonPath = path.join(__dirname, '..', '..', 'package.json');
        const packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf-8'));
        return packageJson.version;
    }
    catch {
        return '0.0.0';
    }
}
/**
 * Interactive entry point — preserves the historical UX (`codegraph
 * install` with no args goes through the prompts), but now starts
 * the targets multi-select pre-populated with detected agents.
 */
async function runInstaller() {
    return runInstallerWithOptions({});
}
async function runInstallerWithOptions(opts) {
    const clack = await importESM('@clack/prompts');
    clack.intro(`CodeGraph v${getVersion()}`);
    // --yes implies all defaults; explicit flags still win.
    const useDefaults = opts.yes === true;
    // Step 1: which agent targets? Asked FIRST so the user knows what
    // they're committing to before we touch npm or disk. Detection
    // probes the user-provided location if known, else 'global' as the
    // most common default — labels are a hint, not load-bearing.
    const detectionLocation = opts.location ?? 'global';
    const targets = await resolveTargets(clack, opts, detectionLocation, useDefaults);
    if (targets.length === 0) {
        clack.outro('No agent targets selected — nothing to do.');
        return;
    }
    // Step 2: install the codegraph npm package on PATH (always offered;
    // matches existing behavior). Skipped when --yes (assume present).
    if (!useDefaults) {
        const shouldInstallGlobally = await clack.confirm({
            message: 'Install the codegraph CLI on your PATH? (Required so agents can launch the MCP server)',
            initialValue: true,
        });
        if (clack.isCancel(shouldInstallGlobally)) {
            clack.cancel('Installation cancelled.');
            process.exit(0);
        }
        if (shouldInstallGlobally) {
            const s = clack.spinner();
            s.start('Installing codegraph CLI...');
            try {
                (0, child_process_1.execSync)('npm install -g @colbymchenry/codegraph', { stdio: 'pipe' });
                s.stop('Installed codegraph CLI on PATH');
            }
            catch {
                s.stop('Could not install (permission denied)');
                clack.log.warn('Try: sudo npm install -g @colbymchenry/codegraph');
            }
        }
        else {
            clack.log.info('Skipped CLI install — agents will not be able to launch the MCP server without it');
        }
    }
    // Step 3: where the per-agent config files should land.
    let location;
    if (opts.location) {
        location = opts.location;
    }
    else if (useDefaults) {
        location = 'global';
    }
    else {
        // If every selected target is global-only (e.g. Codex), skip the
        // prompt and force user-wide — project-local would just produce
        // skip warnings.
        const allGlobalOnly = targets.every((t) => !t.supportsLocation('local'));
        if (allGlobalOnly) {
            location = 'global';
            clack.log.info('Writing user-wide configs (selected agents have no project-local config).');
        }
        else {
            const sel = await clack.select({
                message: 'Apply agent configs to all your projects, or just this one?',
                options: [
                    { value: 'global', label: 'All projects', hint: '~/.claude, ~/.cursor, etc.' },
                    { value: 'local', label: 'Just this project', hint: './.claude, ./.cursor, etc.' },
                ],
                initialValue: 'global',
            });
            if (clack.isCancel(sel)) {
                clack.cancel('Installation cancelled.');
                process.exit(0);
            }
            location = sel;
        }
    }
    // Step 4: auto-allow permissions (only meaningful for Claude;
    // skipped silently by other targets).
    let autoAllow;
    if (opts.autoAllow !== undefined) {
        autoAllow = opts.autoAllow;
    }
    else if (useDefaults) {
        autoAllow = true;
    }
    else if (targets.some((t) => t.id === 'claude')) {
        const ans = await clack.confirm({
            message: 'Auto-allow CodeGraph commands? (Skips permission prompts in Claude Code)',
            initialValue: true,
        });
        if (clack.isCancel(ans)) {
            clack.cancel('Installation cancelled.');
            process.exit(0);
        }
        autoAllow = ans;
    }
    else {
        autoAllow = false;
    }
    // Step 5: per-target install loop.
    for (const target of targets) {
        if (!target.supportsLocation(location)) {
            clack.log.warn(`${target.displayName}: skipped — does not support --location=${location}.`);
            continue;
        }
        const result = target.install(location, { autoAllow });
        for (const file of result.files) {
            const verb = file.action === 'unchanged'
                ? 'Unchanged'
                : file.action === 'created' ? 'Created' : 'Updated';
            clack.log.success(`${target.displayName}: ${verb} ${tildify(file.path)}`);
        }
        for (const note of result.notes ?? []) {
            clack.log.info(`${target.displayName}: ${note}`);
        }
    }
    // Step 6: for local install, initialize the project.
    if (location === 'local') {
        await initializeLocalProject(clack, useDefaults);
    }
    if (location === 'global') {
        clack.note('cd your-project\ncodegraph init -i', 'Quick start');
    }
    const finalNote = targets.length > 0
        ? `Done! Restart your agent${targets.length > 1 ? 's' : ''} to use CodeGraph.`
        : 'Done!';
    clack.outro(finalNote);
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
function uninstallTargets(targets, location) {
    return targets.map((target) => {
        if (!target.supportsLocation(location)) {
            const only = location === 'local' ? 'global' : 'local';
            return {
                id: target.id,
                displayName: target.displayName,
                status: 'unsupported',
                removedPaths: [],
                notes: [`no ${location} config — this agent is ${only}-only`],
            };
        }
        const result = target.uninstall(location);
        const removedPaths = result.files
            .filter((f) => f.action === 'removed')
            .map((f) => f.path);
        return {
            id: target.id,
            displayName: target.displayName,
            status: removedPaths.length > 0 ? 'removed' : 'not-configured',
            removedPaths,
            notes: result.notes ?? [],
        };
    });
}
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
async function runUninstaller(opts) {
    const clack = await importESM('@clack/prompts');
    clack.intro(`CodeGraph v${getVersion()} — uninstall`);
    const useDefaults = opts.yes === true;
    // Step 1: which location — asked FIRST, the one decision the user
    // must make. Global sweeps ~/.claude, ~/.codex, etc.; local sweeps
    // the configs in this project directory.
    let location;
    if (opts.location) {
        location = opts.location;
    }
    else if (useDefaults) {
        location = 'global';
    }
    else {
        const sel = await clack.select({
            message: 'Remove CodeGraph from all your projects, or just this one?',
            options: [
                { value: 'global', label: 'All projects (global)', hint: '~/.claude, ~/.cursor, ~/.codex, ~/.config/opencode, ~/.hermes' },
                { value: 'local', label: 'Just this project (local)', hint: './.claude, ./.cursor, ./opencode.jsonc' },
            ],
            initialValue: 'global',
        });
        if (clack.isCancel(sel)) {
            clack.cancel('Uninstall cancelled.');
            process.exit(0);
        }
        location = sel;
    }
    // Step 2: which agents. Default is every agent, so the user doesn't
    // have to remember where they installed it — unconfigured agents are
    // reported as "nothing to remove" and left untouched. An explicit
    // --target subsets this.
    let targets;
    if (opts.target !== undefined) {
        targets = (0, registry_1.resolveTargetFlag)(opts.target, location);
    }
    else {
        targets = [...registry_1.ALL_TARGETS];
    }
    if (targets.length === 0) {
        clack.outro('No agent targets selected — nothing to do.');
        return;
    }
    // Step 3: sweep + per-agent feedback.
    const reports = uninstallTargets(targets, location);
    const removed = reports.filter((r) => r.status === 'removed');
    for (const r of reports) {
        if (r.status === 'removed') {
            for (const p of r.removedPaths) {
                clack.log.success(`${r.displayName}: removed ${tildify(p)}`);
            }
        }
        else if (r.status === 'not-configured') {
            clack.log.info(`${r.displayName}: not configured — nothing to remove`);
        }
        else {
            clack.log.info(`${r.displayName}: skipped — ${r.notes[0] ?? 'unsupported location'}`);
        }
    }
    // Step 4: for local uninstall, the index dir is separate — point at
    // `uninit` so the user knows it's still there (and how to remove it).
    if (location === 'local' && fs.existsSync(path.join(process.cwd(), '.codegraph'))) {
        clack.log.info('The .codegraph/ index for this project is still here. Run `codegraph uninit` to delete it.');
    }
    // Step 5: summary.
    if (removed.length > 0) {
        const names = removed.map((r) => r.displayName).join(', ');
        clack.outro(`Removed CodeGraph from ${removed.length} agent${removed.length > 1 ? 's' : ''}: ${names}. ` +
            `Restart ${removed.length > 1 ? 'them' : 'it'} to apply.`);
    }
    else {
        clack.outro(`CodeGraph was not configured in any ${location} agent — nothing to remove.`);
    }
}
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
function wireProjectSurfacesForGlobalAgents() {
    const written = [];
    for (const target of registry_1.ALL_TARGETS) {
        if (typeof target.wireProjectSurfaces !== 'function')
            continue;
        const detection = target.detect('global');
        if (!detection.alreadyConfigured)
            continue;
        const result = target.wireProjectSurfaces();
        for (const file of result.files) {
            if (file.action === 'created' || file.action === 'updated') {
                written.push({ target, file });
            }
        }
    }
    return written;
}
/**
 * Replace home-directory prefix in a path with `~/` for cleaner log
 * lines. Pure cosmetic.
 */
function tildify(p) {
    const home = require('os').homedir();
    if (p.startsWith(home + path.sep))
        return '~' + p.substring(home.length);
    return p;
}
async function resolveTargets(clack, opts, location, useDefaults) {
    // Explicit --target flag wins.
    if (opts.target !== undefined) {
        return (0, registry_1.resolveTargetFlag)(opts.target, location);
    }
    // --yes implies auto-detect.
    if (useDefaults) {
        return (0, registry_1.resolveTargetFlag)('auto', location);
    }
    // Interactive multi-select.
    const detected = (0, registry_1.detectAll)(location);
    const initialValues = detected
        .filter(({ detection }) => detection.installed)
        .map(({ target }) => target.id);
    // If nothing detected, default to Claude alone (matches the
    // historical default and the smallest-surprise outcome).
    const initial = initialValues.length > 0 ? initialValues : ['claude'];
    const choice = await clack.multiselect({
        message: 'Which agents should CodeGraph configure?',
        options: registry_1.ALL_TARGETS.map((t) => {
            const det = detected.find(({ target }) => target.id === t.id).detection;
            const flag = det.installed ? '(detected)' : '(not found)';
            const globalOnly = !t.supportsLocation('local') ? ' — global only' : '';
            return {
                value: t.id,
                label: `${t.displayName} ${flag}${globalOnly}`,
            };
        }),
        initialValues: initial,
        required: false,
    });
    if (clack.isCancel(choice)) {
        clack.cancel('Installation cancelled.');
        process.exit(0);
    }
    return choice
        .map((id) => (0, registry_1.getTarget)(id))
        .filter((t) => t !== undefined);
}
/**
 * Initialize CodeGraph in the current project (for local installs), then
 * offer the watch fallback when the live watcher won't run here (see
 * offerWatchFallback). Agent-agnostic by nature.
 */
async function initializeLocalProject(clack, useDefaults = false) {
    const projectPath = process.cwd();
    let CodeGraph;
    try {
        CodeGraph = (await Promise.resolve().then(() => __importStar(require('../index')))).default;
    }
    catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        clack.log.error(`Could not load native modules: ${msg}`);
        clack.log.info('Skipping project initialization. Run "codegraph init -i" later.');
        return;
    }
    // Check if already initialized
    if (CodeGraph.isInitialized(projectPath)) {
        clack.log.info('CodeGraph already initialized in this project');
        await offerWatchFallback(clack, projectPath, { yes: useDefaults });
        return;
    }
    // Initialize
    const cg = await CodeGraph.init(projectPath);
    clack.log.success('Created .codegraph/ directory');
    // Index the project with shimmer progress (worker thread for smooth animation)
    const { createShimmerProgress } = await Promise.resolve().then(() => __importStar(require('../ui/shimmer-progress')));
    process.stdout.write(`\x1b[2m${(0, glyphs_1.getGlyphs)().rail}\x1b[0m\n`);
    const progress = createShimmerProgress();
    const result = await cg.indexAll({
        onProgress: progress.onProgress,
    });
    await progress.stop();
    if (result.filesErrored > 0) {
        clack.log.success(`Indexed ${formatNumber(result.filesIndexed)} files (${formatNumber(result.filesErrored)} failed, ${formatNumber(result.nodesCreated)} symbols)`);
    }
    else {
        clack.log.success(`Indexed ${formatNumber(result.filesIndexed)} files (${formatNumber(result.nodesCreated)} symbols)`);
    }
    cg.close();
    await offerWatchFallback(clack, projectPath, { yes: useDefaults });
}
/**
 * When the live file watcher will be disabled for this project (e.g. WSL2
 * /mnt drives, or CODEGRAPH_NO_WATCH), the index would silently go stale.
 * Explain that, and offer to keep it fresh automatically via git hooks
 * (commit / pull / checkout) instead of manual `codegraph sync`.
 *
 * No-op on environments where the watcher runs normally, so it's safe to
 * call unconditionally after init.
 */
async function offerWatchFallback(clack, projectPath, opts = {}) {
    const reason = (0, watch_policy_1.watchDisabledReason)(projectPath);
    if (!reason)
        return; // Watcher runs normally — nothing to set up.
    clack.log.warn(`Live file watching is disabled here — ${reason}.`);
    clack.log.info('Until you re-sync, the CodeGraph index stays frozen — it will not pick up edits on its own.');
    // No git repo → the commit-hook path doesn't apply; point at manual sync.
    if (!(0, git_hooks_1.isGitRepo)(projectPath)) {
        clack.log.info('Run `codegraph sync` after changing files to refresh the index.');
        return;
    }
    // Already wired up on a previous run — confirm and move on without nagging.
    if ((0, git_hooks_1.isSyncHookInstalled)(projectPath)) {
        clack.log.info('Git sync hooks are already installed — the index refreshes after commit / pull / checkout.');
        return;
    }
    let choice;
    if (opts.yes) {
        choice = 'hook';
    }
    else {
        const sel = await clack.select({
            message: 'How should CodeGraph keep its index fresh?',
            options: [
                { value: 'hook', label: 'Sync on git commit / pull / checkout', hint: 'installs git hooks (recommended)' },
                { value: 'manual', label: 'I\'ll run `codegraph sync` myself', hint: 'fully manual' },
            ],
            initialValue: 'hook',
        });
        if (clack.isCancel(sel)) {
            clack.log.info('Skipped — run `codegraph sync` after changes to refresh the index.');
            return;
        }
        choice = sel;
    }
    if (choice === 'manual') {
        clack.log.info('Run `codegraph sync` after changing files to refresh the index.');
        return;
    }
    const result = (0, git_hooks_1.installGitSyncHook)(projectPath);
    if (result.installed.length > 0) {
        clack.log.success(`Installed git ${result.installed.join(', ')} hook${result.installed.length > 1 ? 's' : ''} — ` +
            'the index refreshes in the background after each.');
        clack.log.info('Run `codegraph sync` anytime to refresh immediately.');
    }
    else {
        clack.log.warn(`Could not install git hooks${result.skipped ? ` (${result.skipped})` : ''}. ` +
            'Run `codegraph sync` after changes instead.');
    }
}
//# sourceMappingURL=index.js.map