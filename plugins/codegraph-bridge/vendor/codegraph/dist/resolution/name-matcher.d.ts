/**
 * Name Matcher
 *
 * Handles symbol name matching for reference resolution.
 */
import { UnresolvedRef, ResolvedRef, ResolutionContext } from './types';
/**
 * Try to resolve a path-like reference (e.g., "snippets/drawer-menu.liquid")
 * by matching the filename against file nodes.
 */
export declare function matchByFilePath(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
/**
 * Try to resolve a reference by exact name match
 */
export declare function matchByExactName(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
/**
 * Try to resolve by qualified name
 */
export declare function matchByQualifiedName(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
/**
 * Try to resolve by method name on a class/object
 */
export declare function matchMethodCall(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
/**
 * Fuzzy match - last resort with lower confidence
 */
export declare function matchFuzzy(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
/**
 * Match all strategies in order of confidence
 */
export declare function matchReference(ref: UnresolvedRef, context: ResolutionContext): ResolvedRef | null;
//# sourceMappingURL=name-matcher.d.ts.map