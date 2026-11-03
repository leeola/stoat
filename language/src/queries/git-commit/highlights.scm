; The patterns of vendor/tree-sitter-git-commit/queries/highlights.scm, with
; the captures renamed to the editor's theme keys and the patterns no key
; styles dropped. A later pattern overrides an earlier one, so the comment
; comes first and the captures inside a comment come after it.

(comment) @comment

(subject) @title.markup
(path) @string
(branch) @constant

(trailer
  key: (trailer_key) @property
  value: (trailer_value) @string)

[":" "=" "->" (scissors)] @punctuation.delimiter
