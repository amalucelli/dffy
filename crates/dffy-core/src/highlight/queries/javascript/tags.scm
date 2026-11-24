; Definitions outside function bodies, for JavaScript and, before its own
; patterns, TypeScript. A class's methods are found from the class, with
; it as their @container; a constructor isn't one to look up by name, and a
; `#private` method can't be named from outside. A variable counts at
; module scope only: a `const` holding a function is that function, any
; other `const` a constant. Functions assigned to `exports.name` or
; `module.exports.name` are the module's functions.

(class_declaration
  name: (_) @container
  body: (class_body
    (method_definition
      name: (property_identifier) @name
      (#not-eq? @name "constructor")) @definition.method))

(class_declaration name: (_) @name) @definition.class

(function_declaration name: (identifier) @name) @definition.function

(generator_function_declaration name: (identifier) @name) @definition.function

(program
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)])) @definition.function)

(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator
        name: (identifier) @name
        value: [(arrow_function) (function_expression)])) @definition.function))

(program
  (lexical_declaration
    kind: "const"
    (variable_declarator name: (identifier) @name)) @definition.constant)

(program
  (export_statement
    declaration: (lexical_declaration
      kind: "const"
      (variable_declarator name: (identifier) @name)) @definition.constant))

(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name)) @definition.variable)

(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name)) @definition.variable))

(program
  (variable_declaration
    (variable_declarator name: (identifier) @name)) @definition.variable)

(program
  (expression_statement
    (assignment_expression
      left: (member_expression
        object: [
          (identifier) @exports
          (member_expression property: (property_identifier) @exports)
        ]
        property: (property_identifier) @name)
      right: [(arrow_function) (function_expression)])) @definition.function
  (#eq? @exports "exports"))

; `export { a, b as c } from "./x"` re-exports `a`, and `c` with `b` as
; its @original name.

(export_statement
  (export_clause [
    (export_specifier name: (_) @name !alias)
    (export_specifier name: (_) @original alias: (_) @name)
  ])
  source: (string)) @definition.reexport
