; Scopes and the names they define, for what a name used in a function
; names there. A declarator's name is read from its chain.

(function_definition) @local.scope
(compound_statement) @local.scope
(for_statement) @local.scope

(parameter_declaration declarator: (_) @local.definition.parameter)

(declaration declarator: (_) @local.definition.var)
