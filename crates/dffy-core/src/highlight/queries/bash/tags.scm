; Functions outside other functions' bodies, and variables a script
; assigns at its top level, `readonly` ones as constants. `export` of a
; variable from the environment assigns it too.

(function_definition name: (word) @name) @definition.function

(program
  (declaration_command
    "readonly"
    (variable_assignment name: (variable_name) @name)) @definition.constant)

(program
  (declaration_command (variable_assignment name: (variable_name) @name)) @definition.variable)

(program (variable_assignment name: (variable_name) @name) @definition.variable)
