/**
 * Import Resolver
 *
 * Resolves import paths to actual files and symbols.
 */
import { Language } from '../types';
import { UnresolvedRef, ResolvedRef, ResolutionContext, ImportMapping, ReExport } from './types';
/**
 * Resolve an import path to an actual file
 */
export declare function resolveImportPath(importPath: string, fromFile: string, language: Language, context: ResolutionContext): string | null;
/**
 * Extract import mappings from a file
 */
export declare function extractImportMappings(_filePath: string, content: string, language: Language): ImportMapping[];
/**
 * Clear the import mapping cache (call between indexing runs)
 */
export declare function clearImportMappingCache(): void;
/**
 * Extract JS/TS re-export declarations from `content`.
 *
 * Recognised forms:
 *   export { foo } from './a';
 *   export { foo as bar } from './a';
 *   export * from './a';
 *   export * as ns from './a';   (treated as wildcard for chasing)
 *   export { default as Foo } from './a';
 *
 * The walker intentionally stays regex-based — the import-resolver
 * elsewhere in this file already chooses regex over a fresh
 * tree-sitter pass, and this function shares that trade-off. Errors
 * fall through silently; resolution simply skips the broken file.
 */
export declare function extractReExports(content: string, language: Language): ReExport[];
/**
 * Resolve a reference using import mappings
 */
export declare function resolveViaImport(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
//# sourceMappingURL=import-resolver.d.ts.map