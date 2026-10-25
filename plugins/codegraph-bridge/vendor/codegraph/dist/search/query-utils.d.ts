/**
 * Search Query Utilities
 *
 * Shared module for search term extraction and scoring.
 */
import { Node } from '../types';
/**
 * Common stop words to filter from search queries.
 * Includes generic English + code-specific noise words.
 */
export declare const STOP_WORDS: Set<string>;
/**
 * Generate stem variants of a search term by removing common English suffixes.
 * Used for FTS query expansion so "caching" also finds "cache", "eviction" finds "evict", etc.
 * Stems are used as PREFIX matches in FTS, so they don't need to be perfect English words.
 */
export declare function getStemVariants(term: string): string[];
/**
 * Extract meaningful search terms from a natural language query.
 * Splits camelCase, PascalCase, snake_case, SCREAMING_SNAKE, and dot.notation
 * into individual tokens before filtering.
 *
 * Preserves original compound identifiers (e.g., "scrapeLoop") alongside
 * their split parts so that FTS can match both the full symbol name and
 * individual words within it.
 *
 * Also generates stem variants (e.g., "caching"→"cache", "eviction"→"evict")
 * so FTS prefix matching can find related code symbols.
 */
export declare function extractSearchTerms(query: string, options?: {
    stems?: boolean;
}): string[];
/**
 * Score path relevance to a query
 * Higher score = more relevant path
 */
export declare function scorePathRelevance(filePath: string, query: string): number;
/**
 * Check if a file path looks like a test file
 */
export declare function isTestFile(filePath: string): boolean;
/**
 * Bonus when a node's name matches the search query.
 * Exact matches get the largest boost; prefix matches get smaller boosts.
 * Multi-word queries also check individual term matches against the name.
 */
export declare function nameMatchBonus(nodeName: string, query: string): number;
/**
 * Kind-based bonus for search ranking
 * Functions and classes are typically more relevant than variables/imports
 */
export declare function kindBonus(kind: Node['kind']): number;
//# sourceMappingURL=query-utils.d.ts.map