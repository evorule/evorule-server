"use strict";
/**
 * Backwards-compat re-export shim.
 *
 * The instructions template moved to `instructions-template.ts` so it
 * can be shared across all agent targets (Claude Code, Cursor, Codex
 * CLI, opencode). This file is preserved purely so existing imports
 * (`@colbymchenry/codegraph` consumers, downstream tooling) keep
 * working unchanged. New code should import from
 * `./instructions-template` directly.
 *
 * @deprecated Import from `./instructions-template` instead.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.INSTRUCTIONS_TEMPLATE = exports.CLAUDE_MD_TEMPLATE = exports.CODEGRAPH_SECTION_END = exports.CODEGRAPH_SECTION_START = void 0;
var instructions_template_1 = require("./instructions-template");
Object.defineProperty(exports, "CODEGRAPH_SECTION_START", { enumerable: true, get: function () { return instructions_template_1.CODEGRAPH_SECTION_START; } });
Object.defineProperty(exports, "CODEGRAPH_SECTION_END", { enumerable: true, get: function () { return instructions_template_1.CODEGRAPH_SECTION_END; } });
Object.defineProperty(exports, "CLAUDE_MD_TEMPLATE", { enumerable: true, get: function () { return instructions_template_1.CLAUDE_MD_TEMPLATE; } });
Object.defineProperty(exports, "INSTRUCTIONS_TEMPLATE", { enumerable: true, get: function () { return instructions_template_1.INSTRUCTIONS_TEMPLATE; } });
//# sourceMappingURL=claude-md-template.js.map