/// tree-sitter grammar for Stoat's stcfg config language.
///
/// The chumsky parser at config/src/parser.rs is the spec. Binding keys are
/// extremely permissive (any non-space, non-brace, non-`#` run, with `-`
/// joining modifiers), so chars like `(`, `;`, `:`, `<`, `=`, `"` are valid
/// keys and collide with structural tokens. An external scanner resolves this:
/// it emits a `key_part` token only when the run is followed by `->`, the same
/// disambiguation the ordered-choice PEG gets implicitly.

const commaSep = (rule) => optional(seq(rule, repeat(seq(',', rule)), optional(',')));

module.exports = grammar({
  name: 'stcfg',

  extras: ($) => [/[ \t\r\n]/, $.comment],

  word: ($) => $.identifier,

  externals: ($) => [$.key_part],

  rules: {
    source_file: ($) => repeat($._item),

    _item: ($) => choice($.theme_block, $.event_block),

    comment: (_) => token(seq('#', /[^\n]*/)),

    identifier: (_) => /[A-Za-z_][A-Za-z0-9_]*/,

    theme_block: ($) => seq('theme', field('name', $.identifier), $._block),

    event_block: ($) => seq('on', field('event', $.event_type), $._block),

    event_type: (_) => choice('init', 'buffer', 'key'),

    _block: ($) => seq('{', repeat($._statement), '}'),

    _statement: ($) =>
      choice(
        $.fn_decl,
        $.fn_call,
        $.let_binding,
        $.binding,
        $.predicate_block,
        $.setting,
      ),

    fn_decl: ($) => seq('fn', field('name', $.identifier), '(', ')', $._block),

    fn_call: ($) => seq(field('name', $.identifier), '(', ')', ';'),

    let_binding: ($) =>
      seq('let', field('name', $.identifier), '=', field('value', $._expr), ';'),

    setting: ($) => seq(field('path', $.setting_path), '=', field('value', $._value), ';'),

    setting_path: ($) => seq($.identifier, repeat(seq('.', $.identifier))),

    binding: ($) =>
      seq(field('key', $.key_part), '->', field('action', $._action_expr), ';'),

    predicate_block: ($) => seq(field('condition', $._predicate), $._block),

    // ---- values ----

    _value: ($) =>
      choice(
        $.string,
        $.boolean,
        $.enum_value,
        $.number,
        $.array,
        $.map,
        $.state_ref,
        $.identifier,
      ),

    string: (_) => token(seq('"', repeat(choice(/[^"\\]/, seq('\\', /./))), '"')),

    number: (_) => token(/-?\d+(\.\d+)?/),

    boolean: (_) => choice('true', 'false'),

    enum_value: ($) => seq($.identifier, '::', $.identifier),

    state_ref: ($) => seq('$', $.identifier),

    array: ($) => seq('[', commaSep($._value), ']'),

    map: ($) => seq('{', commaSep($.map_entry), '}'),

    map_entry: ($) => seq(field('key', $.identifier), ':', field('value', $._value)),

    // ---- let expressions ----

    _expr: ($) => choice($._expr_atom, $.if_expr, $.with_default),

    _expr_atom: ($) =>
      choice($.string, $.boolean, $.number, $.state_ref, $.identifier),

    if_expr: ($) =>
      prec.right(
        seq(
          'if',
          field('condition', $._predicate),
          'then',
          field('consequence', $._expr),
          'else',
          field('alternative', $._expr),
        ),
      ),

    with_default: ($) => prec.left(seq($._expr, '??', $._expr)),

    // ---- predicates ----

    _predicate: ($) => choice($._predicate_atom, $.and_predicate, $.or_predicate),

    _predicate_atom: ($) =>
      choice(
        $.parenthesized_predicate,
        $.match_predicate,
        $.comparison,
        $.bool_predicate,
      ),

    parenthesized_predicate: ($) => seq('(', $._predicate, ')'),

    bool_predicate: ($) => $.identifier,

    comparison: ($) =>
      seq(
        field('field', $.identifier),
        field('operator', choice('==', '!=', '>=', '<=', '>', '<')),
        field('value', $._value),
      ),

    match_predicate: ($) =>
      seq(field('field', $.identifier), '~', field('pattern', $.string)),

    and_predicate: ($) => prec.left(2, seq($._predicate, '&&', $._predicate)),

    or_predicate: ($) => prec.left(1, seq($._predicate, '||', $._predicate)),

    // ---- key bindings ----

    _action_expr: ($) => choice($.action, $.action_sequence),

    action_sequence: ($) => seq('[', commaSep($.action), ']'),

    action: ($) => seq(field('name', $.action_name), '(', commaSep($.argument), ')'),

    action_name: (_) => token(/[A-Za-z_][A-Za-z0-9_-]*/),

    argument: ($) => choice($.named_argument, $._value),

    named_argument: ($) =>
      seq(field('name', $.identifier), ':', field('value', $._value)),
  },
});
