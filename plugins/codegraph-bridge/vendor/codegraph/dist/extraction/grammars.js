"use strict";
/**
 * Grammar Loading and Caching
 *
 * Uses web-tree-sitter (WASM) for universal cross-platform support.
 * Grammars are loaded lazily — only languages actually present in the project
 * are compiled, keeping V8 WASM memory pressure low on large codebases.
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
exports.EXTENSION_MAP = void 0;
exports.isSourceFile = isSourceFile;
exports.initGrammars = initGrammars;
exports.loadGrammarsForLanguages = loadGrammarsForLanguages;
exports.loadAllGrammars = loadAllGrammars;
exports.isGrammarsInitialized = isGrammarsInitialized;
exports.getParser = getParser;
exports.detectLanguage = detectLanguage;
exports.isLanguageSupported = isLanguageSupported;
exports.isGrammarLoaded = isGrammarLoaded;
exports.getSupportedLanguages = getSupportedLanguages;
exports.resetParser = resetParser;
exports.clearParserCache = clearParserCache;
exports.getUnavailableGrammarErrors = getUnavailableGrammarErrors;
exports.getLanguageDisplayName = getLanguageDisplayName;
const path = __importStar(require("path"));
const web_tree_sitter_1 = require("web-tree-sitter");
/**
 * WASM filename map — maps each language to its .wasm grammar file
 * in the tree-sitter-wasms package.
 */
const WASM_GRAMMAR_FILES = {
    typescript: 'tree-sitter-typescript.wasm',
    tsx: 'tree-sitter-tsx.wasm',
    javascript: 'tree-sitter-javascript.wasm',
    jsx: 'tree-sitter-javascript.wasm',
    python: 'tree-sitter-python.wasm',
    go: 'tree-sitter-go.wasm',
    rust: 'tree-sitter-rust.wasm',
    java: 'tree-sitter-java.wasm',
    c: 'tree-sitter-c.wasm',
    cpp: 'tree-sitter-cpp.wasm',
    csharp: 'tree-sitter-c_sharp.wasm',
    php: 'tree-sitter-php.wasm',
    ruby: 'tree-sitter-ruby.wasm',
    swift: 'tree-sitter-swift.wasm',
    kotlin: 'tree-sitter-kotlin.wasm',
    dart: 'tree-sitter-dart.wasm',
    pascal: 'tree-sitter-pascal.wasm',
    scala: 'tree-sitter-scala.wasm',
    lua: 'tree-sitter-lua.wasm',
    luau: 'tree-sitter-luau.wasm',
};
/**
 * File extension to Language mapping
 */
exports.EXTENSION_MAP = {
    '.ts': 'typescript',
    '.tsx': 'tsx',
    '.js': 'javascript',
    '.mjs': 'javascript',
    '.cjs': 'javascript',
    '.jsx': 'jsx',
    '.py': 'python',
    '.pyw': 'python',
    '.go': 'go',
    '.rs': 'rust',
    '.java': 'java',
    '.c': 'c',
    '.h': 'c', // Could also be C++, defaulting to C
    '.cpp': 'cpp',
    '.cc': 'cpp',
    '.cxx': 'cpp',
    '.hpp': 'cpp',
    '.hxx': 'cpp',
    '.cs': 'csharp',
    '.php': 'php',
    // Drupal-specific PHP file extensions
    '.module': 'php',
    '.install': 'php',
    '.theme': 'php',
    '.inc': 'php',
    // YAML (used for Drupal routing files; no symbol extraction, file-level tracking only)
    '.yml': 'yaml',
    '.yaml': 'yaml',
    // Twig templates (file-level tracking only, no symbol extraction)
    '.twig': 'twig',
    '.rb': 'ruby',
    '.rake': 'ruby',
    '.swift': 'swift',
    '.kt': 'kotlin',
    '.kts': 'kotlin',
    '.dart': 'dart',
    '.liquid': 'liquid',
    '.svelte': 'svelte',
    '.vue': 'vue',
    '.pas': 'pascal',
    '.dpr': 'pascal',
    '.dpk': 'pascal',
    '.lpr': 'pascal',
    '.dfm': 'pascal',
    '.fmx': 'pascal',
    '.scala': 'scala',
    '.sc': 'scala',
    '.lua': 'lua',
    '.luau': 'luau',
};
/**
 * Whether a file is one CodeGraph can parse, based purely on its extension.
 * This is the single source of truth for "should we index this file" — derived
 * from EXTENSION_MAP so parser support and indexing selection never drift.
 */
