; Definitions outside function bodies. A container is named by the
; declaration it's the value of: its fields, functions and declarations
; have that name as their @container, and an enum's fields are its
; values. A declaration whose value is a container is that container,
; nested in another or not, so those patterns come first. Top-level
; `const`s are constants, `var`s variables.

(variable_declaration (identifier) @name (struct_declaration)) @definition.struct

(variable_declaration (identifier) @name (enum_declaration)) @definition.enum

(variable_declaration (identifier) @name (union_declaration)) @definition.union

(variable_declaration (identifier) @name (opaque_declaration)) @definition.type

(variable_declaration
  (identifier) @container
  [
    (struct_declaration [
      (container_field name: (identifier) @name) @definition.field
      (function_declaration name: (identifier) @name) @definition.method
      (variable_declaration (identifier) @name) @definition.constant
    ])
    (union_declaration [
      (container_field name: (identifier) @name) @definition.field
      (function_declaration name: (identifier) @name) @definition.method
    ])
    (enum_declaration [
      (container_field name: (identifier) @name) @definition.enumerator
      (function_declaration name: (identifier) @name) @definition.method
      (variable_declaration (identifier) @name) @definition.constant
    ])
    (opaque_declaration
      (function_declaration name: (identifier) @name) @definition.method)
  ])

(function_declaration name: (identifier) @name) @definition.function

(source_file
  (variable_declaration "const" (identifier) @name (#not-eq? @name "_")) @definition.constant)

(source_file
  (variable_declaration (identifier) @name (#not-eq? @name "_")) @definition.variable)
