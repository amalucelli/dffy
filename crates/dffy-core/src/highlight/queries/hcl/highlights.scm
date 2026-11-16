; dffy's highlight query for tree-sitter-hcl, which ships none.

(comment) @comment

[
  (quoted_template_start)
  (quoted_template_end)
  (template_literal)
  (heredoc_start)
  (heredoc_identifier)
] @string

[
  (template_interpolation_start)
  (template_interpolation_end)
  (template_directive_start)
  (template_directive_end)
  (strip_marker)
] @punctuation.special

(numeric_lit) @number
[(bool_lit) (null_lit)] @constant.builtin

["for" "in" "if" "else" "endif" "endfor"] @keyword

["!" "*" "/" "%" "+" "-" ">" ">=" "<" "<=" "==" "!=" "&&" "||" "?" "=>"] @operator

[
  (block_start) (block_end)
  (object_start) (object_end)
  (tuple_start) (tuple_end)
  "(" ")" "[" "]"
] @punctuation.bracket

["." ".*" "[*]" "," (ellipsis)] @punctuation.delimiter

; A top-level block's type (`resource`, `variable`) reads as a keyword; a
; nested one (`ingress`, `lifecycle`) names a schema type.
(config_file (body (block . (identifier) @keyword)))
(block (body (block . (identifier) @type)))

(attribute (identifier) @property)
(object_elem key: (expression (variable_expr (identifier) @property)))

(function_call (identifier) @function)

(get_attr (identifier) @property)
(expression
  (variable_expr
    (identifier) @variable.builtin
    (#any-of? @variable.builtin "var" "local" "module" "data" "each" "count" "path" "self" "terraform"))
  .
  (get_attr))
