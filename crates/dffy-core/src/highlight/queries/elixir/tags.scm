; Definitions outside function bodies, which Elixir writes as calls of
; its definition macros. A module's or a protocol's functions (each clause
; of one), macros, guards, delegations, struct fields and module
; attributes have it as their @container; a dotted module name is named
; by its last part. Attributes Elixir itself reads aren't definitions.

(call
  target: (identifier) @keyword
  (arguments (alias) @container)
  (do_block
    (call
      target: (identifier) @define
      (arguments [
        (identifier) @name
        (call target: (identifier) @name)
        (binary_operator left: (call target: (identifier) @name))
      ])) @definition.method)
  (#any-of? @keyword "defmodule" "defprotocol" "defimpl")
  (#any-of? @define "def" "defp" "defdelegate" "defguard" "defguardp"))

(call
  target: (identifier) @keyword
  (arguments (alias) @container)
  (do_block
    (call
      target: (identifier) @define
      (arguments [
        (identifier) @name
        (call target: (identifier) @name)
        (binary_operator left: (call target: (identifier) @name))
      ])) @definition.macro)
  (#eq? @keyword "defmodule")
  (#any-of? @define "defmacro" "defmacrop"))

(call
  target: (identifier) @keyword
  (arguments (alias) @container)
  (do_block
    (call
      target: (identifier) @define
      (arguments [
        (list (atom) @name)
        (list (keywords (pair key: (keyword) @name)))
      ])) @definition.field)
  (#eq? @keyword "defmodule")
  (#eq? @define "defstruct"))

(call
  target: (identifier) @keyword
  (arguments (alias) @container)
  (do_block
    (unary_operator
      operator: "@"
      operand: (call target: (identifier) @name)) @definition.constant)
  (#eq? @keyword "defmodule")
  (#not-any-of? @name
    "moduledoc" "doc" "spec" "type" "typep" "opaque" "impl" "behaviour" "callback"
    "macrocallback" "derive" "enforce_keys" "compile" "dialyzer" "before_compile"
    "after_compile" "on_definition" "external_resource" "deprecated" "since" "optional_callbacks"))

(call
  target: (identifier) @keyword
  (arguments (alias) @name)
  (#eq? @keyword "defmodule")) @definition.module

(call
  target: (identifier) @keyword
  (arguments (alias) @name)
  (#eq? @keyword "defprotocol")) @definition.interface
