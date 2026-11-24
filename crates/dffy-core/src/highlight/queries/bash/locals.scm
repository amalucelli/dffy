; Scopes and the names they define, for what a `$name` used in a function
; names there. What the script's top level sets is the index's.

(function_definition) @local.scope

(variable_assignment name: (variable_name) @local.definition.var)
(for_statement variable: (variable_name) @local.definition.var)
