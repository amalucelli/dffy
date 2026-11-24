; Scopes and the names they define, for what a name used in a function
; names there. A class's body is a scope only what's directly in it sees.
; `global` and `nonlocal` say whose a name is.

(function_definition) @local.scope
(lambda) @local.scope
(list_comprehension) @local.scope
(set_comprehension) @local.scope
(dictionary_comprehension) @local.scope
(generator_expression) @local.scope
(class_definition body: (block) @local.scope.class)

(global_statement (identifier) @local.global)
(nonlocal_statement (identifier) @local.nonlocal)

(parameters (identifier) @local.definition.parameter)
(parameters (default_parameter name: (identifier) @local.definition.parameter))
(parameters (typed_parameter (identifier) @local.definition.parameter))
(parameters (typed_default_parameter name: (identifier) @local.definition.parameter))
(parameters (list_splat_pattern (identifier) @local.definition.parameter))
(parameters (dictionary_splat_pattern (identifier) @local.definition.parameter))
(lambda_parameters (identifier) @local.definition.parameter)

(assignment left: (_) @local.definition.var)
(augmented_assignment left: (identifier) @local.definition.var)
(for_statement left: (_) @local.definition.var)
(for_in_clause left: (_) @local.definition.var)
(with_item value: (as_pattern alias: (as_pattern_target) @local.definition.var))
(except_clause (as_pattern alias: (as_pattern_target) @local.definition.var))
(named_expression name: (identifier) @local.definition.var)
(function_definition name: (identifier) @local.definition.var)
(class_definition name: (identifier) @local.definition.var)
