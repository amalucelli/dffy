; Definitions outside method bodies. A class's or a module's methods (its
; own `self.` ones and those in `class << self` included), its constants,
; the readers and writers `attr_*` makes and the instance variables
; `initialize` assigns have it as their @container, and so does a method
; defined as a call's argument (`private def secret`, `memoize def x`,
; `module_function def util`). A symbol's `:`, an instance variable's `@`
; and a method's `?`, `!` or `=` aren't part of the name.

(class
  name: [
    (constant) @container
    (scope_resolution name: (constant) @container)
  ]
  body: (body_statement [
    (method name: (_) @name) @definition.method
    (singleton_method name: (_) @name) @definition.method
    (assignment left: (constant) @name) @definition.constant
    (singleton_class
      (body_statement (method name: (_) @name) @definition.method))
    (call arguments: (argument_list (method name: (_) @name) @definition.method))
  ]))

(class
  name: [
    (constant) @container
    (scope_resolution name: (constant) @container)
  ]
  body: (body_statement
    (call
      method: (identifier) @attr
      arguments: (argument_list (simple_symbol) @name)) @definition.field)
  (#match? @attr "^attr_(reader|writer|accessor)$"))

(class
  name: [
    (constant) @container
    (scope_resolution name: (constant) @container)
  ]
  body: (body_statement
    (method
      name: (identifier) @init
      body: (body_statement
        (assignment left: (instance_variable) @name) @definition.field)))
  (#eq? @init "initialize"))

(module
  name: [
    (constant) @container
    (scope_resolution name: (constant) @container)
  ]
  body: (body_statement [
    (method name: (_) @name) @definition.method
    (singleton_method name: (_) @name) @definition.method
    (assignment left: (constant) @name) @definition.constant
    (call arguments: (argument_list (method name: (_) @name) @definition.method))
  ]))

(class
  name: [
    (constant) @name
    (scope_resolution name: (constant) @name)
  ]) @definition.class

(module
  name: [
    (constant) @name
    (scope_resolution name: (constant) @name)
  ]) @definition.module

(method name: (_) @name) @definition.function

(singleton_method name: (_) @name) @definition.function

(program (assignment left: (constant) @name) @definition.constant)
