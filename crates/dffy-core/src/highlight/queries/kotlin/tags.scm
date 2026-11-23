; Definitions outside function bodies. Functions in a class, an interface,
; an object or a companion object have the type as their @container, an
; interface's and an abstract one's without a body too; so do the
; properties a body or a primary constructor declares, and enum entries.
; An extension function has its receiver's type. Top-level `const val`s are
; constants, other top-level properties variables. An `object` is a class.

(class_declaration
  (type_identifier) @container
  (class_body [
    (function_declaration (simple_identifier) @name) @definition.method
    (property_declaration (variable_declaration (simple_identifier) @name)) @definition.field
    (companion_object
      (class_body [
        (function_declaration (simple_identifier) @name) @definition.method
        (property_declaration (variable_declaration (simple_identifier) @name)) @definition.field
      ]))
  ]))

(class_declaration
  (type_identifier) @container
  (primary_constructor
    (class_parameter (binding_pattern_kind) (simple_identifier) @name) @definition.field))

(class_declaration
  (type_identifier) @container
  (enum_class_body [
    (enum_entry (simple_identifier) @name) @definition.enumerator
    (function_declaration (simple_identifier) @name) @definition.method
  ]))

(object_declaration
  (type_identifier) @container
  (class_body [
    (function_declaration (simple_identifier) @name) @definition.method
    (property_declaration (variable_declaration (simple_identifier) @name)) @definition.field
  ]))

(function_declaration
  (receiver_type [
    (user_type (type_identifier) @container)
    (nullable_type (user_type (type_identifier) @container))
  ])
  (simple_identifier) @name
  (function_body)) @definition.method

(function_declaration (simple_identifier) @name (function_body)) @definition.function

(class_declaration "interface" (type_identifier) @name) @definition.interface

(class_declaration "enum" (type_identifier) @name) @definition.enum

(class_declaration (type_identifier) @name) @definition.class

(object_declaration (type_identifier) @name) @definition.class

(type_alias (type_identifier) @name) @definition.type

(source_file
  (property_declaration
    (modifiers (property_modifier) @modifier)
    (variable_declaration (simple_identifier) @name)) @definition.constant
  (#eq? @modifier "const"))

(source_file
  (property_declaration (variable_declaration (simple_identifier) @name)) @definition.variable)
