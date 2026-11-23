; Definitions outside function bodies. Classes, structs, enums, actors
; and extensions share one node, told apart by their keyword. Functions
; with a body, properties and enum cases in a type's body have the type as
; their @container: for an extension, the type it extends, which itself
; isn't defined there. A protocol's requirements have it as theirs.
; Top-level `let`s are constants, `var`s variables. An actor is a class.

(class_declaration
  name: [
    (type_identifier) @container
    (user_type (type_identifier) @container)
  ]
  body: (class_body [
    (function_declaration name: (simple_identifier) @name body: (function_body)) @definition.method
    (property_declaration
      name: (pattern bound_identifier: (simple_identifier) @name)) @definition.field
  ]))

(class_declaration
  name: (type_identifier) @container
  body: (enum_class_body [
    (enum_entry name: (simple_identifier) @name @definition.enumerator)
    (function_declaration name: (simple_identifier) @name body: (function_body)) @definition.method
    (property_declaration
      name: (pattern bound_identifier: (simple_identifier) @name)) @definition.field
  ]))

(protocol_declaration
  name: (type_identifier) @container
  body: (protocol_body [
    (protocol_function_declaration name: (simple_identifier) @name) @definition.method
    (protocol_property_declaration
      name: (pattern bound_identifier: (simple_identifier) @name)) @definition.field
  ]))

(class_declaration declaration_kind: "struct" name: (type_identifier) @name) @definition.struct

(class_declaration declaration_kind: "enum" name: (type_identifier) @name) @definition.enum

(class_declaration declaration_kind: "class" name: (type_identifier) @name) @definition.class

(class_declaration declaration_kind: "actor" name: (type_identifier) @name) @definition.class

(protocol_declaration name: (type_identifier) @name) @definition.interface

(typealias_declaration name: (type_identifier) @name) @definition.type

(function_declaration name: (simple_identifier) @name body: (function_body)) @definition.function

(source_file
  (property_declaration
    (value_binding_pattern mutability: "let")
    name: (pattern bound_identifier: (simple_identifier) @name)) @definition.constant)

(source_file
  (property_declaration
    name: (pattern bound_identifier: (simple_identifier) @name)) @definition.variable)
