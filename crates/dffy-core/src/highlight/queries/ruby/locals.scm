; Scopes and the names they define, for what a name used in a method names
; there. A method starts afresh; a block sees what's around it.

(method) @local.scope
(singleton_method) @local.scope
(block) @local.scope
(do_block) @local.scope
(lambda) @local.scope

(method_parameters (identifier) @local.definition.parameter)
(lambda_parameters (identifier) @local.definition.parameter)
(block_parameters (identifier) @local.definition.parameter)
(destructured_parameter (identifier) @local.definition.parameter)
(optional_parameter name: (identifier) @local.definition.parameter)
(splat_parameter name: (identifier) @local.definition.parameter)
(hash_splat_parameter name: (identifier) @local.definition.parameter)
(keyword_parameter name: (identifier) @local.definition.parameter)
(block_parameter name: (identifier) @local.definition.parameter)

(assignment left: (identifier) @local.definition.var)
(assignment left: (left_assignment_list) @local.definition.var)
(operator_assignment left: (identifier) @local.definition.var)
(exception_variable (identifier) @local.definition.var)
