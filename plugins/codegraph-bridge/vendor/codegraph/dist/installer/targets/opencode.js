"use strict";
/**
 * opencode target.
 *
 *   - MCP server entry to `~/.config/opencode/opencode.jsonc` (global,
 *     XDG-style; `%APPDATA%/opencode/opencode.jsonc` on Windows) or
 *     `./opencode.jsonc` (local). Falls back to `opencode.json` when a
 *     `.json` file already exists; defaults new installs to `.jsonc`
 *     because that's what opencode itself creates on first run.
 *   - Instructions to `~/.config/opencode/AGENTS.md` (global) or
 *     `./AGENTS.md` (local). opencode reads AGENTS.md for agent
 *     instructions — same convention Codex CLI uses.
 *   - No permissions concept.
 *
 * Config shape uses opencode's wrapper:
 *   {
 *     "$schema": "https://opencode.ai/config.json",
 *     "mcp": { "codegraph": { "type": "local", "command": [...], "enabled": true } }
 *   }
 *
 * The shape differs from Claude/Cursor — opencode uses `mcp.<name>`
 * (not `mcpServers`), takes `command` as a string array combining
 * binary + args, and includes an explicit `enabled` flag.
 *
 * Reads + writes go through `jsonc-parser` so any `//` and `/* *\/`
 * comments the user has added to their `.jsonc` survive idempotent
 * re-runs.
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
exports.opencodeTarget = void 0;
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const os = __importStar(require("os"));
const jsonc_parser_1 = require("jsonc-parser");
const shared_1 = require("./shared");
const instructions_template_1 = require("../instructions-template");
function globalConfigDir() {
    if (process.platform === 'win32') {
        const appData = process.env.APPDATA ?? path.join(os.homedir(), 'AppData', 'Roaming');
        return path.join(appData, 'opencode');
    }
    // XDG_CONFIG_HOME if set, else ~/.config — matches opencode's docs.
    const xdg = process.env.XDG_CONFIG_HOME && process.env.XDG_CONFIG_HOME.trim().length > 0
        ? process.env.XDG_CONFIG_HOME
        : path.join(os.homedir(), '.config');
    return path.join(xdg, 'opencode');
}
function configBaseDir(loc) {
    return loc === 'global' ? globalConfigDir() : process.cwd();
}
// Pick existing .jsonc, then .json, default to .jsonc for new files.
// opencode auto-creates .jsonc on first run, so that's the dominant
// real-world case and the sensible default for greenfield installs.
function configPath(loc) {
    const dir = configBaseDir(loc);
    const jsonc = path.join(dir, 'opencode.jsonc');
    const json = path.join(dir, 'opencode.json');
    if (fs.existsSync(jsonc))
        return jsonc;
    if (fs.existsSync(json))
        return json;
    return jsonc;
}
function instructionsPath(loc) {
    return path.join(configBaseDir(loc), 'AGENTS.md');
}
function readConfigText(file) {
    if (!fs.existsSync(file))
        return '';
    return fs.readFileSync(file, 'utf-8');
}
function parseConfig(text) {
    if (!text.trim())
        return {};
    const errors = [];
    const result = (0, jsonc_parser_1.parse)(text, errors, { allowTrailingComma: true });
    if (result == null || typeof result !== 'object' || Array.isArray(result)) {
        return {};
    }
    return result;
}
function getOpencodeServerEntry() {
    return {
        type: 'local',
        command: ['codegraph', 'serve', '--mcp'],
        enabled: true,
    };
}
const FORMATTING = { tabSize: 2, insertSpaces: true, eol: '\n' };
class OpencodeTarget {
    id = 'opencode';
    displayName = 'opencode';
    docsUrl = 'https://opencode.ai/docs/config';
    supportsLocation(_loc) {
        return true;
    }
    detect(loc) {
        const file = configPath(loc);
        const config = parseConfig(readConfigText(file));
        const alreadyConfigured = !!config.mcp?.codegraph;
        const installed = loc === 'global'
            ? fs.existsSync(globalConfigDir())
            : fs.existsSync(file);
        return { installed, alreadyConfigured, configPath: file };
    }
    install(loc, _opts) {
        const files = [];
        files.push(writeMcpEntry(loc));
        files.push(writeInstructionsEntry(loc));
        return { files };
    }
    uninstall(loc) {
        const files = [];
        const file = configPath(loc);
        if (!fs.existsSync(file)) {
            files.push({ path: file, action: 'not-found' });
        }
        else {
            const text = readConfigText(file);
            const config = parseConfig(text);
            if (!config.mcp?.codegraph) {
                files.push({ path: file, action: 'not-found' });
            }
            else {
                // Drop our key surgically. Leaves siblings + comments untouched.
                let edits = (0, jsonc_parser_1.modify)(text, ['mcp', 'codegraph'], undefined, {
                    formattingOptions: FORMATTING,
                });
                let updated = (0, jsonc_parser_1.applyEdits)(text, edits);
                // If `mcp` is now an empty object, drop the wrapper too.
                const afterParsed = parseConfig(updated);
                if (afterParsed.mcp && typeof afterParsed.mcp === 'object' &&
                    Object.keys(afterParsed.mcp).length === 0) {
                    edits = (0, jsonc_parser_1.modify)(updated, ['mcp'], undefined, { formattingOptions: FORMATTING });
                    updated = (0, jsonc_parser_1.applyEdits)(updated, edits);
                }
                (0, shared_1.atomicWriteFileSync)(file, updated);
                files.push({ path: file, action: 'removed' });
            }
        }
        const instr = instructionsPath(loc);
        const instrAction = (0, shared_1.removeMarkedSection)(instr, instructions_template_1.CODEGRAPH_SECTION_START, instructions_template_1.CODEGRAPH_SECTION_END);
        files.push({ path: instr, action: instrAction });
        return { files };
    }
    printConfig(loc) {
        const target = configPath(loc);
        const snippet = JSON.stringify({
            $schema: 'https://opencode.ai/config.json',
            mcp: { codegraph: getOpencodeServerEntry() },
        }, null, 2);
        return `# Add to ${target}\n\n${snippet}\n`;
    }
    describePaths(loc) {
        return [configPath(loc), instructionsPath(loc)];
    }
}
function writeMcpEntry(loc) {
    const file = configPath(loc);
    const existed = fs.existsSync(file);
    let text = readConfigText(file);
    // Seed a minimal opencode config when the file is brand-new so
    // the result is a complete, schema-tagged file (not just a bare
    // `{ "mcp": {...} }`).
    if (!text.trim()) {
        text = '{\n  "$schema": "https://opencode.ai/config.json"\n}\n';
    }
    const config = parseConfig(text);
    const before = config.mcp?.codegraph;
    const after = getOpencodeServerEntry();
    if ((0, shared_1.jsonDeepEqual)(before, after)) {
        return { path: file, action: 'unchanged' };
    }
    // Add $schema if the user's existing file is missing it.
    if (!config.$schema) {
        const schemaEdits = (0, jsonc_parser_1.modify)(text, ['$schema'], 'https://opencode.ai/config.json', {
            formattingOptions: FORMATTING,
        });
        text = (0, jsonc_parser_1.applyEdits)(text, schemaEdits);
    }
    // Surgical edit — preserves comments, formatting, and order of
    // every key we don't touch.
    const edits = (0, jsonc_parser_1.modify)(text, ['mcp', 'codegraph'], after, {
        formattingOptions: FORMATTING,
    });
    const updated = (0, jsonc_parser_1.applyEdits)(text, edits);
    (0, shared_1.atomicWriteFileSync)(file, updated);
    return { path: file, action: existed ? 'updated' : 'created' };
}
function writeInstructionsEntry(loc) {
    const file = instructionsPath(loc);
    const dir = path.dirname(file);
    if (!fs.existsSync(dir))
        fs.mkdirSync(dir, { recursive: true });
    const action = (0, shared_1.replaceOrAppendMarkedSection)(file, instructions_template_1.INSTRUCTIONS_TEMPLATE, instructions_template_1.CODEGRAPH_SECTION_START, instructions_template_1.CODEGRAPH_SECTION_END);
    const mapped = action === 'created' ? 'created'
        : action === 'unchanged' ? 'unchanged'
            : 'updated';
    return { path: file, action: mapped };
}
exports.opencodeTarget = new OpencodeTarget();
//# sourceMappingURL=opencode.js.map