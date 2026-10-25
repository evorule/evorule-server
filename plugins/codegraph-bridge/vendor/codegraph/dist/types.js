"use strict";
/**
 * CodeGraph Type Definitions
 *
 * Core types for the semantic knowledge graph system.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.LANGUAGES = exports.NODE_KINDS = void 0;
// =============================================================================
// Union Types
// =============================================================================
/**
 * Types of nodes in the knowledge graph.
 *
 * Defined as a runtime-iterable `as const` array so the same source
 * of truth backs both the TS type and any runtime validation
 * (e.g. the search query parser).
 */
exports.NODE_KINDS = [
    'file',
    'module',
    'class',
    'struct',
    'interface',
    'trait',
    'protocol',
    'function',
    'method',
    'property',
    'field',
    'variable',
    'constant',
    'enum',
    'enum_member',
    'type_alias',
    'namespace',
    'parameter',
    'import',
    'export',
    'route',
    'component',
];
/**
 * Supported programming languages. See NODE_KINDS for why this is a
 * runtime-iterable const array.
 */
exports.LANGUAGES = [
    'typescript',
    'javascript',
    'tsx',
    'jsx',
    'python',
    'go',
    'rust',
    'java',
    'c',
    'cpp',
    'csharp',
    'php',
    'ruby',
    'swift',
    'kotlin',
    'dart',
    'svelte',
    'vue',
    'liquid',
    'pascal',
    'scala',
    'lua',
    'luau',
    'yaml',
    'twig',
    'unknown',
];
//# sourceMappingURL=types.js.map