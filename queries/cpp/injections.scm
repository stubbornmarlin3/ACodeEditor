; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/cpp/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

; --- inherited from: c ---
((comment) @injection.content
 (#set! injection.language "comment"))

((preproc_arg) @injection.content
 (#set! injection.language "c")
 (#set! injection.include-children))
; --- cpp (injections.scm) ---

((preproc_arg) @injection.content
 (#set! injection.language "cpp")
 (#set! injection.include-children))

(raw_string_literal
  delimiter: (raw_string_delimiter) @injection.language
  (raw_string_content) @injection.content)
