; Definitions at file scope. Prototypes, declarations and tagged types
; without a body only refer to a definition elsewhere. Variables are left
; out: the kernel's attributes after a name (`__read_mostly`) parse as the
; declarator, which would define the attribute. Definitions made by macros
; (SYSCALL_DEFINE3, DEFINE_MUTEX) are read in highlight/tags.rs, as the
; grammar parses their uses in many shapes. A @declarator's name is the
; identifier its chain of declarators ends at.

(function_definition declarator: (_) @declarator) @definition.function

(preproc_def name: (identifier) @name) @definition.macro

(preproc_function_def name: (identifier) @name) @definition.macro

(type_definition declarator: (_) @declarator) @definition.type

(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.struct

(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.union

(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @definition.enum

(enumerator name: (identifier) @name) @definition.enumerator

; A struct's or a union's fields, with it as @container: its tag, or the
; name a typedef gives an untagged one. A field of an untagged struct
; nested in another has none.

(struct_specifier
  name: (type_identifier) @container
  body: (field_declaration_list
    (field_declaration declarator: (_) @declarator) @definition.field))

(union_specifier
  name: (type_identifier) @container
  body: (field_declaration_list
    (field_declaration declarator: (_) @declarator) @definition.field))

(type_definition
  type: [
    (struct_specifier
      !name
      body: (field_declaration_list
        (field_declaration declarator: (_) @declarator) @definition.field))
    (union_specifier
      !name
      body: (field_declaration_list
        (field_declaration declarator: (_) @declarator) @definition.field))
  ]
  declarator: (type_identifier) @container)
