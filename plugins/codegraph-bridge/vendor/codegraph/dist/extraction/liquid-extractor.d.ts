import { ExtractionResult } from '../types';
/**
 * LiquidExtractor - Extracts relationships from Liquid template files
 *
 * Liquid is a templating language (used by Shopify, Jekyll, etc.) that doesn't
 * have traditional functions or classes. Instead, we extract:
 * - Section references ({% section 'name' %})
 * - Snippet references ({% render 'name' %} and {% include 'name' %})
 * - Schema blocks ({% schema %}...{% endschema %})
 */
export declare class LiquidExtractor {
    private filePath;
    private source;
    private nodes;
    private edges;
    private unresolvedReferences;
    private errors;
    constructor(filePath: string, source: string);
    /**
     * Extract from Liquid source
     */
    extract(): ExtractionResult;
    /**
     * Create a file node for the Liquid template
     */
    private createFileNode;
    /**
     * Extract {% render 'snippet' %} and {% include 'snippet' %} references
     */
    private extractSnippetReferences;
    /**
     * Extract {% section 'name' %} references
     */
    private extractSectionReferences;
    /**
     * Extract {% schema %}...{% endschema %} blocks
     */
    private extractSchema;
    /**
     * Extract {% assign var = value %} statements
     */
    private extractAssignments;
    /**
     * Get the line number for a character index
     */
    private getLineNumber;
    /**
     * Get the character index of the start of a line
     */
    private getLineStart;
}
//# sourceMappingURL=liquid-extractor.d.ts.map