; Scopes and the names they define, for what a name used in a function
; names there. A pattern's capitalized name is a constant it compares with.

(function_definition) @local.scope
(lambda_expression) @local.scope
(block) @local.scope
(case_clause) @local.scope
(for_expression) @local.scope

(parameter name: (identifier) @local.definition.parameter)
(lambda_expression parameters: (identifier) @local.definition.parameter)

(val_definition pattern: (_) @local.definition.var)
(var_definition pattern: (_) @local.definition.var)
(enumerator . (_) @local.definition.var)
(case_clause pattern: (_) @local.definition.var)
