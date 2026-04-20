; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/make/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

((comment) @injection.content
 (#set! injection.language "comment"))

((shell_text) @injection.content
 (#set! injection.language "bash"))
