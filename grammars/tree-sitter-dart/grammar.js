/**
 * @file Dart grammar for tree-sitter
 * @author Kasper Overgård Nielsen <kasper@byolimit.com>
 * @license MIT
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Operator precedence levels (from Dart spec, ascending)
const PREC = {
  CASCADE: 2,
  ASSIGNMENT: 3,
  CONDITIONAL: 4,
  IF_NULL: 5,
  LOGICAL_OR: 6,
  LOGICAL_AND: 7,
  EQUALITY: 8,
  RELATIONAL: 9,
  BITWISE_OR: 10,
  BITWISE_XOR: 11,
  BITWISE_AND: 12,
  SHIFT: 13,
  ADDITIVE: 14,
  MULTIPLICATIVE: 15,
  UNARY_PREFIX: 16,
  UNARY_POSTFIX: 17,
  SELECTOR: 18,
  TYPE_IDENTIFIER: 19,
};

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}

function commaSep(rule) {
  return optional(commaSep1(rule));
}

function commaSep1TrailingComma(rule) {
  return seq(rule, repeat(seq(",", rule)), optional(","));
}

function commaSepTrailingComma(rule) {
  return optional(commaSep1TrailingComma(rule));
}

// Wraps a type expression with the optional `?` nullable marker.
function nullable(rule) {
  return seq(rule, optional("?"));
}

// Produces the 5 wrapper rules shared between non-cascade and cascade postfix
// chains: member/index/null-aware-member/null-aware-index/null-assertion.
// `base` is the recursive postfix-chain rule name (the "object" position).
// `prefix` is "" for non-cascade and "cascade_" for cascade variants.
function postfixChainWrappers(base, prefix) {
  return {
    [`${prefix}member_expression`]: ($) =>
      prec.left(PREC.UNARY_POSTFIX, seq(
        field("object", $[base]), ".", field("property", $.identifier),
      )),
    [`${prefix}index_expression`]: ($) =>
      prec.left(PREC.UNARY_POSTFIX, seq(
        field("object", $[base]), "[", field("index", $._expression), "]",
      )),
    [`${prefix}null_aware_member_expression`]: ($) =>
      prec.left(PREC.UNARY_POSTFIX, seq(
        field("object", $[base]), "?.", field("property", $.identifier),
      )),
    [`${prefix}null_aware_index_expression`]: ($) =>
      prec.left(PREC.UNARY_POSTFIX, seq(
        field("object", $[base]), "?", "[", field("index", $._expression), "]",
      )),
    [`${prefix}null_assertion_expression`]: ($) =>
      prec.left(PREC.UNARY_POSTFIX, seq(
        field("value", $[base]), "!",
      )),
  };
}

module.exports = grammar({
  name: "dart",

  externals: ($) => [
    $.template_chars_single,
    $.template_chars_double,
    $.template_chars_single_single,
    $.template_chars_double_single,
    $.template_chars_raw_slash,
    $.block_comment,
    $.documentation_block_comment,
    $.annotation_open_paren,
  ],

  extras: ($) => [/\s/, $.comment],

  word: ($) => $._name,

  supertypes: ($) => [$._statement, $._literal, $._declaration, $._instantiation],

  precedences: ($) => [
    ["call", "instantiation"],
  ],

  conflicts: ($) => [
    // Type vs expression ambiguity: `x<y>` could be type args or relational
    // [$.type_arguments, $.relational_expression], -- unnecessary
    // Identifier could be type_identifier or expression
    [$._type_name, $._primary],
    [$._type_name, $._primary, $._function_name],
    // [$._type_name, $._primary, $.function_signature], -- covered by _function_name conflicts
    [$._primary, $._simple_formal_parameter],
    [$._primary, $._simple_formal_parameter, $.constant_pattern],
    // Constructor signature shares prefix with function_signature
    // [$.constructor_signature, $._formal_parameter_part], -- covered by _function_name conflicts
    // Declaration ambiguity with external/static
    [$.declaration, $._external_and_static],
    // Type ambiguities
    [$._type_not_function, $._type_not_void],
    [$._type_not_void_not_function],
    [$._type_not_void],
    [$._function_type_tail],
    [$._type_not_void_not_function, $._function_type_tail],
    [$.type, $._type_not_function],
    [$.type, $._function_type_tail, $._built_in_identifier],
    [$.type, $._type_not_function, $._function_type_tail, $._built_in_identifier],
    [$.type, $._function_type_tail],
    [$.type, $._type_not_function, $._function_type_tail],
    [$._type_not_function, $._function_type_tail, $._built_in_identifier],
    [$._type_not_void, $._function_type_tail, $._built_in_identifier],
    [$._type_not_function, $._type_not_void, $._function_type_tail, $._built_in_identifier],
    [$._type_not_function, $._function_type_tail],
    [$.type, $._type_not_function, $._type_not_void, $._function_type_tail, $._built_in_identifier],
    [$._type_not_void, $._function_type_tail],
    [$.function_type],
    // Var or type ambiguities
    [$._var_or_type],
    [$._var_or_type, $.function_signature],
    [$._var_or_type, $._function_formal_parameter],
    [$._final_const_var_or_type],
    [$._final_const_var_or_type, $.const_object_expression],
    // Expression ambiguities
    [$._expression],
    [$._primary, $.assignable_expression],
    [$._primary, $.labeled_statement],
    // Assignable-vs-wrapper: same chain reduces under either name; lookahead picks
    [$.assignable_expression, $.member_expression, $._unary_expression],
    [$.assignable_expression, $.index_expression, $._unary_expression],
    [$.assignable_expression, $.null_aware_member_expression, $._unary_expression],
    [$.assignable_expression, $.null_aware_index_expression, $._unary_expression],
    // Type name ambiguities
    [$._type_name],
    // [$._type_name, $._simple_formal_parameter], -- subsumed
    // [$._type_name, $._function_formal_parameter], -- subsumed
    // [$._type_name, $.function_signature], -- covered by _function_name conflicts
    // [$._type_name, $._primary, $.assignable_expression], -- subsumed
    // Parameter ambiguities
    [$._normal_formal_parameters],
    [$._normal_formal_parameter],
    [$.typed_identifier, $._var_or_type, $._function_formal_parameter],
    [$.record_type_field, $._function_formal_parameter, $._var_or_type],
    [$.record_type_field, $._var_or_type, $._final_var_or_type, $._function_formal_parameter],
    [$._var_or_type, $._final_var_or_type],
    [$._var_or_type, $._final_var_or_type, $._function_formal_parameter],
    [$._final_const_var_or_type, $._final_var_or_type],
    [$.record_type_field, $._final_var_or_type],
    [$._final_var_or_type],
    // Type parameter vs type name
    [$.type_parameter, $._type_name],
    // For loop parts ambiguity
    [$._var_or_type, $._for_loop_parts, $.pattern_variable_declaration],
    [$.pattern_variable_declaration, $._for_loop_parts, $._final_const_var_or_type],
    [$.pattern_variable_declaration, $._var_or_type],
    [$._final_const_var_or_type, $.pattern_variable_declaration],
    // Pattern ambiguities
    [$.set_or_map_literal, $.map_pattern],
    [$.list_literal, $.list_pattern],
    [$.static_member_shorthand, $.constant_pattern],
    // [$.constant_pattern, $._type_name], -- subsumed
    [$._primary, $.constant_pattern],
    [$._instantiation, $.constant_pattern],
    // [$._primary, $.constant_pattern, $._type_name], -- subsumed
    // [$._primary, $.constant_pattern, $._simple_formal_parameter], -- subsumed
    // [$._primary, $.constant_pattern, $._type_name, $._simple_formal_parameter], -- subsumed
    [$._literal, $.constant_pattern],
    [$.prefix_operator, $.constant_pattern],
    [$._parenthesized_pattern, $._pattern_field],
    [$.set_or_map_literal, $.constant_pattern],
    [$.list_literal, $.constant_pattern],
    [$.record_literal, $.constant_pattern],
    // Constructor tearoff vs primary
    [$.constructor_tearoff, $._identifier_or_new],
    // [$._simple_formal_parameter, $.assignable_expression], -- unnecessary
    // Declaration external
    [$.declaration, $.external],
    // Type arguments vs type parameters vs relational
    [$.type_arguments, $.relational_operator],
    // [$.relational_operator, $.type_parameters], -- unnecessary
    [$.type_arguments, $.relational_operator, $.type_parameters],
    // Generic call/instantiation vs comparisons (TS-style GLR fork on `<`)
    [$._unary_expression, $.instantiation_expression],
    [$._unary_expression, $.call_expression],
    // Record literal vs record field / record type
    [$._record_literal_no_const, $.record_field],
    [$.record_type, $._record_literal_no_const],
    [$.record_type, $._record_literal_no_const, $.record_pattern],
    [$._record_literal_no_const, $.record_pattern],
    [$.record_type, $.record_pattern],
    [$._record_literal_no_const, $.formal_parameter_list],
    // Block vs set_or_map literal
    [$.block, $.set_or_map_literal],
    // Empty switch block vs empty switch expression
    [$.switch_block, $.switch_expression],
    // Primary + constructor_param
    [$._primary, $.constructor_param],
    // Switch statement case
    [$.switch_statement_case],
    // Primary + function_formal_parameter + type_name
    [$._type_name, $._function_formal_parameter],
    [$._type_name, $.constant_pattern],
    [$._type_name, $._simple_formal_parameter],
    // [$._type_name, $._function_name], -- subsumed
    // [$._type_name, $._function_name, $._primary], -- subsumed
    // [$._function_name, $.constructor_signature], -- subsumed
    // Built-in identifier conflicts
    // Top-level wrapper rules competing with `_built_in_identifier` for the
    // `augment` lookahead. Each top-level form needs to keep itself alive when
    // a containing rule (class/mixin/etc.) is also a candidate.
    [$.function_declaration, $.getter_declaration, $.setter_declaration,
     $.external_function_declaration, $.external_getter_declaration, $.external_setter_declaration,
     $.top_level_variable_declaration, $._built_in_identifier],
    [$.function_declaration, $.getter_declaration, $.setter_declaration,
     $.external_function_declaration, $.external_getter_declaration, $.external_setter_declaration,
     $.top_level_variable_declaration, $.external_variable_declaration, $._built_in_identifier],
    [$.class_declaration, $.function_declaration, $.getter_declaration, $.setter_declaration,
     $.external_function_declaration, $.external_getter_declaration, $.external_setter_declaration,
     $.top_level_variable_declaration, $._built_in_identifier],
    [$.class_declaration, $.mixin_declaration, $.function_declaration, $.getter_declaration, $.setter_declaration,
     $.external_function_declaration, $.external_getter_declaration, $.external_setter_declaration,
     $.top_level_variable_declaration, $._built_in_identifier],
    [$.top_level_variable_declaration, $._built_in_identifier],
    [$.class_member, $.method_declaration, $._built_in_identifier],
    [$.type_alias, $._built_in_identifier],
    [$.function_signature, $.getter_signature, $._var_or_type],
    [$.function_signature, $.setter_signature, $._var_or_type],
    [$._final_const_var_or_type, $._built_in_identifier],
    [$._declared_identifier, $._built_in_identifier],
    [$.external, $._built_in_identifier],
    [$.declaration, $._built_in_identifier],
    [$.method_signature, $.declaration, $._built_in_identifier],
    [$.operator_signature, $._built_in_identifier],
    [$.function_signature, $._var_or_type, $.operator_signature],
    [$._function_formal_parameter, $._declared_identifier, $._built_in_identifier],
    [$.try_statement],
    [$._default_named_parameter, $._built_in_identifier],
    [$.library_name, $._built_in_identifier],
    [$.getter_signature, $._built_in_identifier],
    [$.setter_signature, $._built_in_identifier],
    [$._class_modifiers, $._mixin_class_modifiers, $._built_in_identifier],
    [$._class_modifiers, $._built_in_identifier],
    [$._mixin_class_modifiers, $._built_in_identifier],
    [$._mixin_class_modifiers, $.mixin_declaration, $._built_in_identifier],
    [$.mixin_declaration, $._built_in_identifier],
    [$._type_name, $._function_name],
    [$.extension_declaration, $._built_in_identifier],
    [$.extension_type_declaration, $._built_in_identifier],
    [$._function_formal_parameter, $._simple_formal_parameter, $._declared_identifier, $._built_in_identifier],
    [$._named_parameter_type, $._built_in_identifier],
    [$.declaration, $.external, $._built_in_identifier],
    [$.declaration, $._external_and_static, $._built_in_identifier],
    [$.factory_constructor_signature, $.redirecting_factory_constructor_signature, $._built_in_identifier],
    [$.redirecting_factory_constructor_signature, $._built_in_identifier],
    [$._function_name, $.constructor_signature],
    [$._type_name, $._primary, $.constant_pattern],
    [$._type_name, $._primary, $._function_formal_parameter],
    [$._type_name, $._primary, $._simple_formal_parameter, $.constant_pattern],
    [$._type_name, $.assignable_expression, $._primary],
    [$._type_not_void_not_function, $._function_type_tail, $._built_in_identifier],
    [$._function_type_tail, $._built_in_identifier],
    [$.factory_constructor_signature, $._built_in_identifier],
    [$.assignable_expression, $._simple_formal_parameter],
    [$._primary, $.super_formal_parameter],
    // this.identifier in initializer_list_entry vs redirection
    [$.initializer_list_entry, $._identifier_or_new],
  ],

  rules: {
    // ========================================================================
    // Top-level structure
    // ========================================================================

    source_file: ($) =>
      seq(
        optional($.script_tag),
        optional($.library_name),
        repeat($.import_or_export),
        repeat($.part_directive),
        repeat($.part_of_directive),
        repeat($._top_level_definition),
      ),

    _top_level_definition: ($) =>
      choice(
        $._declaration,
        $.function_declaration,
        $.getter_declaration,
        $.setter_declaration,
        $.external_function_declaration,
        $.external_getter_declaration,
        $.external_setter_declaration,
        $.top_level_variable_declaration,
        $.external_variable_declaration,
      ),

    // ========================================================================
    // Library structure (Phase 9)
    // ========================================================================

    script_tag: (_) => seq("#!", /[^\n]*/, "\n"),

    library_name: ($) =>
      choice(
        seq(
          optional($._metadata),
          "library",
          optional($.dotted_identifier_list),
          ";",
        ),
        seq(
          optional($._metadata),
          "library",
          "augment",
          field("uri", $.uri),
          ";",
        ),
      ),

    import_or_export: ($) =>
      prec(PREC.TYPE_IDENTIFIER, choice($.library_import, $.library_export)),

    library_import: ($) => seq(optional($._metadata), $.import_specification),

    import_specification: ($) =>
      choice(
        seq(
          "import",
          field("uri", $.configurable_uri),
          optional(seq(optional("deferred"), "as", field("alias", $.identifier))),
          repeat($.combinator),
          ";",
        ),
        prec(1, seq(
          "import",
          field("uri", $.uri),
          "deferred",
          "as",
          field("alias", $.identifier),
          repeat($.combinator),
          ";",
        )),
      ),

    library_export: ($) =>
      seq(
        optional($._metadata),
        "export",
        field("uri", $.configurable_uri),
        repeat($.combinator),
        ";",
      ),

    part_directive: ($) =>
      seq(optional($._metadata), "part", field("uri", $.uri), ";"),

    part_of_directive: ($) =>
      seq(
        optional($._metadata),
        "part",
        "of",
        choice($.dotted_identifier_list, $.uri),
        ";",
      ),

    uri: ($) => $.string_literal,

    configurable_uri: ($) => seq($.uri, repeat($.configuration_uri)),

    configuration_uri: ($) =>
      seq("if", "(", $.uri_test, ")", $.uri),

    uri_test: ($) =>
      seq($.dotted_identifier_list, optional(seq("==", $.string_literal))),

    combinator: ($) =>
      choice(
        seq("show", commaSep1($.identifier)),
        seq("hide", commaSep1($.identifier)),
      ),

    dotted_identifier_list: ($) => seq($.identifier, repeat(seq(".", $.identifier))),

    // ========================================================================
    // Metadata / Annotations (Phase 9)
    // ========================================================================

    _metadata: ($) => repeat1($._annotation),

    _annotation: ($) =>
      choice(
        alias($._annotation_with_args, $.annotation),
        alias($._annotation_no_args, $.annotation),
      ),

    _annotation_with_args: ($) =>
      seq(
        "@",
        field("name", choice($.identifier, $.qualified)),
        optional(seq($.type_arguments, optional(seq(".", $._identifier_or_new)))),
        $.annotation_arguments,
      ),

    annotation_arguments: ($) =>
      seq($.annotation_open_paren, optional($._argument_list), ")"),

    _annotation_no_args: ($) =>
      seq("@", field("name", choice($.identifier, $.qualified))),

    qualified: ($) =>
      choice(
        seq($._type_name, ".", $._identifier_or_new),
        seq($._type_name, ".", $._type_name, ".", $._identifier_or_new),
      ),

    // ========================================================================
    // Types (Phase 2)
    // ========================================================================

    // Public wrapper: every type-labeled position emits exactly one `type`
    // child. The scope-restricted variants below alias to `type` so consumers
    // see a uniform `(type ...)` regardless of which slot matched.
    type: ($) =>
      choice(
        nullable($.function_type),
        nullable($.record_type),
        $.void_type,
        nullable(seq($._type_name, optional($.type_arguments))),
        nullable("Function"),
      ),

    _type_not_function: ($) =>
      alias(
        choice(
          nullable(seq($._type_name, optional($.type_arguments))),
          nullable("Function"),
          nullable($.record_type),
          $.void_type,
        ),
        $.type,
      ),

    _type_not_void: ($) =>
      alias(
        choice(
          nullable($.function_type),
          nullable($.record_type),
          nullable(seq($._type_name, optional($.type_arguments))),
          nullable("Function"),
        ),
        $.type,
      ),

    _type_not_void_not_function: ($) =>
      alias(
        choice(
          nullable(seq($._type_name, optional($.type_arguments))),
          nullable("Function"),
        ),
        $.type,
      ),

    _type_name: ($) =>
      seq(
        alias($.identifier, $.type_identifier),
        optional(
          prec.right(PREC.TYPE_IDENTIFIER, seq(".", alias($.identifier, $.type_identifier))),
        ),
      ),

    void_type: (_) => "void",

    function_type: ($) =>
      choice(
        $._function_type_tails,
        seq($._type_not_function, $._function_type_tails),
      ),

    _function_type_tails: ($) => repeat1($._function_type_tail),

    _function_type_tail: ($) =>
      seq(
        "Function",
        optional($.type_parameters),
        optional($.parameter_type_list),
        optional("?"),
      ),

    parameter_type_list: ($) =>
      seq(
        "(",
        optional(
          choice(
            commaSep1TrailingComma($.normal_parameter_type),
            seq(
              commaSep1($.normal_parameter_type),
              ",",
              $.optional_parameter_types,
            ),
            $.optional_parameter_types,
          ),
        ),
        ")",
      ),

    normal_parameter_type: ($) =>
      seq(optional($._metadata), choice($.typed_identifier, $.type)),

    optional_parameter_types: ($) =>
      choice(
        $.optional_positional_parameter_types,
        $.named_parameter_types,
      ),

    optional_positional_parameter_types: ($) =>
      seq("[", commaSep1TrailingComma($.normal_parameter_type), "]"),

    named_parameter_types: ($) =>
      seq("{", commaSep1TrailingComma($._named_parameter_type), "}"),

    _named_parameter_type: ($) =>
      seq(optional($._metadata), optional("required"), $.typed_identifier),

    typed_identifier: ($) => seq(field("type", $.type), field("name", $.identifier)),

    type_arguments: ($) =>
      seq("<", commaSep1($.type), ">"),

    type_parameters: ($) =>
      seq("<", commaSep1($.type_parameter), ">"),

    type_parameter: ($) =>
      seq(
        optional($._metadata),
        optional(field("variance", $.variance_modifier)),
        field("name", alias($.identifier, $.type_identifier)),
        optional(seq("extends", field("bound", $._type_not_void))),
      ),

    variance_modifier: (_) => choice("in", "out", "inout"),

    record_type: ($) =>
      choice(
        seq("(", ")"),
        seq(
          "(",
          commaSep1($.record_type_field),
          ",",
          "{",
          commaSep1TrailingComma($.record_type_named_field),
          "}",
          ")",
        ),
        seq("(", commaSep1TrailingComma($.record_type_field), ")"),
        seq(
          "(",
          "{",
          commaSep1TrailingComma($.record_type_named_field),
          "}",
          ")",
        ),
      ),

    record_type_field: ($) =>
      seq(optional($._metadata), field("type", $.type), optional(field("name", $.identifier))),

    record_type_named_field: ($) =>
      seq(optional($._metadata), $.typed_identifier),

    // ========================================================================
    // Literals (Phase 3)
    // ========================================================================

    _literal: ($) =>
      choice(
        $.null_literal,
        $.true,
        $.false,
        $.decimal_integer_literal,
        $.hex_integer_literal,
        $.decimal_floating_point_literal,
        $.string_literal,
        $.symbol_literal,
        $.list_literal,
        $.set_or_map_literal,
        $.record_literal,
      ),

    null_literal: (_) => "null",

    true: (_) => "true",
    false: (_) => "false",

    // Numeric literals (with digit separator support for Dart 3.6)
    decimal_integer_literal: (_) =>
      token(sep1(/[0-9]+/, /_+/)),

    hex_integer_literal: (_) =>
      token(seq(choice("0x", "0X"), sep1(/[A-Fa-f0-9]+/, /_+/))),

    decimal_floating_point_literal: (_) =>
      token(
        choice(
          seq(
            sep1(/[0-9]+/, /_+/),
            ".",
            sep1(/[0-9]+/, /_+/),
            optional(seq(/[eE]/, optional(choice("-", "+")), sep1(/[0-9]+/, /_+/))),
          ),
          seq(
            ".",
            sep1(/[0-9]+/, /_+/),
            optional(seq(/[eE]/, optional(choice("-", "+")), sep1(/[0-9]+/, /_+/))),
          ),
          seq(sep1(/[0-9]+/, /_+/), /[eE]/, optional(choice("-", "+")), sep1(/[0-9]+/, /_+/)),
        ),
      ),

    // --- Strings ---

    string_literal: ($) =>
      repeat1(
        choice(
          $.string_literal_double_quotes,
          $.string_literal_single_quotes,
          $.string_literal_double_quotes_multiple,
          $.string_literal_single_quotes_multiple,
          $.raw_string_literal_double_quotes,
          $.raw_string_literal_single_quotes,
          $.raw_string_literal_double_quotes_multiple,
          $.raw_string_literal_single_quotes_multiple,
        ),
      ),

    string_literal_double_quotes: ($) =>
      seq(
        '"',
        repeat(
          choice(
            $.template_chars_double_single,
            "'",
            $.escape_sequence,
            $._sub_string_test,
            $.template_substitution,
          ),
        ),
        '"',
      ),

    string_literal_single_quotes: ($) =>
      seq(
        "'",
        repeat(
          choice(
            $.template_chars_single_single,
            '"',
            $.escape_sequence,
            $._sub_string_test,
            $.template_substitution,
          ),
        ),
        "'",
      ),

    string_literal_double_quotes_multiple: ($) =>
      prec.left(
        seq(
          '"""',
          repeat(
            choice(
              $.template_chars_double,
              "'",
              '"',
              $.escape_sequence,
              $._sub_string_test,
              $.template_substitution,
            ),
          ),
          '"""',
        ),
      ),

    string_literal_single_quotes_multiple: ($) =>
      prec.left(
        seq(
          "'''",
          repeat(
            choice(
              $.template_chars_single,
              '"',
              "'",
              $.escape_sequence,
              $._sub_string_test,
              $.template_substitution,
            ),
          ),
          "'''",
        ),
      ),

    raw_string_literal_double_quotes: ($) =>
      seq(
        'r"',
        repeat(
          choice(
            $.template_chars_double_single,
            "'",
            $.template_chars_raw_slash,
            $._unused_escape_sequence,
            $._sub_string_test,
            "$",
          ),
        ),
        '"',
      ),

    raw_string_literal_single_quotes: ($) =>
      seq(
        "r'",
        repeat(
          choice(
            $.template_chars_single_single,
            '"',
            $.template_chars_raw_slash,
            $._unused_escape_sequence,
            $._sub_string_test,
            "$",
          ),
        ),
        "'",
      ),

    raw_string_literal_double_quotes_multiple: ($) =>
      prec.left(
        seq(
          'r"""',
          repeat(
            choice(
              $.template_chars_double,
              "'",
              $.template_chars_raw_slash,
              '"',
              $._unused_escape_sequence,
              $._sub_string_test,
              "$",
            ),
          ),
          '"""',
        ),
      ),

    raw_string_literal_single_quotes_multiple: ($) =>
      prec.left(
        seq(
          "r'''",
          repeat(
            choice(
              $.template_chars_single,
              '"',
              "'",
              $.template_chars_raw_slash,
              $._unused_escape_sequence,
              $._sub_string_test,
              "$",
            ),
          ),
          "'''",
        ),
      ),

    template_substitution: ($) =>
      seq(
        "$",
        choice(
          seq("{", $._expression, "}"),
          $.identifier_dollar_escaped,
        ),
      ),

    _sub_string_test: (_) => seq("$", /[^a-zA-Z_{]/),

    identifier_dollar_escaped: (_) => /([a-zA-Z_]|(\\\$))([\w]|(\\\$))*/,

    _unused_escape_sequence: (_) =>
      token.immediate(
        seq(
          "\\",
          choice(
            /[^xu0-7]/,
            /[0-7]{1,3}/,
            /x[0-9a-fA-F]{2}/,
            /u[0-9a-fA-F]{4}/,
            /u\{[0-9a-fA-F]+\}/,
          ),
        ),
      ),

    escape_sequence: ($) => $._unused_escape_sequence,

    // --- Symbol literals ---

    symbol_literal: ($) =>
      prec.right(
        seq(
          "#",
          choice(
            seq($.identifier, repeat(seq(".", $.identifier))),
            "~",
            $.binary_operator,
            "[]",
            "[]=",
          ),
        ),
      ),

    // --- Collection literals ---

    list_literal: ($) =>
      seq(
        optional("const"),
        optional($.type_arguments),
        "[",
        commaSepTrailingComma($._element),
        "]",
      ),

    set_or_map_literal: ($) =>
      seq(
        optional("const"),
        optional($.type_arguments),
        "{",
        commaSepTrailingComma($._element),
        "}",
      ),

    _element: ($) =>
      choice(
        $._expression,
        $.pair,
        $.spread_element,
        $.null_aware_element,
        $.null_aware_pair,
        $.if_element,
        $.for_element,
      ),

    null_aware_element: ($) => seq("?", $._expression),

    null_aware_pair: ($) =>
      seq("?", field("key", $._expression), ":", field("value", choice($._expression, $.null_aware_element))),

    pair: ($) =>
      seq(
        field("key", $._expression),
        ":",
        field("value", choice($._expression, $.null_aware_element)),
      ),

    spread_element: ($) => seq(choice("...", "...?"), field("value", $._expression)),

    if_element: ($) =>
      prec.right(
        seq(
          "if",
          "(",
          field("condition", $._expression),
          optional(seq("case", $._guarded_pattern)),
          ")",
          field("consequence", $._element),
          optional(seq("else", field("alternative", $._element))),
        ),
      ),

    for_element: ($) =>
      seq(
        optional("await"),
        "for",
        "(", $._for_loop_parts, ")",
        field("body", $._element),
      ),

    // --- Record literals ---

    record_literal: ($) =>
      seq(optional("const"), $._record_literal_no_const),

    _record_literal_no_const: ($) =>
      choice(
        seq("(", ")"),
        seq(
          "(",
          choice(
            seq($.label, $._expression, optional(",")),
            seq($._expression, ","),
            seq($.record_field, repeat1(seq(",", $.record_field)), optional(",")),
          ),
          ")",
        ),
      ),

    record_field: ($) => seq(optional($.label), $._expression),

    // ========================================================================
    // Expressions (Phase 4)
    // ========================================================================

    _expression: ($) =>
      choice(
        $.pattern_assignment,
        $.assignment_expression,
        $.throw_expression,
        seq($._real_expression, repeat($.cascade_section)),
      ),

    _expression_without_cascade: ($) =>
      choice(
        $.assignment_expression,
        $._real_expression,
        $.throw_expression,
      ),

    _real_expression: ($) =>
      choice(
        $.conditional_expression,
        $.logical_or_expression,
        $.if_null_expression,
        $.logical_and_expression,
        $.equality_expression,
        $.relational_expression,
        $.bitwise_or_expression,
        $.bitwise_xor_expression,
        $.bitwise_and_expression,
        $.shift_expression,
        $.additive_expression,
        $.multiplicative_expression,
        $.type_cast_expression,
        $.type_test_expression,
        $._unary_expression,
      ),

    // --- Throw ---

    throw_expression: ($) => seq("throw", field("value", $._expression)),

    // --- Assignment ---

    assignment_expression: ($) =>
      prec.right(
        PREC.ASSIGNMENT,
        seq(
          field("left", $.assignable_expression),
          field("operator", $._assignment_operator),
          field("right", $._expression),
        ),
      ),

    _assignment_operator: (_) =>
      choice(
        "=", "+=", "-=", "*=", "/=", "%=", "~/=",
        "<<=", ">>=", ">>>=", "&=", "^=", "|=", "??=",
      ),

    assignable_expression: ($) =>
      choice(
        $.identifier,
        seq(
          field("object", $._postfix_expression),
          ".",
          field("property", $.identifier),
        ),
        seq(
          field("object", $._postfix_expression),
          "[",
          field("index", $._expression),
          "]",
        ),
        seq(
          field("object", $._postfix_expression),
          "?.",
          field("property", $.identifier),
        ),
        seq(
          field("object", $._postfix_expression),
          "?",
          "[",
          field("index", $._expression),
          "]",
        ),
      ),

    // --- Conditional ---

    conditional_expression: ($) =>
      prec.left(
        PREC.CONDITIONAL,
        seq(
          $._real_expression,
          "?",
          field("consequence", $._expression_without_cascade),
          ":",
          field("alternative", $._expression_without_cascade),
        ),
      ),

    // --- If-null ---

    if_null_expression: ($) =>
      prec.left(
        PREC.IF_NULL,
        seq($._real_expression, repeat1(seq("??", $._real_expression))),
      ),

    // --- Logical ---

    logical_or_expression: ($) =>
      prec.left(PREC.LOGICAL_OR, seq($._real_expression, "||", $._real_expression)),

    logical_and_expression: ($) =>
      prec.left(PREC.LOGICAL_AND, seq($._real_expression, "&&", $._real_expression)),

    // --- Equality ---

    equality_expression: ($) =>
      prec.left(
        PREC.EQUALITY,
        choice(
          seq($._real_expression, choice("==", "!="), $._real_expression),
          seq("super", choice("==", "!="), $._real_expression),
        ),
      ),

    // --- Relational ---

    relational_expression: ($) =>
      prec.left(
        PREC.RELATIONAL,
        choice(
          seq($._real_expression, $.relational_operator, $._real_expression),
          seq("super", $.relational_operator, $._real_expression),
        ),
      ),

    relational_operator: (_) => choice("<", ">", "<=", ">="),

    // --- Type test/cast ---

    type_test_expression: ($) =>
      prec(PREC.RELATIONAL, seq($._real_expression, $.type_test)),

    type_test: ($) => seq($.is_operator, $._type_not_void),

    is_operator: (_) => seq("is", optional("!")),

    type_cast_expression: ($) =>
      prec.left(PREC.RELATIONAL, seq($._real_expression, $.type_cast)),

    type_cast: ($) => seq("as", $._type_not_void),

    // --- Bitwise ---

    bitwise_or_expression: ($) =>
      prec.left(
        PREC.BITWISE_OR,
        choice(
          seq($._real_expression, "|", $._real_expression),
          seq("super", repeat1(seq("|", $._real_expression))),
        ),
      ),

    bitwise_xor_expression: ($) =>
      prec.left(
        PREC.BITWISE_XOR,
        choice(
          seq($._real_expression, "^", $._real_expression),
          seq("super", repeat1(seq("^", $._real_expression))),
        ),
      ),

    bitwise_and_expression: ($) =>
      prec.left(
        PREC.BITWISE_AND,
        choice(
          seq($._real_expression, "&", $._real_expression),
          seq("super", repeat1(seq("&", $._real_expression))),
        ),
      ),

    // --- Shift ---

    shift_expression: ($) =>
      prec.left(
        PREC.SHIFT,
        choice(
          seq($._real_expression, $._shift_operator, $._real_expression),
          seq("super", repeat1(seq($._shift_operator, $._real_expression))),
        ),
      ),

    _shift_operator: (_) => choice("<<", ">>>", ">>"),

    // --- Additive ---

    additive_expression: ($) =>
      prec.left(
        PREC.ADDITIVE,
        choice(
          seq($._real_expression, $._additive_operator, $._real_expression),
          seq("super", repeat1(seq($._additive_operator, $._real_expression))),
        ),
      ),

    _additive_operator: (_) => choice("+", "-"),

    // --- Multiplicative ---

    multiplicative_expression: ($) =>
      prec.left(
        PREC.MULTIPLICATIVE,
        choice(
          seq(
            $._unary_expression,
            repeat1(seq(choice("*", "/", "%", "~/"), $._unary_expression)),
          ),
          seq("super", repeat1(seq(choice("*", "/", "%", "~/"), $._unary_expression))),
        ),
      ),

    // --- Unary ---

    _unary_expression: ($) =>
      prec(
        PREC.UNARY_PREFIX,
        choice($._postfix_expression, $.unary_expression),
      ),

    unary_expression: ($) =>
      prec(
        PREC.UNARY_PREFIX,
        choice(
          seq($.prefix_operator, $._unary_expression),
          seq($.negate_operator, $._unary_expression),
          $.await_expression,
          seq(choice("-", "~"), "super"),
          seq(choice("++", "--"), $.assignable_expression),
        ),
      ),

    prefix_operator: (_) => choice("-", "~"),

    negate_operator: (_) => "!",

    await_expression: ($) => seq("await", $._unary_expression),

    // --- Postfix ---

    _postfix_expression: ($) =>
      choice(
        $._primary,
        $.call_expression,
        $.member_expression,
        $.index_expression,
        $.null_aware_member_expression,
        $.null_aware_index_expression,
        $.null_assertion_expression,
        $.instantiation_expression,
        $.postfix_expression,
      ),

    postfix_expression: ($) =>
      prec.right(
        seq(field("argument", $.assignable_expression), choice("++", "--")),
      ),

    call_expression: ($) =>
      prec.dynamic(1, prec("call",
        seq(
          field("function", $._postfix_expression),
          field("arguments", $.arguments),
        ),
      )),

    // Named prec required so the `<` ambiguity (call/instantiation/relational)
    // falls through to GLR conflict declarations rather than being auto-resolved.
    instantiation_expression: ($) =>
      prec("instantiation",
        seq(
          field("function", $._postfix_expression),
          field("type_arguments", $.type_arguments),
        ),
      ),

    ...postfixChainWrappers("_postfix_expression", ""),

    // --- Cascade ---

    cascade_section: ($) =>
      prec.left(
        PREC.CASCADE,
        seq(
          choice("..", "?.."),
          $._cascade_postfix_expression,
          optional(seq($._assignment_operator, $._expression_without_cascade)),
        ),
      ),

    _cascade_postfix_expression: ($) =>
      choice(
        $.cascade_selector,
        $.cascade_call_expression,
        $.cascade_member_expression,
        $.cascade_index_expression,
        $.cascade_null_aware_member_expression,
        $.cascade_null_aware_index_expression,
        $.cascade_null_assertion_expression,
      ),

    cascade_selector: ($) =>
      choice(
        seq(optional("?"), "[", $._expression, "]"),
        $.identifier,
      ),

    // cascade_call_expression keeps `optional(type_arguments)` inline because
    // there's no `cascade_instantiation_expression` - bare `..foo<int>` (cascade
    // tearoff) isn't valid Dart, so the `<` fork that forced the call/instantiation
    // split on the non-cascade side doesn't apply here.
    //
    // The simple `..foo()` / `..foo<T>()` form exposes the method name as
    // `property:` directly (mirroring `member_expression` and
    // `cascade_member_expression`), so consumers don't need an extra hop
    // through `cascade_selector`. Chained forms (e.g. `..foo().bar()`) keep
    // the `function:` field for the more complex tail.
    cascade_call_expression: ($) =>
      prec.left(
        PREC.UNARY_POSTFIX,
        choice(
          prec.dynamic(1, seq(
            field("property", $.identifier),
            optional(field("type_arguments", $.type_arguments)),
            field("arguments", $.arguments),
          )),
          seq(
            field("function", $._cascade_postfix_expression),
            optional(field("type_arguments", $.type_arguments)),
            field("arguments", $.arguments),
          ),
        ),
      ),

    ...postfixChainWrappers("_cascade_postfix_expression", "cascade_"),

    // --- Primary expressions ---

    _primary: ($) =>
      choice(
        $._literal,
        $.identifier,
        $.function_expression,
        $._instantiation,
        $.parenthesized_expression,
        "this",
        "super",
        $.constructor_tearoff,
        $.switch_expression,
        $.static_member_shorthand,
      ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    function_expression: ($) =>
      seq(
        field("parameters", $._formal_parameter_part),
        field("body", $.function_expression_body),
      ),

    function_expression_body: ($) =>
      choice(
        seq(optional("async"), "=>", $._expression),
        seq(optional(choice("async", "async*", "sync*")), $.block),
      ),

    // Hidden supertype: any expression that constructs an instance. Lets
    // queries address the three forms (new/const/implicit) uniformly via
    // (_instantiation) without alternation. Does not add an AST layer.
    _instantiation: ($) =>
      choice($.new_expression, $.const_object_expression, $.constructor_invocation),

    new_expression: ($) =>
      seq("new", field("type", $._type_not_void), optional(seq(".", field("constructor", $.identifier))), field("arguments", $.arguments)),

    const_object_expression: ($) =>
      seq("const", field("type", $._type_not_void), optional(seq(".", field("constructor", $.identifier))), field("arguments", $.arguments)),

    constructor_invocation: ($) =>
      prec.right(
        choice(
          seq(field("type", $._type_name), $.type_arguments, ".", field("constructor", $.identifier), field("arguments", $.arguments)),
          seq(field("type", $._type_name), ".", "new", field("arguments", $.arguments)),
        ),
      ),

    constructor_tearoff: ($) =>
      prec.right(
        seq($._type_name, optional($.type_arguments), ".", "new"),
      ),

    // Dot shorthand syntax (Dart 3.10+)
    static_member_shorthand: ($) =>
      prec.dynamic(-1,
        choice(
          seq(".", $.identifier),
          seq(".", "new"),
          seq("const", ".", choice($.identifier, "new"), $.arguments),
        ),
      ),

    // --- Arguments ---

    arguments: ($) => seq("(", optional($._argument_list), ")"),

    _argument_list: ($) =>
      prec.right(commaSep1TrailingComma(choice($.named_argument, $._expression))),

    named_argument: ($) => seq($.label, $._expression),

    // ========================================================================
    // Formal Parameters (Phase 5)
    // ========================================================================

    _formal_parameter_part: ($) =>
      seq(optional($.type_parameters), $.formal_parameter_list),

    formal_parameter_list: ($) =>
      choice(
        seq("(", ")"),
        seq("(", $._normal_formal_parameters, optional(","), ")"),
        seq(
          "(",
          $._normal_formal_parameters,
          ",",
          $.optional_formal_parameters,
          ")",
        ),
        seq("(", $.optional_formal_parameters, ")"),
      ),

    _normal_formal_parameters: ($) => commaSep1($.formal_parameter),

    formal_parameter: ($) => $._normal_formal_parameter,

    optional_formal_parameters: ($) =>
      choice(
        seq("[", commaSep1TrailingComma($._default_formal_parameter), "]"),
        seq("{", commaSep1TrailingComma($._default_named_parameter), "}"),
      ),

    _default_formal_parameter: ($) =>
      seq($.formal_parameter, optional(seq("=", $._expression))),

    _default_named_parameter: ($) =>
      seq(
        optional($._metadata),
        optional("required"),
        $.formal_parameter,
        optional(seq(choice("=", ":"), $._expression)),
      ),

    _normal_formal_parameter: ($) =>
      seq(
        optional($._metadata),
        choice(
          $._function_formal_parameter,
          $._simple_formal_parameter,
          $.constructor_param,
          $.super_formal_parameter,
        ),
      ),

    _function_formal_parameter: ($) =>
      seq(
        optional("covariant"),
        optional($.type),
        $.identifier,
        $._formal_parameter_part,
        optional("?"),
      ),

    _simple_formal_parameter: ($) =>
      choice(
        $._declared_identifier,
        seq(optional("covariant"), $.identifier),
      ),

    _declared_identifier: ($) =>
      seq(
        optional("covariant"),
        $._final_const_var_or_type,
        field("name", $.identifier),
      ),

    constructor_param: ($) =>
      seq(
        optional($._final_const_var_or_type),
        "this",
        ".",
        $.identifier,
        optional(seq($._formal_parameter_part, optional("?"))),
      ),

    super_formal_parameter: ($) =>
      seq(
        optional($._final_const_var_or_type),
        "super",
        ".",
        $.identifier,
        optional(seq($._formal_parameter_part, optional("?"))),
      ),

    // ========================================================================
    // Statements (Phase 6)
    // ========================================================================

    _statement: ($) =>
      choice(
        $.block,
        prec.dynamic(1, $.local_function_declaration),
        prec.dynamic(2, $.local_variable_declaration),
        $.for_statement,
        $.while_statement,
        $.do_statement,
        $.switch_statement,
        $.if_statement,
        $.try_statement,
        $.break_statement,
        $.continue_statement,
        $.rethrow_statement,
        $.return_statement,
        $.yield_statement,
        $.yield_each_statement,
        $.expression_statement,
        $.assert_statement,
        $.labeled_statement,
        $.empty_statement,
      ),

    empty_statement: (_) => ";",

    block: ($) => seq("{", repeat($._statement), "}"),

    expression_statement: ($) => seq($._expression, ";"),

    local_variable_declaration: ($) =>
      choice(
        seq(
          optional($._metadata),
          $.initialized_variable_definition,
          ";",
        ),
        seq(
          optional($._metadata),
          $.pattern_variable_declaration,
          ";",
        ),
      ),

    initialized_variable_definition: ($) =>
      seq(
        $._declared_identifier,
        optional(seq("=", field("value", $._expression))),
        repeat(seq(",", $.initialized_identifier)),
      ),

    local_function_declaration: ($) =>
      seq(
        optional($._metadata),
        $.function_signature,
        $.function_body,
      ),

    labeled_statement: ($) => seq($.identifier, ":", $._statement),

    // --- Control flow ---

    if_statement: ($) =>
      prec.right(
        seq(
          "if",
          "(",
          $._expression,
          optional(seq("case", $._guarded_pattern)),
          ")",
          field("consequence", $._statement),
          optional(seq("else", field("alternative", $._statement))),
        ),
      ),

    for_statement: ($) =>
      seq(
        optional("await"),
        "for",
        "(", $._for_loop_parts, ")",
        field("body", $._statement),
      ),

    _for_loop_parts: ($) =>
      choice(
        // for-in loop
        seq(
          optional($._metadata),
          choice($._declared_identifier, $.identifier),
          "in",
          field("value", $._expression),
        ),
        // C-style for loop
        seq(
          optional(
            choice(
              field("init", $.local_variable_declaration),
              seq(commaSep(field("init", $._expression)), ";"),
            ),
          ),
          field("condition", optional($._expression)),
          ";",
          commaSep(field("update", $._expression)),
        ),
        // Pattern for-in loop (Dart 3.0)
        seq(
          optional($._metadata),
          choice("final", "var"),
          $._outer_pattern,
          "in",
          field("value", $._expression),
        ),
      ),

    while_statement: ($) =>
      seq(
        "while",
        field("condition", $.parenthesized_expression),
        field("body", $._statement),
      ),

    do_statement: ($) =>
      seq(
        "do",
        field("body", $._statement),
        "while",
        field("condition", $.parenthesized_expression),
        ";",
      ),

    // --- Switch ---

    switch_statement: ($) =>
      seq(
        "switch",
        field("condition", $.parenthesized_expression),
        field("body", $.switch_block),
      ),

    switch_block: ($) =>
      seq(
        "{",
        repeat($.switch_statement_case),
        optional($.switch_statement_default),
        "}",
      ),

    switch_statement_case: ($) =>
      seq(
        repeat($.label),
        "case",
        $._guarded_pattern,
        ":",
        repeat($._statement),
      ),

    switch_statement_default: ($) =>
      seq(repeat($.label), "default", ":", repeat($._statement)),

    switch_expression: ($) =>
      seq(
        "switch",
        field("condition", $.parenthesized_expression),
        field("body", seq("{", commaSepTrailingComma($.switch_expression_case), "}")),
      ),

    switch_expression_case: ($) =>
      seq($._guarded_pattern, "=>", $._expression),

    // --- Exception handling ---

    try_statement: ($) =>
      seq(
        "try",
        field("body", $.block),
        optional(
          choice(
            $.finally_clause,
            seq(repeat1($._on_part), optional($.finally_clause)),
          ),
        ),
      ),

    _on_part: ($) =>
      choice(
        seq($.catch_clause, $.block),
        seq("on", $._type_not_void, optional($.catch_clause), $.block),
      ),

    catch_clause: ($) =>
      seq("catch", "(", field("exception", $.identifier), optional(seq(",", field("stack_trace", $.identifier))), ")"),

    finally_clause: ($) => seq("finally", $.block),

    // --- Jump statements ---

    break_statement: ($) => seq("break", optional($.identifier), ";"),

    continue_statement: ($) => seq("continue", optional($.identifier), ";"),

    rethrow_statement: (_) => seq("rethrow", ";"),

    return_statement: ($) => seq("return", optional($._expression), ";"),

    yield_statement: ($) => seq("yield", $._expression, ";"),

    yield_each_statement: ($) => seq("yield", "*", $._expression, ";"),

    // --- Assert ---

    assert_statement: ($) => seq($.assertion, ";"),

    assertion: ($) =>
      seq("assert", "(", $._expression, optional(seq(",", $._expression)), optional(","), ")"),

    // ========================================================================
    // Function signatures and bodies (Phase 7)
    // ========================================================================

    function_signature: ($) =>
      seq(
        optional(field("return_type", $.type)),
        field("name", $._function_name),
        field("parameters", $._formal_parameter_part),
      ),

    // All built-in identifiers can be used as function/method names
    _function_name: ($) => $.identifier,

    getter_signature: ($) =>
      seq(optional(field("return_type", $.type)), "get", field("name", $.identifier)),

    setter_signature: ($) =>
      seq(optional(field("return_type", $.type)), "set", field("name", $.identifier), field("parameters", $._formal_parameter_part)),

    function_body: ($) =>
      choice(
        seq(optional("async"), "=>", $._expression, ";"),
        seq(optional(choice("async", "async*", "sync*")), $.block),
        $.native,
      ),

    native: ($) => seq("native", optional($.string_literal), ";"),

    // ========================================================================
    // Declarations and class members (Phase 7)
    // ========================================================================

    _declaration: ($) =>
      prec(
        1,
        choice(
          $.class_declaration,
          $.mixin_declaration,
          $.extension_declaration,
          $.extension_type_declaration,
          $.enum_declaration,
          $.type_alias,
        ),
      ),

    // --- Type helpers ---

    _final_const_var_or_type: ($) =>
      choice(
        seq(optional("late"), "final", optional($.type)),
        seq("const", optional($.type)),
        seq(optional("late"), $._var_or_type),
      ),

    _var_or_type: ($) =>
      choice($.type, seq("var", optional($.type))),

    _final_var_or_type: ($) =>
      choice(
        "var",
        "final",
        seq(optional("final"), $.type),
      ),

    // --- Class members ---

    method_signature: ($) =>
      choice(
        seq($.constructor_signature, optional($.initializers)),
        $.factory_constructor_signature,
        seq(optional("static"), choice($.function_signature, $.getter_signature, $.setter_signature)),
        $.operator_signature,
      ),

    declaration: ($) =>
      choice(
        seq(
          $.constant_constructor_signature,
          optional(choice($.redirection, $.initializers)),
        ),
        seq(
          $.constructor_signature,
          optional(choice($.redirection, $.initializers)),
        ),
        seq($.external, optional("const"), $.factory_constructor_signature),
        seq("external", $.constant_constructor_signature),
        $.redirecting_factory_constructor_signature,
        seq("external", $.constructor_signature),
        seq(optional($._external_and_static), $.getter_signature),
        seq(optional($._external_and_static), $.setter_signature),
        seq(optional($.external), $.operator_signature),
        seq(optional($._external_and_static), $.function_signature),
        seq("static", $.function_signature),
        // Static field declarations
        seq(
          "static",
          choice(
            seq(
              choice("final", "const"),
              optional($.type),
              $.static_final_declaration_list,
            ),
            seq(
              "late",
              choice(
                seq("final", optional($.type), $.initialized_identifier_list),
                seq($._var_or_type, $.initialized_identifier_list),
              ),
            ),
            seq($._var_or_type, $.initialized_identifier_list),
          ),
        ),
        // Covariant field declarations
        seq(
          "covariant",
          choice(
            seq(
              "late",
              choice(
                seq("final", optional($.type), $.identifier_list),
                seq($._var_or_type, $.initialized_identifier_list),
              ),
            ),
            seq($._var_or_type, $.initialized_identifier_list),
          ),
        ),
        // Instance field declarations
        seq(optional("late"), "final", optional($.type), $.initialized_identifier_list),
        seq(optional("late"), $._var_or_type, $.initialized_identifier_list),
        seq("const", optional($.type), $.static_final_declaration_list),
        // Abstract field declarations
        seq("abstract", "final", optional($.type), $.identifier_list),
        seq("abstract", optional($.type), $.identifier_list),
        seq("abstract", "covariant", "final", optional($.type), $.identifier_list),
        seq("abstract", "covariant", optional($.type), $.identifier_list),
        // External field declarations
        seq($.external, optional("static"), optional("covariant"), optional(choice("final", "var")), optional($.type), $.identifier_list),
      ),

    external: (_) => "external",

    _external_and_static: ($) => seq($.external, optional("static")),

    // --- Constructors ---

    constructor_signature: ($) =>
      seq(
        field("name", seq($.identifier, optional(seq(".", $._identifier_or_new)))),
        field("parameters", $.formal_parameter_list),
      ),

    constant_constructor_signature: ($) =>
      seq(
        "const",
        field("name", seq($.identifier, optional(seq(".", $._identifier_or_new)))),
        field("parameters", $.formal_parameter_list),
      ),

    factory_constructor_signature: ($) =>
      seq(
        "factory",
        field("name", seq($.identifier, optional(seq(".", $._identifier_or_new)))),
        field("parameters", $.formal_parameter_list),
      ),

    redirecting_factory_constructor_signature: ($) =>
      seq(
        optional("const"),
        "factory",
        field("name", seq($.identifier, optional(seq(".", $._identifier_or_new)))),
        field("parameters", $.formal_parameter_list),
        "=",
        field("target", $._type_not_void),
        optional(seq(".", field("target_constructor", $.identifier))),
      ),

    redirection: ($) =>
      seq(":", "this", optional(seq(".", $._identifier_or_new)), $.arguments),

    initializers: ($) => seq(":", commaSep1($.initializer_list_entry)),

    initializer_list_entry: ($) =>
      choice(
        seq("super", $.arguments),
        seq("super", ".", choice($.identifier, "new"), $.arguments),
        seq("this", ".", choice($.identifier, "new"), $.arguments),
        $.field_initializer,
        $.assertion,
      ),

    field_initializer: ($) =>
      seq(
        optional(seq("this", ".")),
        field("name", $.identifier),
        "=",
        field("value", $._expression),
      ),

    // --- Operators ---

    operator_signature: ($) =>
      seq(
        optional(field("return_type", $.type)),
        "operator",
        field("operator", choice("~", $.binary_operator, "[]", "[]=")),
        field("parameters", $.formal_parameter_list),
      ),

    binary_operator: ($) =>
      choice(
        choice("*", "/", "%", "~/"),
        $._additive_operator,
        $._shift_operator,
        $.relational_operator,
        "==",
        "|",
        "^",
        "&",
      ),

    // --- Field helpers ---

    static_final_declaration_list: ($) =>
      commaSep1($.static_final_declaration),

    static_final_declaration: ($) =>
      seq(field("name", $.identifier), optional(seq("=", field("value", $._expression)))),

    initialized_identifier_list: ($) =>
      commaSep1($.initialized_identifier),

    initialized_identifier: ($) =>
      seq(field("name", $.identifier), optional(seq("=", field("value", $._expression)))),

    identifier_list: ($) => commaSep1($.identifier),

    // ========================================================================
    // Top-level declarations (Phase 8)
    // ========================================================================

    // --- Classes ---

    class_declaration: ($) =>
      choice(
        seq(
          optional($._metadata),
          optional("augment"),
          choice($._class_modifiers, $._mixin_class_modifiers),
          field("name", $.identifier),
          optional(field("type_parameters", $.type_parameters)),
          optional(field("superclass", $.superclass)),
          optional(field("interfaces", $.interfaces)),
          field("body", $.class_body),
        ),
        seq(
          optional($._metadata),
          choice($._class_modifiers, $._mixin_class_modifiers),
          $.mixin_application_class,
        ),
      ),

    _class_modifiers: ($) =>
      seq(
        choice(
          "sealed",
          seq(
            optional("abstract"),
            optional(choice("base", "interface", "final", "inline")),
          ),
        ),
        "class",
      ),

    _mixin_class_modifiers: ($) =>
      seq(optional("abstract"), optional("base"), "mixin", "class"),

    class_body: ($) =>
      seq(
        "{",
        repeat($.class_member),
        "}",
      ),

    class_member: ($) =>
      choice(
        seq(optional($._metadata), optional("augment"), $.declaration, ";"),
        $.method_declaration,
      ),

    method_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        field("signature", $.method_signature),
        field("body", $.function_body),
      ),

    superclass: ($) =>
      choice(
        seq("extends", field("type", $._type_not_void), optional($.mixins)),
        $.mixins,
      ),

    mixins: ($) => seq("with", commaSep1($._type_not_void)),

    interfaces: ($) => seq("implements", commaSep1($._type_not_void)),

    mixin_application_class: ($) =>
      seq(
        $.identifier,
        optional($.type_parameters),
        "=",
        $.mixin_application,
        ";",
      ),

    mixin_application: ($) =>
      seq($._type_not_void, $.mixins, optional($.interfaces)),

    // --- Mixins ---

    mixin_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        optional("base"),
        "mixin",
        field("name", $.identifier),
        optional(field("type_parameters", $.type_parameters)),
        optional(seq("on", commaSep1($._type_not_void))),
        optional(field("interfaces", $.interfaces)),
        field("body", $.class_body),
      ),

    // --- Extensions ---

    extension_declaration: ($) =>
      choice(
        seq(
          optional($._metadata),
          "extension",
          optional(field("name", $.identifier)),
          optional(field("type_parameters", $.type_parameters)),
          "on",
          field("class", $.type),
          field("body", $.extension_body),
        ),
        seq(
          optional($._metadata),
          "augment",
          "extension",
          field("name", $.identifier),
          optional(field("type_parameters", $.type_parameters)),
          field("body", $.extension_body),
        ),
      ),

    extension_body: ($) =>
      seq(
        "{",
        repeat($.class_member),
        "}",
      ),

    // --- Extension types (Dart 3.3) ---

    extension_type_declaration: ($) =>
      choice(
        seq(
          optional($._metadata),
          "extension",
          "type",
          optional("const"),
          field("name", $.extension_type_name),
          field("representation", $.extension_type_representation),
          optional(seq("implements", commaSep1($.type))),
          field("body", $.class_body),
        ),
        seq(
          optional($._metadata),
          "augment",
          "extension",
          "type",
          field("name", $.identifier),
          optional(field("type_parameters", $.type_parameters)),
          optional(field("interfaces", $.interfaces)),
          field("body", $.class_body),
        ),
      ),

    extension_type_name: ($) =>
      seq(
        $.identifier,
        optional($.type_parameters),
        optional(seq(".", $.identifier)),
      ),

    extension_type_representation: ($) =>
      seq("(", optional($._metadata), field("type", $.type), field("name", $.identifier), ")"),

    // --- Enums (enhanced, Dart 2.17) ---

    enum_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        "enum",
        field("name", $.identifier),
        optional($.type_parameters),
        optional($.mixins),
        optional($.interfaces),
        field("body", $.enum_body),
      ),

    enum_body: ($) =>
      seq(
        "{",
        commaSep1TrailingComma($.enum_constant),
        optional(
          seq(
            ";",
            repeat($.class_member),
          ),
        ),
        "}",
      ),

    enum_constant: ($) =>
      choice(
        seq(
          optional($._metadata),
          field("name", $.identifier),
          optional(seq(optional($.type_arguments), $.arguments)),
        ),
        seq(
          optional($._metadata),
          field("name", $.identifier),
          optional($.type_arguments),
          ".",
          choice($.identifier, "new"),
          $.arguments,
        ),
      ),

    // --- Typedefs ---

    type_alias: ($) =>
      choice(
        seq(
          optional($._metadata),
          "typedef",
          optional($.type),
          $._type_name,
          $._formal_parameter_part,
          ";",
        ),
        seq(
          optional($._metadata),
          "typedef",
          $._type_name,
          optional($.type_parameters),
          "=",
          $.type,
          ";",
        ),
      ),

    function_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        field("signature", $.function_signature),
        field("body", $.function_body),
      ),

    getter_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        field("signature", $.getter_signature),
        field("body", $.function_body),
      ),

    setter_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        field("signature", $.setter_signature),
        field("body", $.function_body),
      ),

    // `external` is optional to accept the (technically-invalid-at-top-level)
    // bodyless `void f();` form the previous grammar permitted; in practice
    // top-level bodyless functions are external.
    external_function_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        optional(field("modifier", $.external)),
        field("signature", $.function_signature),
        ";",
      ),

    external_getter_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        optional(field("modifier", $.external)),
        field("signature", $.getter_signature),
        ";",
      ),

    external_setter_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        optional(field("modifier", $.external)),
        field("signature", $.setter_signature),
        ";",
      ),

    external_variable_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        field("modifier", $.external),
        optional("final"),
        optional(field("type", $.type)),
        $.identifier_list,
        ";",
      ),

    top_level_variable_declaration: ($) =>
      seq(
        optional($._metadata),
        optional("augment"),
        choice(
          seq(
            field("modifier", choice("final", "const")),
            optional(field("type", $.type)),
            $.static_final_declaration_list,
          ),
          seq(
            field("modifier", "late"),
            "final",
            optional(field("type", $.type)),
            $.initialized_identifier_list,
          ),
          seq(
            optional(field("modifier", "late")),
            $._var_or_type,
            $.initialized_identifier_list,
          ),
        ),
        ";",
      ),

    // ========================================================================
    // Patterns (Phase 10 - Dart 3.0)
    // ========================================================================

    _guarded_pattern: ($) =>
      seq($._pattern, optional(seq("when", $._expression))),

    _pattern: ($) => $._logical_or_pattern,

    _logical_or_pattern: ($) =>
      seq($._logical_and_pattern, repeat(seq("||", $._logical_and_pattern))),

    _logical_and_pattern: ($) =>
      seq($._relational_pattern, repeat(seq("&&", $._relational_pattern))),

    _relational_pattern: ($) =>
      prec(
        PREC.RELATIONAL,
        choice(
          seq(choice($.relational_operator, choice("==", "!=")), $._real_expression),
          $._unary_pattern,
        ),
      ),

    _unary_pattern: ($) =>
      choice(
        $.cast_pattern,
        $.null_check_pattern,
        $.null_assert_pattern,
        $._primary_pattern,
      ),

    _primary_pattern: ($) =>
      choice(
        $.constant_pattern,
        $.variable_pattern,
        $._parenthesized_pattern,
        $.list_pattern,
        $.map_pattern,
        $.record_pattern,
        $.object_pattern,
      ),

    cast_pattern: ($) => seq($._primary_pattern, "as", field("type", $.type)),

    null_check_pattern: ($) => seq($._primary_pattern, "?"),

    null_assert_pattern: ($) => seq($._primary_pattern, "!"),

    constant_pattern: ($) =>
      choice(
        $.true,
        $.false,
        $.null_literal,
        seq(optional("-"), choice($.decimal_integer_literal, $.hex_integer_literal, $.decimal_floating_point_literal)),
        seq($.string_literal, optional(seq(".", $.identifier))),
        $.symbol_literal,
        seq($.identifier, optional(seq(".", $.identifier))),
        $.qualified,
        $.const_object_expression,
        seq("const", optional($.type_arguments), "[", commaSep1TrailingComma($._element), "]"),
        seq("const", optional($.type_arguments), "{", commaSep1TrailingComma($._element), "}"),
        seq("const", "(", $._expression, ")"),
        seq("const", $._record_literal_no_const),
        // Dot shorthand (Dart 3.6+): .enumValue in patterns
        seq(".", $.identifier),
      ),

    variable_pattern: ($) => seq($._final_var_or_type, field("name", $.identifier)),

    _parenthesized_pattern: ($) => seq("(", $._pattern, ")"),

    list_pattern: ($) =>
      seq(
        optional($.type_arguments),
        "[",
        commaSepTrailingComma(choice($._pattern, $.rest_pattern)),
        "]",
      ),

    rest_pattern: ($) => seq("...", optional($._pattern)),

    map_pattern: ($) =>
      seq(
        optional($.type_arguments),
        "{",
        commaSepTrailingComma(choice(seq($._expression, ":", $._pattern), $.rest_pattern)),
        "}",
      ),

    record_pattern: ($) =>
      seq("(", commaSepTrailingComma($._pattern_field), ")"),

    _pattern_field: ($) =>
      seq(optional(choice($.label, ":")), $._pattern),

    object_pattern: ($) =>
      seq(
        $._type_name,
        optional($.type_arguments),
        "(",
        commaSepTrailingComma($._pattern_field),
        ")",
      ),

    pattern_variable_declaration: ($) =>
      seq(
        choice("final", "var"),
        $._outer_pattern,
        "=",
        $._expression,
      ),

    _outer_pattern: ($) =>
      choice(
        $._parenthesized_pattern,
        $.list_pattern,
        $.map_pattern,
        $.record_pattern,
        $.object_pattern,
      ),

    pattern_assignment: ($) => seq($._outer_pattern, "=", $._expression),

    // ========================================================================
    // Identifiers and helpers
    // ========================================================================

    _identifier_or_new: ($) => choice($.identifier, "new"),

    label: ($) => seq($.identifier, ":"),

    // The raw regex for word-like tokens - used as the `word` rule
    _name: (_) => /[a-zA-Z_$][\w$]*/,

    // Dart built-in identifiers (category 2) and context keywords (category 3)
    _built_in_identifier: (_) =>
      choice(
        // Category 2: built-in identifiers
        "abstract", "as", "covariant", "deferred",
        "dynamic", "export", "external", "factory", "Function", "get", "implements",
        "import", "interface", "late", "library", "mixin",
        "operator", "part", "required", "set", "show",
        "static", "typedef",
        // Category 3: context keywords
        "hide", "native", "on", "sealed", "when", "base", "inline", "type", "augment",
      ),

    // The "super-identifier" - accepts both regular names and contextual keywords
    identifier: ($) => choice($._name, $._built_in_identifier),

    // ========================================================================
    // Comments
    // ========================================================================

    comment: ($) =>
      choice(
        // Block comments (via external scanner)
        $.block_comment,
        $.documentation_block_comment,
        // Single-line comments
        seq("//", /([^/\n].*)?/),
        seq("///", /.*/),
      ),
  },
});

function sep1(rule, separator) {
  return seq(rule, repeat(seq(separator, rule)));
}
