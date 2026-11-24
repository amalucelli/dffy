; JavaScript's parameters, after the locals it shares with TypeScript.

(formal_parameters (identifier) @local.definition.parameter)
(formal_parameters (assignment_pattern left: (_) @local.definition.parameter))
(formal_parameters (rest_pattern (_) @local.definition.parameter))
(formal_parameters (object_pattern) @local.definition.parameter)
(formal_parameters (array_pattern) @local.definition.parameter)
