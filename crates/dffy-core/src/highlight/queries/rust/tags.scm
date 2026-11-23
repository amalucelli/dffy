; Items outside function bodies. When two patterns capture one definition
; the earlier wins, so items in impls and traits come before plain ones.
; Their @container is the type an impl is on, or the trait for what a trait
; declares or provides: a trait's members count with or without a body, as
; they're what a call through the trait names. A struct's or a union's
; named fields and an enum's variants have their type as @container. A
; const or static outside a trait without a value only declares one, and
; `_` names nothing. A `pub use` re-exports each name it ends at, one it
; renames with the name it had as @original.

(impl_item
  type: [
    (type_identifier) @container
    (scoped_type_identifier name: (type_identifier) @container)
    (generic_type type: (type_identifier) @container)
    (generic_type type: (scoped_type_identifier name: (type_identifier) @container))
  ]
  body: (declaration_list [
    (function_item name: (identifier) @name) @definition.method
    (const_item name: (identifier) @name value: (_)) @definition.constant
    (type_item name: (type_identifier) @name) @definition.type
  ]))

(impl_item
  body: (declaration_list (function_item name: (identifier) @name) @definition.method))

(trait_item
  name: (type_identifier) @container
  body: (declaration_list [
    (function_item name: (identifier) @name) @definition.method
    (function_signature_item name: (identifier) @name) @definition.method
    (const_item name: (identifier) @name) @definition.constant
    (associated_type name: (type_identifier) @name) @definition.type
  ]))

(struct_item
  name: (type_identifier) @container
  body: (field_declaration_list
    (field_declaration name: (field_identifier) @name) @definition.field))

(union_item
  name: (type_identifier) @container
  body: (field_declaration_list
    (field_declaration name: (field_identifier) @name) @definition.field))

(enum_item
  name: (type_identifier) @container
  body: (enum_variant_list
    (enum_variant name: (identifier) @name) @definition.enumerator))

(function_item name: (identifier) @name) @definition.function

(struct_item name: (type_identifier) @name) @definition.struct

(enum_item name: (type_identifier) @name) @definition.enum

(union_item name: (type_identifier) @name) @definition.union

(trait_item name: (type_identifier) @name) @definition.trait

(type_item name: (type_identifier) @name) @definition.type

(const_item name: (identifier) @name value: (_) (#not-eq? @name "_")) @definition.constant

(static_item name: (identifier) @name value: (_) (#not-eq? @name "_")) @definition.static

(macro_definition name: (identifier) @name) @definition.macro

(mod_item name: (identifier) @name) @definition.module

(use_declaration
  (visibility_modifier)
  argument: [
    (identifier) @name
    (scoped_identifier name: (identifier) @name)
    (use_as_clause
      path: [(identifier) @original (scoped_identifier name: (identifier) @original)]
      alias: (identifier) @name)
    (scoped_use_list list: (use_list [
      (identifier) @name
      (scoped_identifier name: (identifier) @name)
      (use_as_clause
        path: [(identifier) @original (scoped_identifier name: (identifier) @original)]
        alias: (identifier) @name)
    ]))
  ]
  (#not-eq? @name "self")) @definition.reexport