function isSourceFile(filePath) {
    const dot = filePath.lastIndexOf('.');
    if (dot < 0)
        return false;
    return filePath.slice(dot).toLowerCase() in exports.EXTENSION_MAP;
}
/**
 * Caches for loaded grammars and parsers
 */
const parserCache = new Map();
const languageCache = new Map();
const unavailableGrammarErrors = new Map();
let parserInitialized = false;
/**
 * Initialize the tree-sitter WASM runtime. Must be called before loading grammars.
 * Does NOT load any grammar WASM files — use loadGrammarsForLanguages() for that.
 * Idempotent — safe to call multiple times.
 */
async function initGrammars() {
    if (parserInitialized)
        return;
    await web_tree_sitter_1.Parser.init();
    parserInitialized = true;
}
/**
 * Load grammar WASM files for specific languages only.
 * Skips languages that are already loaded or have no WASM grammar.
 * Must be called after initGrammars().
 */
async function loadGrammarsForLanguages(languages) {
    if (!parserInitialized) {
        await initGrammars();
    }
    // Deduplicate and filter to languages that have WASM grammars and aren't already loaded
    const toLoad = [...new Set(languages)].filter((lang) => lang in WASM_GRAMMAR_FILES &&
        !languageCache.has(lang) &&
        !unavailableGrammarErrors.has(lang));
    // Load grammars sequentially to avoid web-tree-sitter WASM race condition on Node 20+
    // See: https://github.com/tree-sitter/tree-sitter/issues/2338
    for (const lang of toLoad) {
        const wasmFile = WASM_GRAMMAR_FILES[lang];
        try {
            // Some grammars ship their own WASMs (not in tree-sitter-wasms, or the
            // tree-sitter-wasms build is too old). Lua: tree-sitter-wasms ships an
            // ABI-13 build that corrupts the shared WASM heap under web-tree-sitter
            // 0.25 (drops nested calls/imports on every file after the first); we
            // vendor the upstream ABI-15 wasm instead.
            const wasmPath = (lang === 'pascal' || lang === 'scala' || lang === 'lua' || lang === 'luau')
                ? path.join(__dirname, 'wasm', wasmFile)
                : require.resolve(`tree-sitter-wasms/out/${wasmFile}`);
            const language = await web_tree_sitter_1.Language.load(wasmPath);
            languageCache.set(lang, language);
        }
        catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            console.warn(`[CodeGraph] Failed to load ${lang} grammar — parsing will be unavailable: ${message}`);
            unavailableGrammarErrors.set(lang, message);
        }
    }
}
/**
 * Load ALL grammar WASM files. Convenience function for tests and
 * backward compatibility. Prefer loadGrammarsForLanguages() in production.
 */
async function loadAllGrammars() {
    const allLanguages = Object.keys(WASM_GRAMMAR_FILES);
    await loadGrammarsForLanguages(allLanguages);
}
/**
 * Check if grammars have been initialized
 */
function isGrammarsInitialized() {
    return parserInitialized;
}
/**
 * Get a parser for the specified language.
 * Returns synchronously from pre-loaded cache.
 */
function getParser(language) {
    if (parserCache.has(language)) {
        return parserCache.get(language);
    }
    const lang = languageCache.get(language);
    if (!lang) {
        return null;
    }
    const parser = new web_tree_sitter_1.Parser();
    parser.setLanguage(lang);
    parserCache.set(language, parser);
    return parser;
}
/**
 * Detect language from file extension
 */
