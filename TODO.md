# TODOs
- [x] Mouse support (clicking cell to set active, explorer clicking files, etc)
- [x] Fullscreen explorer mode (and change the current explorer to fixed width) — `F` on focused explorer toggles full-width; sidebar is now a fixed 32 cols (no more grow-to-fit on focus).
- [x] Prevent closing active claude/shell pty cells (while claude is doing something, while command is running) (require :q!) (if not active :q works) (also look out for (:Q)
- [x] Fix pty busy-detection: descendant walk now filters both pty-host helpers (conhost/openconsole) and known shell names (bash/sh/zsh/fish/pwsh/...). Idle shell = 0 user descendants; running command = ≥1. Drops the 1.5s startup baseline — Git Bash's inner bash no longer poisons the count.
- [x] Editor features: bracket & quote autoclosing (`:set autopair`), auto-indent on Enter / `o` / `O` plus smart-split between matched pairs (`:set autoindent`), Tab/Shift-Tab indent in insert mode (`:set expandtab`), buffer-word completion popup (`:set completion`).
- [] Intellisense / LSP: hook a language server (start/stop per filetype, document sync), wire diagnostics, hover, go-to-definition, and symbol-aware completions into the existing popup.

## IDE-feature suggestions
Unordered backlog — anything here is a candidate, not a commitment. Top three by bang-for-buck: fuzzy file picker, comment toggle, matching-bracket highlight.

Navigation / search:
- [] Fuzzy file picker (`Ctrl+P`-style overlay or `:find <pat>`) — likely the single biggest "feels like an IDE" win.
- [] Symbol jump: `gd` (regex-based go-to-def per filetype) and `:tag <name>` — cheap proxy for real LSP go-to-def.
- [] Project-wide search (`:grep <pat>`) opening a quickfix-style results pane; jumping in updates the focused editor.
- [] Jumplist + tag stack (`Ctrl+O` / `Ctrl+I`) for cursor-history navigation.
- [] Recent files quick-switcher.

Editing:
- [] Smart `}` dedent — typing `}` on an indent-only line auto-aligns to its matching `{`. Pairs with autopair.
- [] Comment toggle (`gcc` / `gc{motion}`) driven by a per-filetype `commentstring`.
- [] Surround (`ysiw"`, `cs'"`, `ds(`) — vim-surround clone.
- [] Snippets — TextMate-style expansion with tabstops, riding the existing completion popup.
- [] Format on save (`:set formatprg=rustfmt`-style hook).
- [] Trailing-whitespace / final-newline cleanup on save.
- [] Multiple cursors / better visual-block editing.
- [] Undo tree / persistent undo across sessions.

Workspace / files:
- [] `.editorconfig` support — per-repo indent style/size picked up automatically.
- [] Per-buffer modelines (`# vim: ts=2 sw=2`) for file-specific overrides.
- [] One-key "reload from disk" on the external-change conflict prompt.
- [] Diff overlay against disk / against HEAD for the focused file (git plumbing already exists in `git.rs`).

Visual / informational:
- [] Matching-bracket highlight when cursor is on `{` / `(` / `[`.
- [] Inline git blame on the current line.
- [] Indent guides (vertical lines at indent boundaries) — extends `:set list` infra.
- [] Trailing-whitespace highlight.
- [] Status-bar enrichments: language, encoding, line endings, git branch, dirty count.

Structural (each is a real undertaking):
- [] Tree-sitter for accurate highlighting + structural motions (`]m` next function, etc.). Worth doing before LSP — LSP doesn't replace TS.
- [] LSP (already split out above).
- [] Debug adapter (DAP) — way later.
- [x] Hex editor cell — `:hex` toggles focused file cell between Edit and Hex (in-place swap, same grid slot, dirty buffer carries across as raw bytes). `:hex <path>` opens (or swaps) a file as hex. `:edit` toggles back; `:edit!` lossy-converts on invalid UTF-8. `:e <path>` falls back to hex when bytes aren't valid UTF-8 (every file is openable). Own Normal/Insert/Visual modes; ASCII pane is read-only mirror. Overwrite-only for v1.
- [x] `:ex` / `:execute <cmd>` — run a one-off command in a pty cell. Starts minimized so it's trackable if it doesn't exit quickly; when the command exits the shell exits and the cell cleans itself up. Ephemeral (not persisted to `.acedata`).
