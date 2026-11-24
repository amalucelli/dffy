; Scopes and the names they define, for what a name used in a function
; names there. A pattern's names are read from it; `self` is a keyword.

(function_item) @local.scope
(closure_expression) @local.scope
(block) @local.scope
(match_arm) @local.scope
(for_expression) @local.scope
(if_expression) @local.scope
(while_expression) @local.scope

(parameter pattern: (_) @local.definition.parameter)
(closure_parameters (_) @local.definition.parameter)

(let_declaration pattern: (_) @local.definition.var)
(for_expression pattern: (_) @local.definition.var)
(match_arm pattern: (_) @local.definition.var)
(let_condition pattern: (_) @local.definition.var)
