; Scopes and the names they define, for what a name used in a method names
; there. Fields and properties are the index's.

(method_declaration) @local.scope
(constructor_declaration) @local.scope
(local_function_statement) @local.scope
(lambda_expression) @local.scope
(anonymous_method_expression) @local.scope
(block) @local.scope
(for_statement) @local.scope
(foreach_statement) @local.scope
(catch_clause) @local.scope
(using_statement) @local.scope

(parameter name: (identifier) @local.definition.parameter)
(lambda_expression parameters: (implicit_parameter) @local.definition.parameter)

(local_declaration_statement
  (variable_declaration (variable_declarator name: (identifier) @local.definition.var)))
(for_statement
  (variable_declaration (variable_declarator name: (identifier) @local.definition.var)))
(using_statement
  (variable_declaration (variable_declarator name: (identifier) @local.definition.var)))
(foreach_statement left: (identifier) @local.definition.var)
(catch_declaration name: (identifier) @local.definition.var)
(declaration_expression name: (identifier) @local.definition.var)
