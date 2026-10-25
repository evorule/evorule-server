import { ExtractionResult } from '../types';
/**
 * VueExtractor - Extracts code relationships from Vue Single-File Component files
 *
 * Vue SFCs are multi-language (script + template + style). Rather than
 * parsing the full Vue grammar, we extract the <script> block content
 * and delegate it to the TypeScript/JavaScript TreeSitterExtractor.
 *
 * Every .vue file produces a component node (Vue components are always importable).
 */
export declare class VueExtractor {
    private filePath;
    private source;
    private nodes;
    private edges;
    private unresolvedReferences;
    private errors;
    constructor(filePath: string, source: string);
    /**
     * Extract from Vue source
     */
    extract(): ExtractionResult;
    /**
     * Create a component node for the .vue file
     */
    private createComponentNode;
    /**
     * Extract <script> and <script setup> blocks from the Vue source
     */
    private extractScriptBlocks;
    /**
     * Process a script block by delegating to TreeSitterExtractor
     */
    private processScriptBlock;
}
//# sourceMappingURL=vue-extractor.d.ts.map