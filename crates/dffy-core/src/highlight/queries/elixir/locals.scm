; Scopes and the names they define, for what a name used in a function
; names there: a `def`'s arguments, a clause's patterns, what `=` binds.

(call
  target: (identifier) @_def
  (#any-of? @_def "def" "defp" "defmacro" "defmacrop")) @local.scope
(anonymous_function) @local.scope
(stab_clause) @local.scope

(call
  target: (identifier) @_def
  (arguments [
    (call (arguments) @local.definition.parameter)
    (binary_operator left: (call (arguments) @local.definition.parameter))
  ])
  (#any-of? @_def "def" "defp" "defmacro" "defmacrop"))
(stab_clause left: (arguments) @local.definition.parameter)

(binary_operator left: (_) @local.definition.var operator: "=")
