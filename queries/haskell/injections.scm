; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/haskell/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

((comment) @injection.content
 (#set! injection.language "comment"))

(quasiquote
 (quoter) @_quoter
 ((quasiquote_body) @injection.content
  (#match? @_quoter "(persistWith|persistLowerCase|persistUpperCase)")
  (#set! injection.language "haskell-persistent")
 )
)

(quasiquote
 (quoter) @injection.language
 (quasiquote_body) @injection.content)
