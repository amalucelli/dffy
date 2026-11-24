; Definitions outside function bodies. A class's, an object's, a trait's
; or an enum's methods (abstract ones without a body too), `val`s and
; `var`s, a class's parameters and an enum's cases have it as their
; @container; an extension's methods have the extended type. Objects are
; classes; top-level `val`s, `var`s and givens are variables.

(class_definition
  name: (identifier) @container
  body: (template_body [
    (function_definition name: (identifier) @name) @definition.method
    (function_declaration name: (identifier) @name) @definition.method
    (val_definition pattern: (identifier) @name) @definition.field
    (var_definition pattern: (identifier) @name) @definition.field
  ]))

(class_definition
  name: (identifier) @container
  class_parameters: (class_parameters
    (class_parameter name: (identifier) @name) @definition.field))

(object_definition
  name: (identifier) @container
  body: (template_body [
    (function_definition name: (identifier) @name) @definition.method
    (val_definition pattern: (identifier) @name) @definition.field
    (var_definition pattern: (identifier) @name) @definition.field
  ]))

(trait_definition
  name: (identifier) @container
  body: (template_body [
    (function_definition name: (identifier) @name) @definition.method
    (function_declaration name: (identifier) @name) @definition.method
    (val_definition pattern: (identifier) @name) @definition.field
    (val_declaration name: (identifier) @name) @definition.field
  ]))

(enum_definition
  name: (identifier) @container
  body: (enum_body [
    (enum_case_definitions [
      (simple_enum_case name: (identifier) @name) @definition.enumerator
      (full_enum_case name: (identifier) @name) @definition.enumerator
    ])
    (function_definition name: (identifier) @name) @definition.method
  ]))

(extension_definition
  parameters: (parameters (parameter type: (type_identifier) @container))
  body: [
    (function_definition name: (identifier) @name) @definition.method
    (template_body (function_definition name: (identifier) @name) @definition.method)
  ])

(class_definition name: (identifier) @name) @definition.class

(object_definition name: (identifier) @name) @definition.class

(trait_definition name: (identifier) @name) @definition.trait

(enum_definition name: (identifier) @name) @definition.enum

(type_definition name: (type_identifier) @name) @definition.type

(function_definition name: (identifier) @name) @definition.function

(compilation_unit (val_definition pattern: (identifier) @name) @definition.variable)

(compilation_unit (var_definition pattern: (identifier) @name) @definition.variable)

(given_definition name: (identifier) @name) @definition.variable
