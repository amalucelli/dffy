; Definitions outside function bodies. A class's, an interface's, a
; trait's or an enum's methods (abstract and interface ones without a body
; too), properties, constants and enum cases have it as their @container;
; a constructor is named after nothing to look up. A property is named
; without its `$`. A namespace is named by its last part.

(class_declaration
  name: (name) @container
  body: (declaration_list [
    (method_declaration name: (name) @name) @definition.method
    (property_declaration (property_element name: (variable_name (name) @name))) @definition.field
    (const_declaration (const_element (name) @name)) @definition.constant
  ])
  (#not-eq? @name "__construct"))

(interface_declaration
  name: (name) @container
  body: (declaration_list [
    (method_declaration name: (name) @name) @definition.method
    (const_declaration (const_element (name) @name)) @definition.constant
  ]))

(trait_declaration
  name: (name) @container
  body: (declaration_list [
    (method_declaration name: (name) @name) @definition.method
    (property_declaration (property_element name: (variable_name (name) @name))) @definition.field
  ])
  (#not-eq? @name "__construct"))

(enum_declaration
  name: (name) @container
  body: (enum_declaration_list [
    (enum_case name: (name) @name) @definition.enumerator
    (method_declaration name: (name) @name) @definition.method
    (const_declaration (const_element (name) @name)) @definition.constant
  ]))

(class_declaration name: (name) @name) @definition.class

(interface_declaration name: (name) @name) @definition.interface

(trait_declaration name: (name) @name) @definition.trait

(enum_declaration name: (name) @name) @definition.enum

(function_definition name: (name) @name) @definition.function

(const_declaration (const_element (name) @name)) @definition.constant

(namespace_definition name: (namespace_name (name) @name .)) @definition.module
