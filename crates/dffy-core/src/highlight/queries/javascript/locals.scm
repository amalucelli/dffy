; Scopes and the names they define, for what a name used in a function
; names there, in JavaScript and, before its own, TypeScript. Each writes
; its parameters its own way, in a file of its own. A `var` is its
; function's, whatever block it's in.

(function_declaration) @local.scope
(generator_function_declaration) @local.scope
(function_expression) @local.scope
(generator_function) @local.scope
(arrow_function) @local.scope
(method_definition) @local.scope
(statement_block) @local.scope
(for_statement) @local.scope
(for_in_statement) @local.scope
(catch_clause) @local.scope

(arrow_function parameter: (identifier) @local.definition.parameter)

(lexical_declaration (variable_declarator name: (_) @local.definition.var))
(variable_declaration (variable_declarator name: (_) @local.definition.hoisted))
(for_in_statement kind: "var" left: (_) @local.definition.hoisted)
(catch_clause parameter: (_) @local.definition.var)
(for_in_statement left: (identifier) @local.definition.var)
(function_declaration name: (identifier) @local.definition.var)
(class_declaration name: (_) @local.definition.var)
