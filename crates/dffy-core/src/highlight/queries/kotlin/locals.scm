; Scopes and the names they define, for what a name used in a function
; names there. A class's properties are the index's.

(function_declaration) @local.scope
(anonymous_function) @local.scope
(lambda_literal) @local.scope
(for_statement) @local.scope
(when_expression) @local.scope
(control_structure_body) @local.scope
(catch_block) @local.scope

(parameter (simple_identifier) @local.definition.parameter)
(lambda_parameters (variable_declaration) @local.definition.parameter)
(catch_block (simple_identifier) @local.definition.parameter)

(property_declaration (variable_declaration) @local.definition.var)
(property_declaration (multi_variable_declaration) @local.definition.var)
(for_statement (variable_declaration) @local.definition.var)
(when_subject (variable_declaration) @local.definition.var)
