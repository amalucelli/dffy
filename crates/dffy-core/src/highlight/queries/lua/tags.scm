; Definitions outside function bodies. A function or a value stored in a
; table at file scope (`function M.area()`, `function M:draw()`,
; `M.version = …`) has the table as its @container. Names assigned at file
; scope, global or local, are variables.

(function_declaration
  name: [
    (dot_index_expression table: (identifier) @container field: (identifier) @name)
    (method_index_expression table: (identifier) @container method: (identifier) @name)
  ]) @definition.method

(function_declaration name: (identifier) @name) @definition.function

(chunk
  (assignment_statement
    (variable_list
      name: (dot_index_expression table: (identifier) @container field: (identifier) @name))
    (expression_list value: (function_definition))) @definition.method)

(chunk
  (assignment_statement
    (variable_list
      name: (dot_index_expression table: (identifier) @container field: (identifier) @name))) @definition.field)

(chunk
  (assignment_statement (variable_list name: (identifier) @name)) @definition.variable)

(chunk
  (variable_declaration
    (assignment_statement (variable_list name: (identifier) @name))) @definition.variable)
