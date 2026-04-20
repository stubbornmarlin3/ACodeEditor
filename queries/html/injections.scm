; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/html/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

((comment) @injection.content
 (#set! injection.language "comment"))

((script_element
  (raw_text) @injection.content)
 (#set! injection.language "javascript"))

((style_element
  (raw_text) @injection.content)
 (#set! injection.language "css"))
