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

(type_identifier) @reference.type

(impl_item
  trait: [
    (type_identifier) @reference.implements
    (scoped_type_identifier
      name: (type_identifier) @reference.implements)
  ])
