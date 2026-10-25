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
export declare function supportsUnicode(): boolean;
export interface Glyphs {
    ok: string;
    err: string;
    info: string;
    warn: string;
    spinner: string[];
    barFilled: string;
    barEmpty: string;
    rail: string;
    phaseDone: string;
    dash: string;
    hLine: string;
    treeBranch: string;
    treeLast: string;
    treePipe: string;
}
export declare const UNICODE_GLYPHS: Glyphs;
export declare const ASCII_GLYPHS: Glyphs;
export declare function getGlyphs(): Glyphs;
/** Reset the cached glyph set. Test-only; production code should call `getGlyphs()`. */
export declare function _resetGlyphsCache(): void;
//# sourceMappingURL=glyphs.d.ts.map