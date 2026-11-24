; Scopes and the names they define, for what a name used in a function
; names there. Only `local` declares one; an assignment otherwise sets a
; global, the index's.

(function_declaration) @local.scope
(function_definition) @local.scope
(block) @local.scope
(for_statement) @local.scope

(parameters name: (identifier) @local.definition.parameter)

(variable_declaration (assignment_statement (variable_list) @local.definition.var))
(variable_declaration (variable_list) @local.definition.var)
(for_generic_clause (variable_list) @local.definition.var)
(for_numeric_clause name: (identifier) @local.definition.var)
