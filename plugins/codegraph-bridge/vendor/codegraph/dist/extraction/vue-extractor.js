"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.VueExtractor = void 0;
const tree_sitter_helpers_1 = require("./tree-sitter-helpers");
const tree_sitter_1 = require("./tree-sitter");
const grammars_1 = require("./grammars");
/**
 * VueExtractor - Extracts code relationships from Vue Single-File Component files
 *
 * Vue SFCs are multi-language (script + template + style). Rather than
 * parsing the full Vue grammar, we extract the <script> block content
 * and delegate it to the TypeScript/JavaScript TreeSitterExtractor.
 *
 * Every .vue file produces a component node (Vue components are always importable).
 */
class VueExtractor {
    filePath;
    source;
    nodes = [];
    edges = [];
    unresolvedReferences = [];
    errors = [];
    constructor(filePath, source) {
        this.filePath = filePath;
        this.source = source;
    }
    /**
     * Extract from Vue source
     */
    extract() {
        const startTime = Date.now();
        try {
            // Create component node for the .vue file itself
            const componentNode = this.createComponentNode();
            // Extract and process script blocks
            const scriptBlocks = this.extractScriptBlocks();
            for (const block of scriptBlocks) {
                this.processScriptBlock(block, componentNode.id);
            }
        }
        catch (error) {
            this.errors.push({
                message: `Vue extraction error: ${error instanceof Error ? error.message : String(error)}`,
                severity: 'error',
            });
        }
        return {
            nodes: this.nodes,
            edges: this.edges,
            unresolvedReferences: this.unresolvedReferences,
            errors: this.errors,
            durationMs: Date.now() - startTime,
        };
    }
    /**
     * Create a component node for the .vue file
     */
    createComponentNode() {
        const lines = this.source.split('\n');
        const fileName = this.filePath.split(/[/\\]/).pop() || this.filePath;
        const componentName = fileName.replace(/\.vue$/, '');
        const id = (0, tree_sitter_helpers_1.generateNodeId)(this.filePath, 'component', componentName, 1);
        const node = {
            id,
            kind: 'component',
            name: componentName,
            qualifiedName: `${this.filePath}::${componentName}`,
            filePath: this.filePath,
            language: 'vue',
            startLine: 1,
            endLine: lines.length,
            startColumn: 0,
            endColumn: lines[lines.length - 1]?.length || 0,
            isExported: true, // Vue components are always importable
            updatedAt: Date.now(),
        };
        this.nodes.push(node);
        return node;
    }
    /**
     * Extract <script> and <script setup> blocks from the Vue source
     */
    extractScriptBlocks() {
        const blocks = [];
        const scriptRegex = /<script(\s[^>]*)?>(?<content>[\s\S]*?)<\/script>/g;
        let match;
        while ((match = scriptRegex.exec(this.source)) !== null) {
            const attrs = match[1] || '';
            const content = match.groups?.content || match[2] || '';
            // Detect TypeScript from lang attribute
            const isTypeScript = /lang\s*=\s*["'](ts|typescript)["']/.test(attrs);
            // Detect <script setup>
            const isSetup = /\bsetup\b/.test(attrs);
            // Calculate start line of the script content (line after <script>)
            const beforeScript = this.source.substring(0, match.index);
            const scriptTagLine = (beforeScript.match(/\n/g) || []).length;
            // The content starts on the line after the opening <script> tag
            const openingTag = match[0].substring(0, match[0].indexOf('>') + 1);
            const openingTagLines = (openingTag.match(/\n/g) || []).length;
            const contentStartLine = scriptTagLine + openingTagLines + 1; // 0-indexed line
            blocks.push({
                content,
                startLine: contentStartLine,
                isSetup,
                isTypeScript,
            });
        }
        return blocks;
    }
    /**
     * Process a script block by delegating to TreeSitterExtractor
     */
    processScriptBlock(block, componentNodeId) {
        const scriptLanguage = block.isTypeScript ? 'typescript' : 'javascript';
        // Check if the script language parser is available
        if (!(0, grammars_1.isLanguageSupported)(scriptLanguage)) {
            this.errors.push({
                message: `Parser for ${scriptLanguage} not available, cannot parse Vue script block`,
                severity: 'warning',
            });
            return;
        }
        // Delegate to TreeSitterExtractor
        const extractor = new tree_sitter_1.TreeSitterExtractor(this.filePath, block.content, scriptLanguage);
        const result = extractor.extract();
        // Offset line numbers from script block back to .vue file positions
        for (const node of result.nodes) {
            node.startLine += block.startLine;
            node.endLine += block.startLine;
            node.language = 'vue'; // Mark as vue, not TS/JS
            this.nodes.push(node);
            // Add containment edge from component to this node
            this.edges.push({
                source: componentNodeId,
                target: node.id,
                kind: 'contains',
            });
        }
        // Offset edges (they reference line numbers)
        for (const edge of result.edges) {
            if (edge.line) {
                edge.line += block.startLine;
            }
            this.edges.push(edge);
        }
        // Offset unresolved references
        for (const ref of result.unresolvedReferences) {
            ref.line += block.startLine;
            ref.filePath = this.filePath;
            ref.language = 'vue';
            this.unresolvedReferences.push(ref);
        }
        // Carry over errors
        for (const error of result.errors) {
            if (error.line) {
                error.line += block.startLine;
            }
            this.errors.push(error);
        }
    }
}
exports.VueExtractor = VueExtractor;
//# sourceMappingURL=vue-extractor.js.map