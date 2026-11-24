; Types, and in them their methods, properties, fields and events, each
; with its type as @container; `const` fields are constants. A method
; without a body counts in an interface and as an abstract one; partial
; ones are defined where they have a body. Constructors are named after
; their type. A namespace is named by its last part.

(class_declaration
  name: (identifier) @container
  body: (declaration_list
    (method_declaration name: (identifier) @name body: (_)) @definition.method))

(class_declaration
  name: (identifier) @container
  body: (declaration_list
    (method_declaration
      (modifier) @modifier
      name: (identifier) @name) @definition.method)
  (#eq? @modifier "abstract"))

(class_declaration
  name: (identifier) @container
  body: (declaration_list
    (field_declaration
      (modifier) @modifier
      (variable_declaration (variable_declarator name: (identifier) @name))) @definition.constant)
  (#eq? @modifier "const"))

(struct_declaration
  name: (identifier) @container
  body: (declaration_list
    (method_declaration name: (identifier) @name body: (_)) @definition.method))

(struct_declaration
  name: (identifier) @container
  body: (declaration_list
    (field_declaration
      (modifier) @modifier
      (variable_declaration (variable_declarator name: (identifier) @name))) @definition.constant)
  (#eq? @modifier "const"))

(record_declaration
  name: (identifier) @container
  body: (declaration_list
    (method_declaration name: (identifier) @name body: (_)) @definition.method))

(record_declaration
  name: (identifier) @container
  (parameter_list (parameter name: (identifier) @name) @definition.field))

(interface_declaration
  name: (identifier) @container
  body: (declaration_list
    (method_declaration name: (identifier) @name) @definition.method))

(class_declaration
  name: (identifier) @container
  body: (declaration_list [
    (property_declaration name: (identifier) @name) @definition.field
    (field_declaration
      (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
    (event_field_declaration
      (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
  ]))

(struct_declaration
  name: (identifier) @container
  body: (declaration_list [
    (property_declaration name: (identifier) @name) @definition.field
    (field_declaration
      (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
  ]))

(record_declaration
  name: (identifier) @container
  body: (declaration_list
    (property_declaration name: (identifier) @name) @definition.field))

(interface_declaration
  name: (identifier) @container
  body: (declaration_list
    (property_declaration name: (identifier) @name) @definition.field))

(enum_declaration
  name: (identifier) @container
  body: (enum_member_declaration_list
    (enum_member_declaration name: (identifier) @name) @definition.enumerator))

(class_declaration name: (identifier) @name) @definition.class

(record_declaration name: (identifier) @name) @definition.class

(struct_declaration name: (identifier) @name) @definition.struct

(interface_declaration name: (identifier) @name) @definition.interface

(enum_declaration name: (identifier) @name) @definition.enum

(delegate_declaration name: (identifier) @name) @definition.type

(namespace_declaration
  name: [
    (identifier) @name
    (qualified_name name: (identifier) @name)
  ]) @definition.module

(file_scoped_namespace_declaration
  name: [
    (identifier) @name
    (qualified_name name: (identifier) @name)
  ]) @definition.module
