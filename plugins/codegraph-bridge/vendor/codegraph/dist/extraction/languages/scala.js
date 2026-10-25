"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.scalaExtractor = void 0;
const tree_sitter_helpers_1 = require("../tree-sitter-helpers");
function getValVarName(node, source) {
    const patternNode = node.childForFieldName('pattern');
    if (!patternNode)
        return null;
    if (patternNode.type === 'identifier')
        return (0, tree_sitter_helpers_1.getNodeText)(patternNode, source);
    const identChild = patternNode.namedChildren.find((c) => c.type === 'identifier');
    return identChild ? (0, tree_sitter_helpers_1.getNodeText)(identChild, source) : null;
}
function extractVisibility(node) {
    for (let i = 0; i < node.namedChildCount; i++) {
        const child = node.namedChild(i);
        if (!child)
            continue;
        if (child.type === 'modifiers' || child.type === 'access_modifier') {
            const text = child.text;
            if (text.includes('private'))
                return 'private';
            if (text.includes('protected'))
                return 'protected';
        }
    }
    return 'public';
}
exports.scalaExtractor = {
    // top-level function_definition is handled via methodTypes (same pattern as Kotlin)
    functionTypes: [],
    classTypes: ['class_definition', 'object_definition', 'trait_definition'],
    methodTypes: ['function_definition', 'function_declaration'],
    interfaceTypes: [],
    structTypes: [],
    enumTypes: ['enum_definition'],
    enumMemberTypes: [], // handled in visitNode — enum_case_definitions wraps the cases
    typeAliasTypes: ['type_definition'],
    importTypes: ['import_declaration'],
    callTypes: ['call_expression'],
    variableTypes: [], // val/var handled in visitNode (use `pattern` field, not `name`)
    fieldTypes: [],
    extraClassNodeTypes: [],
    nameField: 'name',
    bodyField: 'body',
    paramsField: 'parameters',
    returnField: 'return_type',
    interfaceKind: 'trait',
    classifyClassNode: (node) => {
        if (node.type === 'trait_definition')
            return 'trait';
        return 'class';
    },
    getSignature: (node, source) => {
        const params = node.childForFieldName('parameters');
        const returnType = node.childForFieldName('return_type');
        if (!params && !returnType)
            return undefined;
        let sig = params ? (0, tree_sitter_helpers_1.getNodeText)(params, source) : '';
        if (returnType)
            sig += ': ' + (0, tree_sitter_helpers_1.getNodeText)(returnType, source);
        return sig || undefined;
    },
    getVisibility: (node) => extractVisibility(node),
    isAsync: () => false,
    isStatic: (node) => {
        for (let i = 0; i < node.namedChildCount; i++) {
            const child = node.namedChild(i);
            if (child?.type === 'modifiers' && child.text.includes('static'))
                return true;
        }
        return false;
    },
    visitNode: (node, ctx) => {
        const t = node.type;
        // val/var: name is in `pattern` field (identifier), not `name`
        if (t === 'val_definition' || t === 'var_definition') {
            const name = getValVarName(node, ctx.source);
            if (!name)
                return false;
            const isInClass = ctx.nodeStack.length > 0 &&
                (() => {
                    const parentId = ctx.nodeStack[ctx.nodeStack.length - 1];
                    const parentNode = ctx.nodes.find((n) => n.id === parentId);
                    return parentNode != null && (parentNode.kind === 'class' || parentNode.kind === 'trait' ||
                        parentNode.kind === 'interface' || parentNode.kind === 'struct' ||
                        parentNode.kind === 'enum' || parentNode.kind === 'module');
                })();
            const kind = isInClass ? 'field' : (t === 'val_definition' ? 'constant' : 'variable');
            const typeNode = node.childForFieldName('type');
            const sig = typeNode
                ? `${t === 'val_definition' ? 'val' : 'var'} ${name}: ${(0, tree_sitter_helpers_1.getNodeText)(typeNode, ctx.source)}`
                : undefined;
            ctx.createNode(kind, name, node, { signature: sig, visibility: extractVisibility(node) });
            return true;
        }
        // enum_case_definitions wraps simple_enum_case / full_enum_case children
        if (t === 'enum_case_definitions') {
            for (let i = 0; i < node.namedChildCount; i++) {
                const child = node.namedChild(i);
                if (!child)
                    continue;
                if (child.type === 'simple_enum_case' || child.type === 'full_enum_case') {
                    const nameNode = child.childForFieldName('name');
                    if (nameNode)
                        ctx.createNode('enum_member', (0, tree_sitter_helpers_1.getNodeText)(nameNode, ctx.source), child);
                }
            }
            return true;
        }
        // extension_definition: visit body children directly, no container node
        if (t === 'extension_definition') {
            const body = node.childForFieldName('body');
            if (body) {
                for (let i = 0; i < body.namedChildCount; i++) {
                    const child = body.namedChild(i);
                    if (child)
                        ctx.visitNode(child);
                }
            }
            return true;
        }
        return false;
    },
    extractImport: (node, source) => {
        const importText = (0, tree_sitter_helpers_1.getNodeText)(node, source).trim();
        const pathNode = node.childForFieldName('path');
        if (pathNode)
            return { moduleName: (0, tree_sitter_helpers_1.getNodeText)(pathNode, source), signature: importText };
        for (let i = 0; i < node.namedChildCount; i++) {
            const child = node.namedChild(i);
            if (child?.type === 'identifier' || child?.type === 'stable_identifier') {
                return { moduleName: (0, tree_sitter_helpers_1.getNodeText)(child, source), signature: importText };
            }
        }
        return null;
    },
};
//# sourceMappingURL=scala.js.map