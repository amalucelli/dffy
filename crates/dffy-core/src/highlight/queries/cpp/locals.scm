; C++'s scopes and names past C's: a lambda's parameters, a range for's
; variable, a catch's parameter.

(lambda_expression) @local.scope
(for_range_loop) @local.scope
(catch_clause) @local.scope

(for_range_loop declarator: (_) @local.definition.var)
