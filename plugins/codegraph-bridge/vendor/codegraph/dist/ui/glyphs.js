"use strict";
/**
 * Glyph selection for CLI output.
 *
 * On Windows, console output is interpreted via the active output
 * codepage. PowerShell 5.1 and cmd.exe default to OEM codepages
 * (CP437, CP936, ...), so UTF-8 bytes written to the console render
 * as mojibake (see #168). The shimmer worker is hit hardest because
 * it uses `fs.writeSync(1, ...)` (raw bytes, no TTY-aware encoding
 * conversion) to keep animation smooth while the main thread is
 * blocked in SQLite. To stay readable everywhere, we fall back to
 * ASCII glyphs whenever the terminal is not known to handle UTF-8.
 *
 * Detection is intentionally simple:
 *   - `CODEGRAPH_ASCII=1`  -> ASCII (escape hatch for any terminal)
 *   - `CODEGRAPH_UNICODE=1` -> Unicode (opt-in on Windows)
 *   - Windows              -> ASCII by default
 *   - Linux kernel console (`TERM=linux`) -> ASCII
 *   - Everything else      -> Unicode
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.ASCII_GLYPHS = exports.UNICODE_GLYPHS = void 0;
exports.supportsUnicode = supportsUnicode;
exports.getGlyphs = getGlyphs;
exports._resetGlyphsCache = _resetGlyphsCache;
function supportsUnicode() {
    if (process.env.CODEGRAPH_ASCII === '1')
        return false;
    if (process.env.CODEGRAPH_UNICODE === '1')
        return true;
    if (process.platform === 'win32')
        return false;
    return process.env.TERM !== 'linux';
}
exports.UNICODE_GLYPHS = {
    ok: '✓',
    err: '✗',
    info: 'ℹ',
    warn: '⚠',
    spinner: ['·', '✢', '✳', '✶', '✻', '✽'],
    barFilled: '█',
    barEmpty: '░',
    rail: '│',
    phaseDone: '◆',
    dash: '—',
    hLine: '─',
    treeBranch: '├── ',
    treeLast: '└── ',
    treePipe: '│   ',
};
exports.ASCII_GLYPHS = {
    ok: '[OK]',
    err: '[ERR]',
    info: '[i]',
    warn: '[!]',
    spinner: ['.', '*', '+', 'x', 'o', 'O'],
    barFilled: '#',
    barEmpty: '-',
    rail: '|',
    phaseDone: '*',
    dash: '-',
    hLine: '-',
    treeBranch: '|-- ',
    treeLast: '`-- ',
    treePipe: '|   ',
};
let cached = null;
function getGlyphs() {
    if (cached === null) {
        cached = supportsUnicode() ? exports.UNICODE_GLYPHS : exports.ASCII_GLYPHS;
    }
    return cached;
}
/** Reset the cached glyph set. Test-only; production code should call `getGlyphs()`. */
function _resetGlyphsCache() {
    cached = null;
}
//# sourceMappingURL=glyphs.js.map