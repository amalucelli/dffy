; Scopes and the names they define, for what a name used in a function
; names there, in JavaScript and, before its own, TypeScript. Each writes
; its parameters its own way, in a file of its own.

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

(variable_declarator name: (_) @local.definition.var)
(catch_clause parameter: (_) @local.definition.var)
(for_in_statement left: (identifier) @local.definition.var)
(function_declaration name: (identifier) @local.definition.var)
(class_declaration name: (_) @local.definition.var)
