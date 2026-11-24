; Definitions outside function bodies. A class's methods, the names its
; body assigns and the `self.x` its `__init__` assigns have it as their
; @container. Names assigned at module scope, or in a block under `if` or
; `try` there, are variables, or constants when written in capitals; the
; walk never reaches such a block inside a function. A relative import at
; module scope re-exports what it imports, which only a package's
; `__init__.py` is taken to mean.

(class_definition
  name: (identifier) @container
  body: (block [
    (function_definition name: (identifier) @name) @definition.method
    (decorated_definition
      definition: (function_definition name: (identifier) @name) @definition.method)
    (expression_statement
      (assignment left: (identifier) @name)) @definition.field
  ]))

(class_definition
  name: (identifier) @container
  body: (block
    (function_definition
      name: (identifier) @init
      body: (block
        (expression_statement
          (assignment
            left: (attribute
              object: (identifier) @self
              attribute: (identifier) @name))) @definition.field))
    (#eq? @init "__init__")
    (#eq? @self "self")))

(class_definition name: (identifier) @name) @definition.class

(function_definition name: (identifier) @name) @definition.function

(module
  (expression_statement
    (assignment left: (identifier) @name)) @definition.constant
  (#match? @name "^[A-Z][A-Z0-9_]*$"))

(module
  (expression_statement
    (assignment left: (identifier) @name)) @definition.variable)

(module
  (expression_statement
    (assignment left: (pattern_list (identifier) @name))) @definition.variable)

(if_statement
  consequence: (block
    (expression_statement (assignment left: (identifier) @name)) @definition.variable))

(else_clause
  body: (block
    (expression_statement (assignment left: (identifier) @name)) @definition.variable))

(try_statement
  body: (block
    (expression_statement (assignment left: (identifier) @name)) @definition.variable))

(except_clause
  (block
    (expression_statement (assignment left: (identifier) @name)) @definition.variable))

(type_alias_statement left: (type (identifier) @name)) @definition.type

(module
  (import_from_statement
    module_name: (relative_import)
    name: [
      (dotted_name . (identifier) @name .)
      (aliased_import name: (dotted_name (identifier) @original .) alias: (identifier) @name)
    ]) @definition.reexport)
