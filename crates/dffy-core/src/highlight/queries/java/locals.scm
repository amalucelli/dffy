; Scopes and the names they define, for what a name used in a method names
; there. Fields are the index's.

(method_declaration) @local.scope
(constructor_declaration) @local.scope
(lambda_expression) @local.scope
(block) @local.scope
(for_statement) @local.scope
(enhanced_for_statement) @local.scope
(catch_clause) @local.scope
(try_with_resources_statement) @local.scope

(formal_parameter name: (identifier) @local.definition.parameter)
(spread_parameter (variable_declarator name: (identifier) @local.definition.parameter))
(catch_formal_parameter name: (identifier) @local.definition.parameter)
(lambda_expression parameters: (identifier) @local.definition.parameter)
(inferred_parameters (identifier) @local.definition.parameter)

(local_variable_declaration
  declarator: (variable_declarator name: (identifier) @local.definition.var))
(enhanced_for_statement name: (identifier) @local.definition.var)
(resource name: (identifier) @local.definition.var)
