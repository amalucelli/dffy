; Scopes and the names they define, for what a name used in a function
; names there. A container's declarations are the index's.

(function_declaration) @local.scope
(block) @local.scope
(for_statement) @local.scope
(while_statement) @local.scope
(if_statement) @local.scope

(parameter name: (identifier) @local.definition.parameter)

(variable_declaration . (identifier) @local.definition.var)
(payload (identifier) @local.definition.var)
