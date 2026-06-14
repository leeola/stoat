; Comments
(comment) @comment

; Literals
(string) @string
(number) @number
(boolean) @boolean

; State references: $name
(state_ref) @variable

; Enum::Variant values
(enum_value
  (identifier) @type
  (identifier) @constant)

; Setting paths (dotted segments)
(setting_path (identifier) @property)

; Map keys
(map_entry key: (identifier) @attribute)

; Action calls
(action name: (action_name) @function)

; Keywords
[
  "theme"
  "on"
  "let"
  "fn"
  "if"
  "then"
  "else"
] @keyword
(event_type) @keyword

; Operators
[
  "->"
  "=="
  "!="
  ">="
  "<="
  ">"
  "<"
  "&&"
  "||"
  "??"
  "~"
  "="
] @operator

; Punctuation
[
  "{"
  "}"
  "["
  "]"
  "("
  ")"
  "."
  ","
  ":"
  ";"
  "::"
] @punctuation
