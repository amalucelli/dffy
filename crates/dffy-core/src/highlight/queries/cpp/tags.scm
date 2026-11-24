; C++'s definitions before C's. A method defined in its class has the
; class as @container; one defined outside it (`int Point::area()`) has
; the scope its name is qualified by, which may as well be a namespace.
; A method declared in its class or struct (`Point();`) is defined
; elsewhere, and only a pure virtual one, which nothing defines, counts
; there. A constructor is left out, as its class is found instead. A
; class's fields have it as @container. Constants count at namespace scope.

(class_specifier
  name: (type_identifier) @container
  body: (field_declaration_list [
    (function_definition declarator: (_) @declarator) @definition.method
    (template_declaration
      (function_definition declarator: (_) @declarator) @definition.method)
    (field_declaration
      declarator: (function_declarator) @declarator
      default_value: (number_literal)) @definition.method
  ]))

(struct_specifier
  name: (type_identifier) @container
  body: (field_declaration_list [
    (function_definition declarator: (_) @declarator) @definition.method
    (template_declaration
      (function_definition declarator: (_) @declarator) @definition.method)
    (field_declaration
      declarator: (function_declarator) @declarator
      default_value: (number_literal)) @definition.method
  ]))

(class_specifier
  name: (type_identifier) @container
  body: (field_declaration_list
    (field_declaration declarator: (_) @declarator) @definition.field))

(class_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class

; A class or a struct an export macro names (`class LEVELDB_EXPORT DB {`)
; parses as a function its name is, of the type the macro's bodiless class
; is: the function's body is the class's.
(function_definition
  type: (class_specifier !body)
  declarator: (identifier) @name
  body: (compound_statement)) @definition.class

(function_definition
  type: (struct_specifier !body)
  declarator: (identifier) @name
  body: (compound_statement)) @definition.struct

(namespace_definition name: (namespace_identifier) @name) @definition.module

(alias_declaration name: (type_identifier) @name) @definition.type

(declaration
  (type_qualifier) @qualifier
  declarator: (init_declarator declarator: (identifier) @name)
  (#any-of? @qualifier "const" "constexpr")) @definition.constant
