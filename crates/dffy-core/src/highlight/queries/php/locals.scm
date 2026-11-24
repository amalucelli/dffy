; Scopes and the names they define, for what a `$name` used in a function
; names there. Blocks make no scope of their own.

(function_definition) @local.scope
(method_declaration) @local.scope
(anonymous_function) @local.scope
(arrow_function) @local.scope

(simple_parameter name: (variable_name) @local.definition.parameter)
(variadic_parameter name: (variable_name) @local.definition.parameter)
(property_promotion_parameter name: (variable_name) @local.definition.parameter)
(anonymous_function_use_clause (variable_name) @local.definition.parameter)

(assignment_expression left: (variable_name) @local.definition.var)
(foreach_statement (_) . (variable_name) @local.definition.var)
(foreach_statement (pair (variable_name) @local.definition.var))
(catch_clause name: (variable_name) @local.definition.var)
