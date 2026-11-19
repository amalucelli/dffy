(function_definition
  declarator: (function_declarator declarator: (_) @name)) @scope
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (_) @name))) @scope
(struct_specifier "struct" @kind name: (_) @name body: (_)) @scope
(union_specifier "union" @kind name: (_) @name body: (_)) @scope
(enum_specifier "enum" @kind name: (_) @name body: (_)) @scope
