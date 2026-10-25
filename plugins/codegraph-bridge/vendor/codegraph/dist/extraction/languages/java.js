"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.javaExtractor = void 0;
const tree_sitter_helpers_1 = require("../tree-sitter-helpers");
exports.javaExtractor = {
    functionTypes: [],
    classTypes: ['class_declaration'],
    methodTypes: ['method_declaration', 'constructor_declaration'],
    interfaceTypes: ['interface_declaration'],
    structTypes: [],
    enumTypes: ['enum_declaration'],
    enumMemberTypes: ['enum_constant'],
    typeAliasTypes: [],
    importTypes: ['import_declaration'],
    callTypes: ['method_invocation'],
    variableTypes: ['local_variable_declaration'],
    fieldTypes: ['field_declaration'],
    nameField: 'name',
    bodyField: 'body',
    paramsField: 'parameters',
    returnField: 'type',
    getSignature: (node, source) => {
        const params = (0, tree_sitter_helpers_1.getChildByField)(node, 'parameters');
        const returnType = (0, tree_sitter_helpers_1.getChildByField)(node, 'type');
        if (!params)
            return undefined;
        const paramsText = (0, tree_sitter_helpers_1.getNodeText)(params, source);
        return returnType ? (0, tree_sitter_helpers_1.getNodeText)(returnType, source) + ' ' + paramsText : paramsText;
    },
    getVisibility: (node) => {
        for (let i = 0; i < node.childCount; i++) {
            const child = node.child(i);
            if (child?.type === 'modifiers') {
                const text = child.text;
                if (text.includes('public'))
                    return 'public';
                if (text.includes('private'))
                    return 'private';
                if (text.includes('protected'))
                    return 'protected';
            }
        }
        return undefined;
    },
    isStatic: (node) => {
        for (let i = 0; i < node.childCount; i++) {
            const child = node.child(i);
            if (child?.type === 'modifiers' && child.text.includes('static')) {
                return true;
            }
        }
        return false;
    },
    extractImport: (node, source) => {
        const importText = source.substring(node.startIndex, node.endIndex).trim();
        const scopedId = node.namedChildren.find((c) => c.type === 'scoped_identifier');
        if (scopedId) {
            const moduleName = source.substring(scopedId.startIndex, scopedId.endIndex);
            return { moduleName, signature: importText };
        }
        return null;
    },
};
//# sourceMappingURL=java.js.map