; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/json/highlights.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

[
  (true)
  (false)
] @constant.builtin.boolean
(null) @constant.builtin
(number) @constant.numeric

(string) @string
(escape_sequence) @constant.character.escape

(pair
  key: (_) @variable.other.member)

(comment) @comment

["," ":"] @punctuation.delimiter
[
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket
