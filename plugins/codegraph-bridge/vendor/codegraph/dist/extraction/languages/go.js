"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.goExtractor = void 0;
const tree_sitter_helpers_1 = require("../tree-sitter-helpers");
exports.goExtractor = {
    functionTypes: ['function_declaration'],
    classTypes: [], // Go doesn't have classes
    methodTypes: ['method_declaration'],
    interfaceTypes: [], // Handled via type_spec → resolveTypeAliasKind
    structTypes: [], // Handled via type_spec → resolveTypeAliasKind
    enumTypes: [],
    typeAliasTypes: ['type_spec'], // Go type declarations
    importTypes: ['import_declaration'],
    callTypes: ['call_expression'],
    variableTypes: ['var_declaration', 'short_var_declaration', 'const_declaration'],
    methodsAreTopLevel: true,
    nameField: 'name',
    bodyField: 'body',
    paramsField: 'parameters',
    returnField: 'result',
    getSignature: (node, source) => {
        const params = (0, tree_sitter_helpers_1.getChildByField)(node, 'parameters');
        const result = (0, tree_sitter_helpers_1.getChildByField)(node, 'result');
        if (!params)
            return undefined;
        let sig = (0, tree_sitter_helpers_1.getNodeText)(params, source);
        if (result) {
            sig += ' ' + (0, tree_sitter_helpers_1.getNodeText)(result, source);
        }
        return sig;
    },
    resolveTypeAliasKind: (node, _source) => {
        // Go type_spec: `type Foo struct { ... }` or `type Bar interface { ... }`
        // The inner type is in the 'type' field of the type_spec node
        const typeChild = (0, tree_sitter_helpers_1.getChildByField)(node, 'type');
        if (!typeChild)
            return undefined;
        if (typeChild.type === 'struct_type')
            return 'struct';
        if (typeChild.type === 'interface_type')
            return 'interface';
        return undefined;
    },
    getReceiverType: (node, source) => {
        // Go method_declaration has a "receiver" field: func (sl *scrapeLoop) run(...)
        // The receiver is a parameter_list containing a parameter_declaration
        // with a type that may be a pointer_type (*scrapeLoop) or plain type (scrapeLoop)
        const receiver = (0, tree_sitter_helpers_1.getChildByField)(node, 'receiver');
        if (!receiver)
            return undefined;
        // Find the type identifier inside the receiver
        const text = (0, tree_sitter_helpers_1.getNodeText)(receiver, source);
        // Extract type name from patterns like "(sl *Type)", "(sl Type)", "(*Type)", "(Type)"
        const match = text.match(/\*?\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)/);
        return match?.[1];
    },
};
//# sourceMappingURL=go.js.map