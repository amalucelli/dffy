(class_declaration declaration_kind: _ @kind name: (_) @name) @scope
(protocol_declaration declaration_kind: _ @kind name: (_) @name) @scope
; The name as a field or a named child costs the compile 80ms.
(function_declaration "func" @kind . _ @name) @scope
(init_declaration name: _ @kind) @scope
