(function_declaration "function" @kind name: (_) @name) @scope
(generator_function_declaration "function" @kind name: (_) @name) @scope
(class_declaration "class" @kind name: (_) @name) @scope
(method_definition name: (_) @name) @scope
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @scope
