; locals.scm - Dart scope and variable resolution queries

; ============================================================================
; Scopes
; ============================================================================

[
  (class_declaration)
  (mixin_declaration)
  (extension_declaration)
  (extension_type_declaration)
  (enum_declaration)
  (function_body)
  (function_expression)
  (block)
  (for_statement)
  (try_statement)
  (catch_clause)
  (finally_clause)
  (switch_statement_case)
] @local.scope

; ============================================================================
; Definitions
; ============================================================================

; Variables
(initialized_variable_definition
  name: (identifier) @local.definition)

(initialized_identifier
  (identifier) @local.definition)

(static_final_declaration
  (identifier) @local.definition)

; Parameters
(formal_parameter
  (identifier) @local.definition)

(constructor_param
  (identifier) @local.definition)

(super_formal_parameter
  (identifier) @local.definition)

; Catch variables
(catch_clause
  exception: (identifier) @local.definition)

(catch_clause
  stack_trace: (identifier) @local.definition)

; Functions
(function_signature
  name: (identifier) @local.definition)

(getter_signature
  name: (identifier) @local.definition)

(setter_signature
  name: (identifier) @local.definition)

; Types
(class_declaration
  name: (identifier) @local.definition)

(mixin_declaration
  (identifier) @local.definition)

(enum_declaration
  name: (identifier) @local.definition)

(extension_declaration
  name: (identifier) @local.definition)

(type_alias
  (type_identifier) @local.definition)

(type_parameter
  (type_identifier) @local.definition)

; Import aliases
(import_specification
  alias: (identifier) @local.definition)

; Pattern bindings (Dart 3.0)
(variable_pattern
  name: (identifier) @local.definition)

; ============================================================================
; References
; ============================================================================

(identifier) @local.reference
(type_identifier) @local.reference
