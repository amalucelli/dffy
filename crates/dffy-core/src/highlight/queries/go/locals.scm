; Scopes and the names they define, for what a name used in a function
; names there. A method's receiver is a parameter.

(function_declaration) @local.scope
(method_declaration) @local.scope
(func_literal) @local.scope
(block) @local.scope
(if_statement) @local.scope
(for_statement) @local.scope
(expression_switch_statement) @local.scope
(type_switch_statement) @local.scope
(expression_case) @local.scope
(type_case) @local.scope
(communication_case) @local.scope

(parameter_declaration name: (identifier) @local.definition.parameter)
(variadic_parameter_declaration name: (identifier) @local.definition.parameter)

(short_var_declaration left: (expression_list) @local.definition.var)
(var_spec name: (identifier) @local.definition.var)
(const_spec name: (identifier) @local.definition.var)
(range_clause left: (expression_list) @local.definition.var)
(type_switch_statement alias: (expression_list) @local.definition.var)
(receive_statement left: (expression_list) @local.definition.var)
