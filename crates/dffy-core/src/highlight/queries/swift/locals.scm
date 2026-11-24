; Scopes and the names they define, for what a name used in a function
; names there. A type's properties are the index's.

(function_declaration) @local.scope
(lambda_literal) @local.scope
(for_statement) @local.scope
(if_statement) @local.scope
(while_statement) @local.scope
(statements) @local.scope

(parameter name: (simple_identifier) @local.definition.parameter)
(lambda_parameter name: (simple_identifier) @local.definition.parameter)

(property_declaration name: (pattern) @local.definition.var)
(for_statement item: (pattern) @local.definition.var)
(if_statement bound_identifier: (simple_identifier) @local.definition.var)
(guard_statement bound_identifier: (simple_identifier) @local.definition.var)
(while_statement bound_identifier: (simple_identifier) @local.definition.var)