function detectLanguage(filePath, source) {
    const ext = filePath.substring(filePath.lastIndexOf('.')).toLowerCase();
    const lang = exports.EXTENSION_MAP[ext] || 'unknown';
    // .h files could be C or C++ — check source content for C++ features
    if (lang === 'c' && ext === '.h' && source) {
        if (looksLikeCpp(source))
            return 'cpp';
    }
    return lang;
}
/**
 * Heuristic: does a .h file contain C++ constructs?
 * Checks the first ~8KB for patterns that are unique to C++ and never valid C.
 */
function looksLikeCpp(source) {
    const sample = source.substring(0, 8192);
    return /\bnamespace\b|\bclass\s+\w+\s*[:{]|\btemplate\s*<|\b(?:public|private|protected)\s*:|\bvirtual\b|\busing\s+(?:namespace\b|\w+\s*=)/.test(sample);
}
/**
 * Check if a language is supported (has a grammar defined).
 * Returns true if the grammar exists, even if not yet loaded.
 */
function isLanguageSupported(language) {
    if (language === 'svelte')
        return true; // custom extractor (script block delegation)
    if (language === 'vue')
        return true; // custom extractor (script block delegation)
    if (language === 'liquid')
        return true; // custom regex extractor
    if (language === 'yaml')
        return true; // file-level tracking only; Drupal routing extraction via framework resolver
    if (language === 'twig')
        return true; // file-level tracking only
    if (language === 'unknown')
        return false;
    return language in WASM_GRAMMAR_FILES;
}
/**
 * Check if a grammar has been loaded and is ready for parsing.
 */
function isGrammarLoaded(language) {
    if (language === 'svelte' || language === 'vue' || language === 'liquid')
        return true;
    if (language === 'yaml' || language === 'twig')
        return true; // no WASM grammar needed
    return languageCache.has(language);
}
/**
 * Get all supported languages (those with grammar definitions).
 */
function getSupportedLanguages() {
    return [...Object.keys(WASM_GRAMMAR_FILES), 'svelte', 'vue', 'liquid'];
}
/**
 * Reset the cached parser for a language to reclaim WASM heap memory.
 * The tree-sitter WASM runtime accumulates fragmented memory over thousands
 * of parses. Deleting and recreating the Parser instance forces the WASM
 * heap to reset, preventing "memory access out of bounds" crashes in
 * large repos.
 */
function resetParser(language) {
    const old = parserCache.get(language);
    if (old) {
        old.delete();
        parserCache.delete(language);
    }
}
/**
 * Clear parser/grammar caches (useful for testing)
 */
function clearParserCache() {
    for (const parser of parserCache.values()) {
        parser.delete();
    }
    parserCache.clear();
    // Note: languageCache is NOT cleared — WASM languages persist.
    // To fully re-init, set parserInitialized = false and call initGrammars() again.
    unavailableGrammarErrors.clear();
}
/**
 * Report grammars that failed to load.
 */
function getUnavailableGrammarErrors() {
    const out = {};
    for (const [language, message] of unavailableGrammarErrors.entries()) {
        out[language] = message;
    }
    return out;
}
/**
 * Get language display name
 */
function getLanguageDisplayName(language) {
    const names = {
        typescript: 'TypeScript',
        javascript: 'JavaScript',
        tsx: 'TypeScript (TSX)',
        jsx: 'JavaScript (JSX)',
        python: 'Python',
        go: 'Go',
        rust: 'Rust',
        java: 'Java',
        c: 'C',
        cpp: 'C++',
        csharp: 'C#',
        php: 'PHP',
        ruby: 'Ruby',
        swift: 'Swift',
        kotlin: 'Kotlin',
        dart: 'Dart',
        svelte: 'Svelte',
        vue: 'Vue',
        liquid: 'Liquid',
        pascal: 'Pascal / Delphi',
        scala: 'Scala',
        lua: 'Lua',
        luau: 'Luau',
        yaml: 'YAML',
        twig: 'Twig',
        unknown: 'Unknown',
    };
    return names[language] || language;
}
//# sourceMappingURL=grammars.js.map