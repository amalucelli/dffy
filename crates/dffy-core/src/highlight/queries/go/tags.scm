; Definitions at package scope. A method's @container is its receiver's
; type, without a pointer or type arguments; a struct's named fields and
; an interface's methods have the type as theirs. A type is a struct or an
; interface by what it's declared as. `init` functions can't be named,
; and `_` names nothing.

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (type_identifier) @container
        (pointer_type (type_identifier) @container)
        (generic_type type: (type_identifier) @container)
        (pointer_type (generic_type type: (type_identifier) @container))
      ]))
  name: (field_identifier) @name) @definition.method

(method_declaration name: (field_identifier) @name) @definition.method

(type_spec
  name: (type_identifier) @container
  type: (struct_type
    (field_declaration_list
      (field_declaration name: (field_identifier) @name) @definition.field)))

(type_spec
  name: (type_identifier) @container
  type: (interface_type
    (method_elem name: (field_identifier) @name) @definition.method))

(function_declaration
  name: (identifier) @name
  (#not-eq? @name "init")) @definition.function

(type_spec name: (type_identifier) @name type: (struct_type)) @definition.struct

(type_spec name: (type_identifier) @name type: (interface_type)) @definition.interface

(type_spec name: (type_identifier) @name) @definition.type

(type_alias name: (type_identifier) @name) @definition.type

(const_spec
  name: (identifier) @name
  (#not-eq? @name "_")) @definition.constant

(var_spec
  name: (identifier) @name
  (#not-eq? @name "_")) @definition.variable
