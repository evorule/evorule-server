"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.csharpExtractor = void 0;
const tree_sitter_helpers_1 = require("../tree-sitter-helpers");
exports.csharpExtractor = {
    functionTypes: [],
    classTypes: ['class_declaration'],
    methodTypes: ['method_declaration', 'constructor_declaration'],
    interfaceTypes: ['interface_declaration'],
    structTypes: ['struct_declaration'],
    enumTypes: ['enum_declaration'],
    enumMemberTypes: ['enum_member_declaration'],
    typeAliasTypes: [],
    importTypes: ['using_directive'],
    callTypes: ['invocation_expression'],
    variableTypes: ['local_declaration_statement'],
    fieldTypes: ['field_declaration'],
    propertyTypes: ['property_declaration'],
    nameField: 'name',
    bodyField: 'body',
    paramsField: 'parameter_list',
    getVisibility: (node) => {
        for (let i = 0; i < node.childCount; i++) {
            const child = node.child(i);
            if (child?.type === 'modifier') {
                const text = child.text;
                if (text === 'public')
                    return 'public';
                if (text === 'private')
                    return 'private';
                if (text === 'protected')
                    return 'protected';
                if (text === 'internal')
                    return 'internal';
            }
        }
        return 'private'; // C# defaults to private
    },
    isStatic: (node) => {
        for (let i = 0; i < node.childCount; i++) {
            const child = node.child(i);
            if (child?.type === 'modifier' && child.text === 'static') {
                return true;
            }
        }
        return false;
    },
    isAsync: (node) => {
        for (let i = 0; i < node.childCount; i++) {
            const child = node.child(i);
            if (child?.type === 'modifier' && child.text === 'async') {
                return true;
            }
        }
        return false;
    },
    extractImport: (node, source) => {
        const importText = source.substring(node.startIndex, node.endIndex).trim();
        // C# using directives: using System, using System.Collections.Generic, using static X, using Alias = X
        const qualifiedName = node.namedChildren.find((c) => c.type === 'qualified_name');
        if (qualifiedName) {
            return { moduleName: (0, tree_sitter_helpers_1.getNodeText)(qualifiedName, source), signature: importText };
        }
        // Simple namespace like "using System;" - get the first identifier
        const identifier = node.namedChildren.find((c) => c.type === 'identifier');
        if (identifier) {
            return { moduleName: (0, tree_sitter_helpers_1.getNodeText)(identifier, source), signature: importText };
        }
        return null;
    },
};
//# sourceMappingURL=csharp.js.map