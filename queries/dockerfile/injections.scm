; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/dockerfile/injections.scm at ref master.
; Inheritance via ';inherits:' has been flattened at vendor time.

((comment) @injection.content
 (#set! injection.language "comment"))

((shell_command (shell_fragment) @injection.content)
 (#set! injection.language "bash")
 (#set! injection.combined))

((run_instruction
 (heredoc_block (heredoc_line) @injection.content . "\n" @injection.content))
 (#set! injection.language "bash")
 (#set! injection.combined))

((copy_instruction
 (path (heredoc_marker)) . (path) @injection.filename
 (heredoc_block (heredoc_line) @injection.content . "\n" @injection.content))
 (#set! injection.combined))
