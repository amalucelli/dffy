; Types, and in them their methods, fields, enum constants and record
; components, each with its type as @container. A method without a body
; counts where it's what a reader jumps to: an interface's, an abstract
; one. Constructors are named after their class, which is found instead.
; `static final` fields are constants. Records are classes, annotation
; types interfaces.

(class_declaration
  name: (identifier) @container
  body: (class_body [
    (method_declaration name: (identifier) @name) @definition.method
    (field_declaration
      (modifiers "static" "final")
      declarator: (variable_declarator name: (identifier) @name)) @definition.constant
  ]))

(class_declaration
  name: (identifier) @container
  body: (class_body
    (field_declaration
      declarator: (variable_declarator name: (identifier) @name)) @definition.field))

(record_declaration
  name: (identifier) @container
  parameters: (formal_parameters
    (formal_parameter name: (identifier) @name) @definition.field))

(record_declaration
  name: (identifier) @container
  body: (class_body
    (method_declaration name: (identifier) @name) @definition.method))

(interface_declaration
  name: (identifier) @container
  body: (interface_body [
    (method_declaration name: (identifier) @name) @definition.method
    (constant_declaration
      declarator: (variable_declarator name: (identifier) @name)) @definition.constant
  ]))

(annotation_type_declaration
  name: (identifier) @container
  body: (annotation_type_body
    (annotation_type_element_declaration name: (identifier) @name) @definition.method))

(enum_declaration
  name: (identifier) @container
  body: (enum_body [
    (enum_constant name: (identifier) @name) @definition.enumerator
    (enum_body_declarations [
      (method_declaration name: (identifier) @name) @definition.method
      (field_declaration
        declarator: (variable_declarator name: (identifier) @name)) @definition.field
    ])
  ]))

(class_declaration name: (identifier) @name) @definition.class

(record_declaration name: (identifier) @name) @definition.class

(interface_declaration name: (identifier) @name) @definition.interface

(annotation_type_declaration name: (identifier) @name) @definition.interface

(enum_declaration name: (identifier) @name) @definition.enum
