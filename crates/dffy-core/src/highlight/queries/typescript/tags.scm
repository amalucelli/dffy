; TypeScript's definitions after JavaScript's. Signatures that an
; implementation follows (overloads, `declare`) only declare, and an
; ambient declaration's contents are left to the walk to skip; but an
; interface's members, an abstract class's abstract methods and the
; members of an object type an alias names are what a reader jumps to,
; with their type as @container. Class fields are fields, or methods when
; they hold a function. Enum members have their enum as @container.

(class_declaration
  name: (_) @container
  body: (class_body
    (public_field_definition
      name: (property_identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.method))

(class_declaration
  name: (_) @container
  body: (class_body
    (public_field_definition name: (property_identifier) @name) @definition.field))

(abstract_class_declaration
  name: (_) @container
  body: (class_body [
    (method_definition
      name: (property_identifier) @name
      (#not-eq? @name "constructor")) @definition.method
    (abstract_method_signature name: (property_identifier) @name) @definition.method
    (public_field_definition name: (property_identifier) @name) @definition.field
  ]))

(interface_declaration
  name: (_) @container
  body: (interface_body [
    (method_signature name: (property_identifier) @name) @definition.method
    (property_signature name: (property_identifier) @name) @definition.field
  ]))

(type_alias_declaration
  name: (_) @container
  value: [
    (object_type [
      (method_signature name: (property_identifier) @name) @definition.method
      (property_signature name: (property_identifier) @name) @definition.field
    ])
    (intersection_type
      (object_type [
        (method_signature name: (property_identifier) @name) @definition.method
        (property_signature name: (property_identifier) @name) @definition.field
      ]))
  ])

(abstract_class_declaration name: (_) @name) @definition.class

(interface_declaration name: (_) @name) @definition.interface

(type_alias_declaration name: (_) @name) @definition.type

(enum_declaration
  name: (_) @container
  body: (enum_body [
    (property_identifier) @name @definition.enumerator
    (enum_assignment name: (property_identifier) @name) @definition.enumerator
  ]))

(enum_declaration name: (_) @name) @definition.enum

(internal_module name: (_) @name) @definition.module

(module name: (identifier) @name) @definition.module
