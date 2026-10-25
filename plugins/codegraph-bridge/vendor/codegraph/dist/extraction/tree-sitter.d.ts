/**
 * Tree-sitter Parser Wrapper
 *
 * Handles parsing source code and extracting structural information.
 */
import { Language, ExtractionResult } from '../types';
export { generateNodeId } from './tree-sitter-helpers';
/**
 * TreeSitterExtractor - Main extraction class
 */
export declare class TreeSitterExtractor {
    private filePath;
    private language;
    private source;
    private tree;
    private nodes;
    private edges;
    private unresolvedReferences;
    private errors;
    private extractor;
    private nodeStack;
    private methodIndex;
    constructor(filePath: string, source: string, language?: Language);
    /**
     * Parse and extract from the source code
     */
    extract(): ExtractionResult;
    /**
     * Visit a node and extract information
     */
    private visitNode;
    /**
     * Create a Node object
     */
    private createNode;
    /**
     * Find first named child whose type is in the given list.
     * Used to locate inner type nodes (e.g. enum_specifier inside a typedef).
     */
    private findChildByTypes;
    /**
     * Build qualified name from node stack
     */
    private buildQualifiedName;
    /**
     * Build an ExtractorContext for passing to language-specific visitNode hooks.
     */
    private makeExtractorContext;
    /**
     * Check if the current node stack indicates we are inside a class-like node
     * (class, struct, interface, trait). File nodes do not count as class-like.
     */
    private isInsideClassLikeNode;
    /**
     * Extract a function
     */
    private extractFunction;
    /**
     * Extract a class
     */
    private extractClass;
    /**
     * Extract a method
     */
    private extractMethod;
    /**
     * Extract an interface/protocol/trait
     */
    private extractInterface;
    /**
     * Extract a struct
     */
    private extractStruct;
    /**
     * Extract an enum
     */
    private extractEnum;
    /**
     * Extract enum member names from an enum member node.
     * Handles multi-case declarations (Swift: `case put, delete`) and single-case patterns.
     */
    private extractEnumMembers;
    /**
     * Extract a class property declaration (e.g. C# `public string Name { get; set; }`).
     * Extracts as 'property' kind node inside the owning class.
     */
    private extractProperty;
    /**
     * Extract a class field declaration (e.g. Java field_declaration, C# field_declaration).
     * Extracts each declarator as a 'field' kind node inside the owning class.
     */
    private extractField;
    /**
     * Extract a variable declaration (const, let, var, etc.)
     *
     * Extracts top-level and module-level variable declarations.
     * Captures the variable name and first 100 chars of initializer in signature for searchability.
     */
    private extractVariable;
    /**
     * Extract a type alias (e.g. `export type X = ...` in TypeScript).
     * For languages like Go, resolveTypeAliasKind detects when the type_spec
     * wraps a struct or interface definition and creates the correct node kind.
     * Returns true if children should be skipped (struct/interface handled body visiting).
     */
    private extractTypeAlias;
    /**
     * Extract an import
     *
     * Creates an import node with the full import statement stored in signature for searchability.
     * Also creates unresolved references for resolution purposes.
     */
    private extractImport;
    /**
     * Extract a function call
     */
    private extractCall;
    /**
     * `new Foo(...)` / `Foo::new(...)` / object_creation_expression —
     * emit an `instantiates` reference to the class name. The resolver
     * then links it to the class node, producing the `instantiates`
     * edge that powers "what creates instances of X" queries.
     *
     * Children are still walked so nested calls inside the constructor
     * arguments (`new Foo(bar())`) get their own `calls` references.
     */
    private extractInstantiation;
    /**
     * Scan `declNode` and its preceding siblings (within the parent's
     * named children) for decorator nodes, emitting a `decorates`
     * reference from `decoratedId` to each decorator's function name.
     *
     * Why preceding siblings: in TypeScript, `@Foo class Bar {}` parses
     * as an `export_statement` (or top-level wrapper) with the
     * `decorator` as a child *before* the `class_declaration` — so the
     * decorator isn't a child of the class itself. For methods/
     * properties, the decorator IS a direct child of the declaration,
     * so we also scan declNode.namedChildren.
     *
     * Idempotent across grammars: if neither location yields decorators
     * (most non-decorator-using languages), the function is a no-op.
     */
    private extractDecoratorsFor;
    /**
     * Visit function body and extract calls (and structural nodes).
     *
     * In addition to call expressions, this also detects class/struct/enum
     * definitions inside function bodies. This handles two cases:
     *   1. Local class/struct/enum definitions (valid in C++, Java, etc.)
     *   2. C++ macro misparsing — macros like NLOHMANN_JSON_NAMESPACE_BEGIN cause
     *      tree-sitter to interpret the namespace block as a function_definition,
     *      hiding real class/struct/enum nodes inside the "function body".
     */
    private visitFunctionBody;
    /**
     * Extract inheritance relationships
     */
    private extractInheritance;
    /**
     * Rust `impl Trait for Type` — creates an implements edge from Type to Trait.
     * For plain `impl Type { ... }` (no trait), no inheritance edge is needed.
     */
    private extractRustImplItem;
    /**
     * Find a previously-extracted node by name (used for back-references like impl blocks)
     */
    private findNodeByName;
    /**
     * Languages that support type annotations (TypeScript, etc.)
     */
    private readonly TYPE_ANNOTATION_LANGUAGES;
    /**
     * Built-in/primitive type names that shouldn't create references
     */
    private readonly BUILTIN_TYPES;
    /**
     * Extract type references from type annotations on a function/method/field node.
     * Creates 'references' edges for parameter types, return types, and field types.
     */
    private extractTypeAnnotations;
    /**
     * Extract type references from a variable's type annotation.
     */
    private extractVariableTypeAnnotation;
    /**
     * Recursively walk a subtree and extract all type_identifier references.
     * Handles unions, intersections, generics, arrays, etc.
     */
    private extractTypeRefsFromSubtree;
    /**
     * Handle Pascal-specific AST structures.
     * Returns true if the node was fully handled and children should be skipped.
     */
    private visitPascalNode;
    /**
     * Extract a Pascal declType node (class, interface, enum, or type alias)
     */
    private extractPascalDeclType;
    /**
     * Extract Pascal uses clause into individual import nodes
     */
    private extractPascalUses;
    /**
     * Extract a Pascal constant declaration
     */
    private extractPascalConst;
    /**
     * Extract Pascal inheritance (extends/implements) from declClass typeref children
     */
    private extractPascalInheritance;
    /**
     * Extract calls and resolve method context from a Pascal defProc (implementation body).
     * Does not create a new node — the declaration was already captured from the interface section.
     */
    private extractPascalDefProc;
    /**
     * Extract function calls from a Pascal expression
     */
    private extractPascalCall;
    /**
     * Recursively visit a Pascal block/statement tree for call expressions
     */
    private visitPascalBlock;
}
/**
 * Extract nodes and edges from source code.
 *
 * If `frameworkNames` is provided, framework-specific extractors matching
 * those names and the file's language are run after the tree-sitter pass.
 * Their nodes/references/errors are merged into the returned result.
 */
export declare function extractFromSource(filePath: string, source: string, language?: Language, frameworkNames?: string[]): ExtractionResult;
//# sourceMappingURL=tree-sitter.d.ts.map