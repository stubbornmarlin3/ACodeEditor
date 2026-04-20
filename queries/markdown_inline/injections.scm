; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/markdown.inline/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.


((html_tag) @injection.content 
  (#set! injection.language "html") 
  (#set! injection.include-unnamed-children)
  (#set! injection.combined))

((latex_block) @injection.content (#set! injection.language "latex") (#set! injection.include-unnamed-children))
