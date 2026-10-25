"use strict";
/**
 * Per-language extraction configurations.
 *
 * Each file exports a LanguageExtractor config object.
 * This barrel builds the EXTRACTORS map consumed by TreeSitterExtractor.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.EXTRACTORS = void 0;
const typescript_1 = require("./typescript");
const javascript_1 = require("./javascript");
const python_1 = require("./python");
const go_1 = require("./go");
const rust_1 = require("./rust");
const java_1 = require("./java");
const c_cpp_1 = require("./c-cpp");
const csharp_1 = require("./csharp");
const php_1 = require("./php");
const ruby_1 = require("./ruby");
const swift_1 = require("./swift");
const kotlin_1 = require("./kotlin");
const dart_1 = require("./dart");
const pascal_1 = require("./pascal");
const scala_1 = require("./scala");
const lua_1 = require("./lua");
const luau_1 = require("./luau");
exports.EXTRACTORS = {
    typescript: typescript_1.typescriptExtractor,
    tsx: typescript_1.typescriptExtractor,
    javascript: javascript_1.javascriptExtractor,
    jsx: javascript_1.javascriptExtractor,
    python: python_1.pythonExtractor,
    go: go_1.goExtractor,
    rust: rust_1.rustExtractor,
    java: java_1.javaExtractor,
    c: c_cpp_1.cExtractor,
    cpp: c_cpp_1.cppExtractor,
    csharp: csharp_1.csharpExtractor,
    php: php_1.phpExtractor,
    ruby: ruby_1.rubyExtractor,
    swift: swift_1.swiftExtractor,
    kotlin: kotlin_1.kotlinExtractor,
    dart: dart_1.dartExtractor,
    pascal: pascal_1.pascalExtractor,
    scala: scala_1.scalaExtractor,
    lua: lua_1.luaExtractor,
    luau: luau_1.luauExtractor,
};
//# sourceMappingURL=index.js.map