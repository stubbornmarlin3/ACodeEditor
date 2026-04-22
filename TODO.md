# TODOs
- [x] Mouse support (clicking cell to set active, explorer clicking files, etc)
- [] Fullscreen explorer mode (and change the current explorer to fixed width
- [x] Prevent closing active claude/shell pty cells (while claude is doing something, while command is running) (require :q!) (if not active :q works) (also look out for (:Q)
- [] Fix pty busy-detection: output-recency + sysinfo descendant-count (with a 1.5s startup baseline) still misreports — idle Git Bash reads as busy, and a real `npm run dev` doesn't always block `:q` / `:Q`. Likely need a different signal (shell-integration OSC 133 for prompt boundaries, or a per-shell heuristic) rather than process-tree counting.
- [] Editor features: intellisense, code suggestions/completions, bracket & quote autoclosing, etc...
- [] Hex editor view for files
- [x] `:ex` / `:execute <cmd>` — run a one-off command in a pty cell. Starts minimized so it's trackable if it doesn't exit quickly; when the command exits the shell exits and the cell cleans itself up. Ephemeral (not persisted to `.acedata`).
