/**
 * Grammar Loading and Caching
 *
 * Uses web-tree-sitter (WASM) for universal cross-platform support.
 * Grammars are loaded lazily — only languages actually present in the project
 * are compiled, keeping V8 WASM memory pressure low on large codebases.
 */
import { Parser } from 'web-tree-sitter';
import { Language } from '../types';
export type GrammarLanguage = Exclude<Language, 'svelte' | 'vue' | 'liquid' | 'yaml' | 'twig' | 'unknown'>;
/**
 * File extension to Language mapping
 */
export declare const EXTENSION_MAP: Record<string, Language>;
/**
 * Whether a file is one CodeGraph can parse, based purely on its extension.
 * This is the single source of truth for "should we index this file" — derived
 * from EXTENSION_MAP so parser support and indexing selection never drift.
 */
export declare function isSourceFile(filePath: string): boolean;
/**
 * Initialize the tree-sitter WASM runtime. Must be called before loading grammars.
 * Does NOT load any grammar WASM files — use loadGrammarsForLanguages() for that.
 * Idempotent — safe to call multiple times.
 */
export declare function initGrammars(): Promise<void>;
/**
 * Load grammar WASM files for specific languages only.
 * Skips languages that are already loaded or have no WASM grammar.
 * Must be called after initGrammars().
 */
export declare function loadGrammarsForLanguages(languages: Language[]): Promise<void>;
/**
 * Load ALL grammar WASM files. Convenience function for tests and
 * backward compatibility. Prefer loadGrammarsForLanguages() in production.
 */
export declare function loadAllGrammars(): Promise<void>;
/**
 * Check if grammars have been initialized
 */
export declare function isGrammarsInitialized(): boolean;
/**
 * Get a parser for the specified language.
 * Returns synchronously from pre-loaded cache.
 */
export declare function getParser(language: Language): Parser | null;
/**
 * Detect language from file extension
 */
export declare function detectLanguage(filePath: string, source?: string): Language;
/**
 * Check if a language is supported (has a grammar defined).
 * Returns true if the grammar exists, even if not yet loaded.
 */
export declare function isLanguageSupported(language: Language): boolean;
/**
 * Check if a grammar has been loaded and is ready for parsing.
 */
export declare function isGrammarLoaded(language: Language): boolean;
/**
 * Get all supported languages (those with grammar definitions).
 */
export declare function getSupportedLanguages(): Language[];
/**
 * Reset the cached parser for a language to reclaim WASM heap memory.
 * The tree-sitter WASM runtime accumulates fragmented memory over thousands
 * of parses. Deleting and recreating the Parser instance forces the WASM
 * heap to reset, preventing "memory access out of bounds" crashes in
 * large repos.
 */
export declare function resetParser(language: Language): void;
/**
 * Clear parser/grammar caches (useful for testing)
 */
export declare function clearParserCache(): void;
/**
 * Report grammars that failed to load.
 */
export declare function getUnavailableGrammarErrors(): Partial<Record<Language, string>>;
/**
 * Get language display name
 */
export declare function getLanguageDisplayName(language: Language): string;
//# sourceMappingURL=grammars.d.ts.map