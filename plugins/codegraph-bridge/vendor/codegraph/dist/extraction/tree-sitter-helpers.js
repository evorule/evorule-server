"use strict";
/**
 * Tree-sitter Shared Helpers
 *
 * Utility functions used by the core TreeSitterExtractor and per-language extractors.
 * Extracted to a leaf module to avoid circular imports between tree-sitter.ts and languages/.
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
exports.generateNodeId = generateNodeId;
exports.getNodeText = getNodeText;
exports.getChildByField = getChildByField;
exports.getPrecedingDocstring = getPrecedingDocstring;
const crypto = __importStar(require("crypto"));
/**
 * Generate a unique node ID
 *
 * Uses a 32-character (128-bit) hash to avoid collisions when indexing
 * large codebases with many files containing similar symbols.
 */
function generateNodeId(filePath, kind, name, line) {
    const hash = crypto
        .createHash('sha256')
        .update(`${filePath}:${kind}:${name}:${line}`)
        .digest('hex')
        .substring(0, 32);
    return `${kind}:${hash}`;
}
/**
 * Extract text from a syntax node
 */
function getNodeText(node, source) {
    return source.substring(node.startIndex, node.endIndex);
}
/**
 * Find a child node by field name
 */
function getChildByField(node, fieldName) {
    return node.childForFieldName(fieldName);
}
/**
 * Get the docstring/comment preceding a node
 */
function getPrecedingDocstring(node, source) {
    let sibling = node.previousNamedSibling;
    const comments = [];
    while (sibling) {
        if (sibling.type === 'comment' ||
            sibling.type === 'line_comment' ||
            sibling.type === 'block_comment' ||
            sibling.type === 'documentation_comment') {
            comments.unshift(getNodeText(sibling, source));
            sibling = sibling.previousNamedSibling;
        }
        else {
            break;
        }
    }
    if (comments.length === 0)
        return undefined;
    // Clean up comment markers
    return comments
        .map((c) => c
        .replace(/^\/\*\*?|\*\/$/g, '')
        .replace(/^\/\/\s?/gm, '')
        .replace(/^\s*\*\s?/gm, '')
        .trim())
        .join('\n')
        .trim();
}
//# sourceMappingURL=tree-sitter-helpers.js.map