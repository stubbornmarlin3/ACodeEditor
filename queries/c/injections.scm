; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/c/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

((comment) @injection.content
 (#set! injection.language "comment"))

((preproc_arg) @injection.content
 (#set! injection.language "c")
 (#set! injection.include-children))
