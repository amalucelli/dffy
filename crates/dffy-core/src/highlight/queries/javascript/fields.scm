; JavaScript's class fields: a method when they hold a function, else a
; field. TypeScript names the field's node differently, in its own
; tags.scm.

(class_declaration
  name: (_) @container
  body: (class_body
    (field_definition
      property: (property_identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.method))

(class_declaration
  name: (_) @container
  body: (class_body
    (field_definition property: (property_identifier) @name) @definition.field))
