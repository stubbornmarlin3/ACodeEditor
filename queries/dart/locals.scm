; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/dart/locals.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

; Scopes
;-------

[
 (block)
 (try_statement)
 (catch_clause)
 (finally_clause)
] @local.scope

; Definitions
;------------

(class_definition
 body: (_) @local.definition.type)

; References
;------------

(identifier) @local.reference
