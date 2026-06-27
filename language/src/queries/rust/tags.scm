(call_expression
  function: [
    (identifier) @reference.call
    (scoped_identifier
      name: (identifier) @reference.call)
    (field_expression
      field: (field_identifier) @reference.call)
  ])

(macro_invocation
  macro: [
    (identifier) @reference.call
    (scoped_identifier
      name: (identifier) @reference.call)
  ])
