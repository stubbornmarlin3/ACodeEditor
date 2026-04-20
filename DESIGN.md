# ACodeTerm — Design Concept

A TUI code editor / dev-environment that bundles a file explorer, git
status, Claude Code CLI sessions, shells, and a vim-style editor into a
single tiling workspace. Keyboard-driven. Runs in any terminal.

---

## 1. Panel Map (matches the reference screenshot)

```
┌────────┬─────────────────────────────────────────────────┬────┐
│  FILES │                                                 │ P  │
│ (green)│                MAIN CELL A                      │ R  │
│        │                 (orange)                        │ O  │
│        │                                                 │ J  │
│        │                                                 │ E  │
│        │                                                 │ C  │
│        │                                                 │ T  │
│        │                                                 │ S  │
│        │                                                 │(yel│
│        │                                                 │low)│
├────────┼────────────────────────┬────────────────────────┤    │
│  GIT   │      MAIN CELL B       │      MAIN CELL C       │    │
│(magent)│        (blue)          │   (grayish purple)     │    │
└────────┴────────────────────────┴────────────────────────┴────┘
 ▲ minimized session tray grows upward from here ▲
 └─────────────── status bar sits at the very bottom row ───────┘
```

| Region          | Role                                                      |
|-----------------|-----------------------------------------------------------|
| Files           | File explorer tree; names tinted by git status.           |
| Git             | Branch, staged / unstaged / untracked. Grows upward.      |
| Main cells      | Swappable: Claude CLI, shell, or editor. Splittable.      |
| Project rail    | Open projects + state dot (running / idle / attention).   |
| Minimized tray  | Minimized cells, stack grows bottom-up.                   |
| Status bar      | Bottom row. Mode · focus · git · tray · help.             |

The screenshot's hero colours are **semantic hints**. The real palette is
defined in §3.

---

## 2. Core UI Concepts

### 2.1 Cells

The main content area is a tree of **cells**:

- `claude`   — a PTY running the **Claude Code CLI** (`claude`)
- `shell`    — a PTY running bash / zsh / pwsh / cmd
- `editor`   — a vim-style buffer for one file
- `diff`     — read-only diff view (drilled-in from git)
- `preview`  — markdown / image / man-page viewer

Because `claude` is just another PTY, the Claude cell inherits everything
the upstream CLI offers (slash commands, plan mode, tools) for free.
ACodeTerm pipes stdin/stdout and renders the PTY buffer.

### 2.2 Split tree

Cells live in a split tree — horizontal or vertical splits with a ratio,
same shape as tmux / zellij.

### 2.3 Sessions vs cells

A **session** is long-lived (a running `claude`, a shell PID, an editor
buffer). A **cell** is where a session is displayed. Sessions can be moved,
minimized into the tray, or duplicated across cells.

### 2.4 Projects

A project = root folder + its sessions + its layout. The yellow rail lists
open projects; each project carries a **state dot** (§3.2) so the rail
doubles as a "what's running everywhere" dashboard.

### 2.5 Minimized tray

Any cell can be minimized. Minimized sessions stack bottom-up — newest on
top. Each row shows a type glyph, session name, and an activity indicator.

### 2.6 Focus indicator (panel-level)

Focus is shown **solely by a heavy border** on the focused panel:

```
 unfocused        focused
 ┌─ label ─┐      ┏━ label ━┓
 │         │      ┃         ┃
 └─────────┘      ┗━━━━━━━━━┛
```

In a colour terminal the heavy border also renders in the `accent` colour.
No window-control buttons — the app is keyboard-driven.

### 2.7 Selection indicator (within a panel)

Inside a panel, the keyboard-selected row uses `bg-sel` background + a `▸`
gutter marker. These are orthogonal to panel focus: the heavy border says
*which panel*, `▸` + tint says *which row inside that panel*.

### 2.8 Mode system (vim-wide)

ACodeTerm is modal end-to-end, vim-style. **One mode is active at a time,
app-wide.** The mode determines what every key does, everywhere.

**Modes:**

| Mode      | Purpose                                                       | Badge |
|-----------|---------------------------------------------------------------|-------|
| `Normal`  | Keys are app commands (focus, splits, toggles, `:`).          | `NOR` |
| `Insert`  | Keys pass through to the focused cell (type into PTY/editor). | `INS` |
| `Command` | `:`-line at the bottom; run `:q`, `:split`, `:e foo`, etc.    | `CMD` |
| `Visual`  | Editor only, for selections. (Later milestone.)               | `VIS` |
| `Replace` | Editor only. (Later milestone.)                               | `REP` |

**Per-cell mode support:**

| Cell type   | Normal | Insert | Visual | Replace |
|-------------|--------|--------|--------|---------|
| `editor`    |   ✓    |   ✓    |   ✓    |   ✓     |
| `claude`    |   ✓    |   ✓    |        |         |
| `shell`     |   ✓    |   ✓    |        |         |
| `diff`      |   ✓    |        |        |         |
| `preview`   |   ✓    |        |        |         |
| Side panels (`files`, `git`, `projects`) | ✓ |  |  |  |

`Command` is always available regardless of focus — it's a global overlay.

**Auto-demotion on focus change:** if you move focus (`Ctrl-hjkl`) from a
cell that supports the current mode to one that doesn't, the mode drops
to `Normal`. E.g., typing in `shell` (Insert) → `Ctrl-h` to `git` → mode
becomes `Normal` automatically.

**Mode transitions (from Normal):**

- `i` / `a` → `Insert` (if the focused cell supports it)
- `:` → `Command`
- `v` → `Visual` (editor only, later)
- `R` → `Replace` (editor only, later)

**Exit back to Normal:** `Esc` from any non-Normal mode.

**Why no command palette:** vim's `:` command line is doing the same job
— it takes a command string, completes it, runs it. Adding a separate
palette overlay duplicates that mechanism. §4.10 is the `:` line, not a
palette.

### 2.9 Design principles

1. **One cue, one job.** Panel focus = heavy border. Row selection = `▸`
   + bg tint. State = colour + shape glyph. No redundant decoration.
2. **Binary panel states.** Sidebars are shown or hidden; no icon-only
   middle state.
3. **Status bar as the single source of ambient truth** (§5).
4. **Contextual key hints.** Each panel shows the 1–3 keys relevant now.
5. **Colour encodes state, never decoration.** Every colour has one
   semantic job; shape carries the same info for monochrome terminals.
6. **Typography as hierarchy.** Semantic glyphs (`⎇` branch, `↑↓`
   ahead/behind, `●◐◉○✕` project states) do work; nothing ornamental.
7. **Responsive breakpoints** (§4.11).

---

## 3. Colour System

Colour encodes **state**, not decoration. Every colour has one semantic
job; every coloured element also carries a shape or letter so the
information survives in a monochrome terminal or colourblind user.

### 3.1 Palette

| Token        | Default (dark theme) | Used for                              |
|--------------|----------------------|---------------------------------------|
| `accent`     | bright cyan          | Focused-panel border, active tab bar  |
| `fg`         | near-white           | Regular text                          |
| `dim`        | gray-50              | Secondary text, inactive borders      |
| `bg`         | ~#11131a             | App background                        |
| `bg-sel`     | ~#1d2230             | Selected-row background               |
| `bg-sel+`    | ~#2a3248             | Visual-mode block / secondary select  |
| `ok`         | green                | Staged, idle, success                 |
| `warn`       | amber / yellow       | Modified, working, running            |
| `attn`       | bright cyan          | Attention (unread reply, new output)  |
| `info`       | blue                 | Untracked, informational              |
| `err`        | red                  | Conflict, error, deletion, crash      |
| `muted`      | gray-30              | No session, empty state               |

Light theme swaps `bg`/`fg` and picks tints with the same roles. Users
pick a theme; code never hard-codes colours, only tokens.

### 3.2 Project dots

Each project in the rail carries a **state dot**. Shape + colour — so the
dot reads the same with no colour.

| Glyph | Colour | Meaning                                                           |
|-------|--------|-------------------------------------------------------------------|
| `○`   | muted  | No session running                                                |
| `●`   | ok     | All sessions idle                                                 |
| `◐`   | warn   | Something is working (claude thinking, shell running a command)   |
| `◉`   | attn   | Attention: claude waiting on prompt, shell printed new output, editor buffer changed on disk |
| `✕`   | err    | Process exited non-zero, merge conflict, crash                    |

The dot represents the *aggregate worst state* across that project's
sessions — so if any claude is `◉`, the whole project is `◉`.

The **active** project (the one you're viewing) is additionally prefixed
with `▸` and gets a `bg-sel` row. State and position are independent
cues: the dot tells you *how*, the arrow tells you *where*.

### 3.3 File explorer (git-status coloured)

Each file shows a one-letter status marker and colour-tinted name:

| Status       | Letter  | Name colour | Notes                       |
|--------------|---------|-------------|-----------------------------|
| Unchanged    | (blank) | `fg`        |                             |
| Staged mod   | `M`     | `ok`        |                             |
| Staged add   | `A`     | `ok`        |                             |
| Unstaged mod | `M`     | `warn`      |                             |
| Untracked    | `?`     | `info`      |                             |
| Deleted      | `D`     | `err`       | strikethrough if supported  |
| Conflict     | `U`     | `err` bg    | red background              |
| Ignored      | `!`     | `muted`     | hidden by default           |

A folder inherits the "worst" state of its children (same rule as project
dots), displayed as a dot by the folder name.

### 3.4 Git panel

Section headings (`STAGED`, `UNSTAGED`, `UNTRACKED`) are `accent`. Each
file row uses the §3.3 letter + colour mapping. Branch badge tints by
health: clean = dim `ok`, dirty = `warn`, conflict = `err`.

### 3.5 Editor syntax highlighting

Powered by tree-sitter. Every language's tokens map to the palette so
colour-blind users who theme palette-tokens get consistent results across
languages.

| Token class        | Colour         | Examples                        |
|--------------------|----------------|---------------------------------|
| keyword / control  | purple         | `if` `else` `return` `match`    |
| type / struct      | `accent` (cyan)| types, typedefs                 |
| function name      | `warn` (yellow)| definition + call site          |
| string             | `ok` (green)   | `"…"` `'…'`                     |
| number / literal   | orange         | `42` `3.14` `true`              |
| comment            | `dim` italic   | `// …` `/* … */`                |
| preproc / macro    | magenta-dim    | `#include` `cfg_attr`           |
| punctuation        | `fg`           | `. , ; = + -`                   |
| diagnostic: error  | `err` squiggle | underlined `~~~`                |
| diagnostic: warn   | `warn` squiggle| underlined `~~~`                |

Cursor line gets `bg-sel`. Visual selection gets `bg-sel+` with `fg`
preserved. Search match: `warn` bg with dark `fg`.

### 3.6 Selection / keyboard cursor

- **Row-level** (tree items, git rows, tray rows, `:` completions):
  `▸` gutter glyph + `bg-sel`. Example:
  ```
    ▾ src/
   ▸  main.rs   M      ← selected row: ▸ glyph + bg tint
     ui.rs      M
  ```
- **Editor** normal mode: cursor-line `bg-sel`; cursor is a block.
- **Editor** visual mode: selection block `bg-sel+`.
- **Search** (`/foo`): all matches `warn` bg; current match `attn` bg.

### 3.7 Status bar

- **Mode badge** uses a mode-coloured background (vim tradition):
  `NOR` = `dim` bg · `INS` = `ok` bg · `VIS` = purple bg · `CMD` = `warn`
  bg · `REP` = `err` bg. The mode text itself is always readable
  contrast (dark fg on bright bg).
- **Git segment** tints by health: clean = dim `ok`, dirty = `warn`,
  conflict = `err`.
- **Tray count** uses `attn` if any tray item has unread activity.
- **Hint** is always `dim`.

---

## 4. UI States (full-layout ASCII mockups)

All mockups are monochrome (markdown can't render colour). Each includes
a short "colour notes" box pointing at what the live UI would tint.

Legend used in mockups:

```
 heavy border ┏━┓ = focused panel
 ▸            = keyboard-selected row within a panel
 ● ◐ ◉ ○ ✕   = project state dots (§3.2)
 M A D ? U   = git status letters (§3.3)
 ▌           = active cursor in a PTY / editor
```

### 4.1 Default layout — Claude cell focused

```
┌─ files ────────┐┏━ claude   claude code cli ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌─ projects ─┐
│ ▾ ACodeTerm    │┃                                                            ┃│▸◐ ACodeTerm│
│   ▾ src/       │┃  claude › what are we building today?                      ┃│ ◉ echo     │
│  ▸  main.rs  M │┃                                                            ┃│ ● AcOS     │
│     ui.rs    M │┃  you › sketch the event loop                               ┃│ ○ aCPU     │
│     app.rs   A │┃                                                            ┃│            │
│   ▸ tests/     │┃  claude › here is a first pass:                            ┃│  [+] new   │
│     Cargo.toml │┃    1. poll crossterm events                                ┃│            │
│     README   ? │┃    2. route to focused cell                                ┃│            │
│                │┃    3. redraw dirty regions                                 ┃│            │
│                │┃                                                            ┃│            │
│                │┃  ▌                                                         ┃│            │
├─ git ──────────┤┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛│            │
│ ⎇ main ↑2      │┌─ shell  pwsh ──────────────────┐┌─ edit  main.rs     ────┐│            │
│ M 3  ?? 1      ││ PS> cargo check                ││  1  use crossterm::*;   ││            │
│                ││ PS> ▌                          ││  2  fn main() { … }     ││            │
└────────────────┘└────────────────────────────────┘└─────────────────────────┘└────────────┘
 claude · build-ui  streaming                    ⎇ main ↑2  M3 ?1    tray 0     [? help]
```

**Colour notes**
- Claude cell border: `accent` (cyan) — focused.
- Project dots: `ACodeTerm ◐` amber (this claude is streaming), `echo ◉`
  cyan (its claude is waiting on a prompt), `AcOS ●` green (idle),
  `aCPU ○` muted gray (no session).
- Active project row `ACodeTerm`: `bg-sel` + `▸`.
- File tree: `main.rs` + `ui.rs` in `warn`; `app.rs` in `ok`; `README` in
  `info`; selected row `main.rs` has `bg-sel` + `▸`.
- Git panel: `M 3` in `warn`, `?? 1` in `info`, branch in dim `warn`
  (dirty).
- Status bar: `streaming` in `warn`; mode badge colour idle here.

### 4.2 Focus moved to editor — syntax colouring visible

```
┌─ files ────────┐┌─ claude   claude code cli ────────────────────────────────┐┌─ projects ─┐
│ ▾ ACodeTerm    ││                                                            ││▸◐ ACodeTerm│
│   ▾ src/       ││  claude › here is a first pass:                            ││ ◉ echo     │
│     main.rs  M ││    1. poll crossterm events                                ││ ● AcOS     │
│  ▸  ui.rs    M ││    2. route to focused cell                                ││ ○ aCPU     │
│     app.rs   A ││    3. redraw dirty regions                                 ││            │
│   ▸ tests/     ││                                                            ││  [+] new   │
│     Cargo.toml ││                                                            ││            │
│     README   ? ││                                                            ││            │
│                ││                                                            ││            │
│                │└────────────────────────────────────────────────────────────┘│            │
│                │┌─ shell  pwsh ──────────────────┐┏━ edit  ui.rs  42 ━━━━━━━┓│            │
├─ git ──────────┤│ PS> cargo check                │┃  40  pub fn render(     ┃│            │
│ ⎇ main ↑2      ││ PS> ▌                          │┃  41      f: &mut Frame, ┃│            │
│ M 3  ?? 1      ││                                │┃  42      area: Rect,    ┃│            │
│                ││                                │┃  43  ) -> Result<()> {  ┃│            │
└────────────────┘└────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━┛└────────────┘
 INS · ui.rs +   42:17                           ⎇ main ↑2  M3 ?1    tray 0     [:w save]
```

**Colour notes**
- Editor border: `accent` heavy.
- Line 42 has `bg-sel` (cursor line). `pub fn` purple; `render` yellow;
  `Frame`, `Rect`, `Result` cyan; `()`, `{}`, `->` fg; numbers orange.
- Mode badge `INS` green bg. File marker `+` = dirty, drawn in `warn`.

### 4.3 Zoom — focused cell fills the main area, rails stay

`<leader>z` grows the focused cell. Rails stay so you don't lose context.
Hidden cells park in the tray for the duration of the zoom.

```
┌─ files ────────┐┏━ edit  main.rs      47 / 182 ━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌─ projects ─┐
│ ▾ ACodeTerm    │┃  45  fn draw(&mut self, frame: &mut Frame) {              ┃│▸◐ ACodeTerm│
│   ▾ src/       │┃  46      let chunks = Layout::default()                   ┃│ ◉ echo     │
│  ▸  main.rs  M │┃  47          .direction(Direction::Horizontal)▌           ┃│ ● AcOS     │
│     ui.rs    M │┃  48          .constraints(vec![                           ┃│ ○ aCPU     │
│     app.rs   A │┃  49              Constraint::Length(16),                  ┃│            │
│   ▸ tests/     │┃  50              Constraint::Min(40),                     ┃│  [+] new   │
│     Cargo.toml │┃  51              Constraint::Length(14),                  ┃│            │
│     README   ? │┃  52          ])                                           ┃│            │
│                │┃  53          .split(frame.size());                        ┃│            │
│                │┃  54      }                                                ┃│            │
├─ git ──────────┤┃                                                           ┃│            │
│ ⎇ main ↑2      │┃                                                           ┃│            │
│ M 3  ?? 1      │┃                                                           ┃│            │
└────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛└────────────┘
 NOR · main.rs  47:44                            ⎇ main ↑2  M3 ?1    tray 2 zoom [<ldr>z]
```

**Colour notes**: line 47 has `bg-sel` (cursor line); keywords purple,
types cyan, numbers orange, strings (if any) green.

### 4.4 Three-way split in the main area

```
┌─ files ────────┐┌─ claude#1 ───────┐┏━ claude#2 ━━━━━━━┓┌─ shell ──────────┐┌─ projects ─┐
│ ▾ ACodeTerm    ││ you › refactor   │┃ you › add tests  ┃│ $ cargo test     ││▸◐ ACodeTerm│
│   ▸ src/       ││       this fn    │┃       for Layout ┃│   running 12     ││ ◉ echo     │
│   ▸ tests/     ││ claude › simpler │┃ claude › matrix: ┃│   test_layout OK ││ ● AcOS     │
│   ▸ docs/      ││   form…          │┃   ▌              ┃│ $ ▌              ││ ○ aCPU     │
│     Cargo.toml ││                  │┃                  ┃│                  ││            │
│     README   ? ││                  │┃                  ┃│                  ││  [+] new   │
│                │└──────────────────┘┗━━━━━━━━━━━━━━━━━━┛└──────────────────┘│            │
│                │┌─ edit  ui.rs      ────────────────────────────────────────┐│            │
│                ││ 101  impl Widget for Rail { … }                           ││            │
│                ││ 102                                                       ││            │
├─ git ──────────┤│ 103  fn render(…) { … }                                   ││            │
│ ⎇ feat/tui     ││ 104                                                       ││            │
│ M ui.rs        ││                                                           ││            │
└────────────────┘└───────────────────────────────────────────────────────────┘└────────────┘
 claude · claude#2  streaming                    ⎇ feat/tui  M1       tray 0    [? help]
```

**Colour notes**: two claude sessions both `◐` → project dot aggregates to
`◐`. Shell output OK line is dim `ok`.

### 4.5 Tabs inside a single cell

```
┌─ files ────────┐┏━ claude   shell   edit main.rs   edit ui.rs ━━━━━━━━━━━━━━┓┌─ projects ─┐
│ ▾ ACodeTerm    │┃          ━━━━━                                             ┃│▸● ACodeTerm│
│   ▸ src/       │┃ PS> ls                                                     ┃│ ◉ echo     │
│   ▸ tests/     │┃ Cargo.toml  src/  tests/  README.md                        ┃│ ● AcOS     │
│   ▸ docs/      │┃ PS> cargo build                                            ┃│ ○ aCPU     │
│     Cargo.toml │┃    Compiling acodeterm v0.1.0                              ┃│            │
│     README   ? │┃    Finished in 2.14s                                       ┃│  [+] new   │
│                │┃ PS> ▌                                                      ┃│            │
│                │┃                                                            ┃│            │
│                │┃                                                            ┃│            │
│                │┃                                                            ┃│            │
│                │┃                                                            ┃│            │
├─ git ──────────┤┃                                                            ┃│            │
│ ⎇ main ↑2      │┃                                                            ┃│            │
│ M 3  ?? 1      │┃                                                            ┃│            │
└────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛└────────────┘
 shell · pwsh                                    ⎇ main ↑2  M3 ?1    tray 0     [gt next]
```

Active tab: its name is `accent` and the heavy-underline under it is
`accent`. Inactive tab names are `dim`.

### 4.6 Minimized tray populated (bottom-up)

```
┌─ files ────────┐┏━ claude   claude code cli ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌─ projects ─┐
│ ▾ ACodeTerm    │┃ you › summarize diff in src/ui.rs                          ┃│▸◐ ACodeTerm│
│   ▸ src/       │┃                                                            ┃│ ◉ echo     │
│   ▸ tests/     │┃ claude › three user-visible changes …                      ┃│ ● AcOS     │
│   ▸ docs/      │┃ ▌                                                          ┃│ ○ aCPU     │
│     Cargo.toml │┃                                                            ┃│            │
│     README   ? │┃                                                            ┃│  [+] new   │
├─ git ──────────┤┃                                                            ┃│            │
│ ⎇ main ↑2      │┃                                                            ┃│            │
│ M 3  ?? 1      │┃                                                            ┃│            │
│                │┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛│            │
│                │ ┌ tray ────────────────────────────────────────────────┐    │            │
│                │ │ ✎ ui.rs              +   unsaved                      │    │            │
│                │ │ ▸ pwsh               ●   output since last view       │    │            │
│                │ │ ✎ main.rs                idle                         │    │            │
└────────────────┘ └──────────────────────────────────────────────────────┘    └────────────┘
 claude · build-ui  streaming                    ⎇ main ↑2  M3 ?1    tray 3    [1..3 open]
```

Tray activity dots use the §3.2 palette: `+` in `warn` (unsaved), `●` in
`attn` (new output).

### 4.7 Project rail wide

```
┌─ files ────────┐┌─ claude ──────────────────────────────┐┏━ projects ━━━━━━━━━━━━━━━━━━━┓
│ ▾ ACodeTerm    ││ you › what's the plan?                 │┃▸◐ ACodeTerm         (active) ┃
│   ▸ src/       ││                                        │┃    ~/Dev/ACodeTerm           ┃
│   ▸ tests/     ││ claude › split the app into four       │┃    ⎇ main   M3              ┃
│   ▸ docs/      ││ passes…                                │┃                               ┃
│     Cargo.toml ││                                        │┃ ◉ echo                       ┃
│     README   ? ││                                        │┃    ~/Dev/echo                ┃
│                ││                                        │┃    ⎇ dev   claude waiting    ┃
│                │└────────────────────────────────────────┘┃                               ┃
│                │┌─ shell ───────────────────────────────┐┃ ● AcOS                       ┃
│                ││ $ ▌                                    │┃    ~/Dev/AcOS                ┃
│                ││                                        │┃    ⎇ feat/boot               ┃
├─ git ──────────┤│                                        │┃                               ┃
│ ⎇ main ↑2      ││                                        │┃ ○ aCPU                       ┃
│ M 3  ?? 1      ││                                        │┃    ~/Dev/aCPU   (no session) ┃
│                ││                                        │┃  ──────────────              ┃
│                ││                                        │┃  [+] open folder…             ┃
│                ││                                        │┃  [G] clone repo…              ┃
└────────────────┘└────────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
 projects · 4 open                               ⎇ main ↑2  M3 ?1    tray 0    [↵ switch]
```

### 4.8 Git panel expanded (grows upward into files)

```
┌─ files ────────┐┌─ claude ─────────────────────────────────────────────────┐┌─ projects ─┐
│ ▾ ACodeTerm    ││ you › …                                                   ││▸● ACodeTerm│
│   ▸ src/       ││                                                           ││ ◉ echo     │
│   ▸ tests/   ● ││                                                           ││ ● AcOS     │
│     Cargo    M │└───────────────────────────────────────────────────────────┘│ ○ aCPU     │
┣━ git ━━━━━━━━━━┫┌─ shell  pwsh ─────────────┐┌─ edit  main.rs     ────────┐│            │
┃ ⎇ main ↑2 ↓0   ┃│ PS> ▌                      ││  1  use crossterm::*;       ││  [+] new   │
┃ origin acarter ┃│                            ││  2  fn main() { … }         ││            │
┃ ─────────────  ┃│                            ││                             ││            │
┃ STAGED (2)     ┃│                            ││                             ││            │
┃   M  src/ui.rs ┃│                            ││                             ││            │
┃   A  src/rail  ┃│                            ││                             ││            │
┃ UNSTAGED (3)   ┃│                            ││                             ││            │
┃  ▸M  main.rs   ┃│                            ││                             ││            │
┃   M  Cargo     ┃│                            ││                             ││            │
┃   D  docs/old  ┃│                            ││                             ││            │
┃ UNTRACKED (1)  ┃│                            ││                             ││            │
┃   ?  notes.txt ┃│                            ││                             ││            │
┃ ─────────────  ┃│                            ││                             ││            │
┃ c commit  p push┃│                           ││                             ││            │
┃ l log   b branch┃│                           ││                             ││            │
┗━━━━━━━━━━━━━━━━┛└────────────────────────────┘└─────────────────────────────┘└────────────┘
 git · 6 changes                                 ⎇ main ↑2  M3 ?1    tray 0    [↵ diff]
```

**Colour notes**: `STAGED` rows green, `UNSTAGED` rows amber, `UNTRACKED`
row blue, `D docs/old` red with strikethrough. Selected row (`▸M main.rs`)
has `bg-sel`. Folder `tests/ ●` shows aggregated child state.

### 4.9 Sidebars hidden

```
┏━ claude   claude code cli ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃                                                                                              ┃
┃  claude › (sidebars hidden — just the main area)                                             ┃
┃                                                                                              ┃
┃  you › sketch the event loop                                                                 ┃
┃                                                                                              ┃
┃  claude › here is a first pass:                                                              ┃
┃    1. poll crossterm events                                                                  ┃
┃    2. route to focused cell                                                                  ┃
┃    3. redraw dirty regions                                                                   ┃
┃                                                                                              ┃
┃  ▌                                                                                           ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
┌─ shell  pwsh ────────────────────────────────┐┌─ edit  main.rs     ──────────────────────┐
│ PS> cargo check                              ││  1  use crossterm::*;                     │
│ PS> ▌                                        ││  2  fn main() { … }                       │
└──────────────────────────────────────────────┘└───────────────────────────────────────────┘
 claude · build-ui  streaming                    ⎇ main ↑2  M3 ?1    tray 0    [<ldr>e files]
```

### 4.10 Command line (`:` mode)

Pressing `:` in Normal mode puts the status bar into command-line mode.
There is no overlay — the status bar *is* the command line (vim-style).
Pane focus doesn't change. Completions appear in a small, dismissible
popup just above the bar.

```
┌─ files ────────┐┌─ claude ─────────────────────────────────────────────────┐┌─ projects ─┐
│ ▾ ACodeTerm    ││ you ›                                                     ││▸◐ ACodeTerm│
│   ▸ src/       ││                                                           ││ ◉ echo     │
│   ▸ tests/     ││                                                           ││ ● AcOS     │
│   ▸ docs/      ││                                                           ││ ○ aCPU     │
│     Cargo.toml ││                                                           ││            │
│     README   ? ││                                                           ││  [+] new   │
│                │└───────────────────────────────────────────────────────────┘│            │
│                │   ┌─ :split_ ──────────────────────────────────────────┐   │            │
│                │   │ split horizontal                       <ldr>-       │   │            │
│                │   │ split vertical                         <ldr>\       │   │            │
│                │   │ split claude                            :split c    │   │            │
│                │   │ split shell                             :split sh   │   │            │
├─ git ──────────┤   └────────────────────────────────────────────────────┘   │            │
│ ⎇ main ↑2      │┌─ shell  pwsh ─────────────────┐┌─ edit  main.rs ────────┐│            │
│ M 3  ?? 1      ││ PS>                            ││  1  use crossterm::*; ││            │
│                ││                                ││  2  fn main() { … }   ││            │
└────────────────┘└────────────────────────────────┘└────────────────────────┘└────────────┘
 CMD  :split_                                    ⎇ main ↑2  M3 ?1             [↵ run  esc]
```

The mode badge turns `CMD` amber. Typing filters the completion popup
live. `Tab` accepts, `Enter` runs, `Esc` cancels. Fuzzy-matched characters
in suggestions are `accent`-coloured.

**Core commands (initial set):**

| Command             | Effect                                            |
|---------------------|---------------------------------------------------|
| `:q` / `:quit`      | Close focused cell (or quit app if last).         |
| `:qa`               | Quit app unconditionally.                         |
| `:w`                | Save focused editor buffer.                       |
| `:wq`               | Save + close focused editor.                      |
| `:e <path>`         | Open `<path>` in a new editor cell.               |
| `:split`, `:sp`     | Horizontal split of focused cell.                 |
| `:vsplit`, `:vsp`   | Vertical split.                                   |
| `:claude`           | Replace / open a claude cell in focused split.    |
| `:shell [cmd]`      | Open a shell cell (optionally running `cmd`).     |
| `:proj <name>`      | Switch to project by name.                        |
| `:theme <name>`     | Swap theme.                                       |
| `:zoom`             | Toggle zoom on focused cell.                      |
| `:tabnew` / `:tabn` | New tab in focused cell / next tab.               |

### 4.11 Responsive breakpoints

| Width       | Layout                                                   |
|-------------|----------------------------------------------------------|
| ≥ 100 cols  | Full: files | main | projects.                           |
| 80 – 99     | Project rail auto-hides. Reveal with `<leader>p`.        |
| 60 – 79     | Files+git column also auto-hides. Reveal with `<ldr>e`.  |
| < 60 cols   | Main area only, no splits. Single cell at a time.        |

A user's manual toggle overrides auto-hide until the next resize.

---

## 5. Status Bar Design

Fixed single row at the bottom. Left context segment, middle git segment,
right tray + hint.

```
 <context-left>                                  ⎇ <branch> <state>    tray <n>    [<hint>]
```

| Segment   | Contents                                                              | Colour                    |
|-----------|-----------------------------------------------------------------------|---------------------------|
| mode badge| `NOR INS VIS CMD REP` — shown for every focus, not just editor        | mode-coloured bg          |
| context   | Editor: `<file><+/->  ln:col`. Claude/shell: `<type> · <name>  <state>`. In `CMD` mode: `:<buffer>▌` replaces the whole left side. | fg, state text in `warn`/`attn` |
| git       | `⎇ <branch> <↑n ↓n>  M<n> ?<n>`                                      | dim `ok`/`warn`/`err`     |
| tray      | `tray <n>` (hidden when 0)                                            | `attn` if any tray unread |
| hint      | 1–3 most relevant keys                                                | `dim`                     |

When nothing needs to be said (clean git, no tray, normal mode), a
segment disappears instead of printing zeros.

---

## 6. Input Model (modal, vim-style)

Input is routed by the current mode (§2.8). Leader: `Space`.

### 6.1 Normal mode (default)

| Key                 | Action                                             |
|---------------------|----------------------------------------------------|
| `Tab` / `Shift+Tab` | Cycle main cells (claude / shell / edit). From a side panel, returns to the last main cell. |
| `<leader>f/g/p`     | Jump to Files / Git / Projects side panel           |
| `Ctrl-h/j/k/l`      | Directional focus movement (explicit)              |
| `←/→/↑/↓`           | Directional focus movement                          |
| `h/j/k/l`           | Move selection within focused panel (file tree, editor cursor) — bare hjkl is reserved for the panel, never for focus movement, so it doesn't clash with editor normal-mode bindings |
| `i` / `a`           | Enter **Insert** (if focused cell supports it)     |
| `:`                 | Enter **Command** (§4.10)                          |
| `v` / `R`           | Enter Visual / Replace (editor only; later)        |
| `<leader>-`         | Split focused cell horizontally                    |
| `<leader>\`         | Split focused cell vertically                      |
| `<leader>z`         | Zoom / un-zoom focused cell                        |
| `<leader>t`         | New tab in focused cell                            |
| `<leader>sc/ss/se`  | New cell: claude / shell / editor                  |
| `<leader>w`         | Swap / move a cell                                 |
| `<leader>m`         | Minimize focused cell                              |
| `<leader>e`         | Toggle files+git column (shown / hidden)           |
| `<leader>1..9`      | Switch project                                     |
| `q`                 | Quit (if no unsaved buffers). `Ctrl-C` always quits.|

### 6.2 Insert mode

Keys pass through to the focused cell: typing into an editor buffer,
into a claude PTY prompt, or into a shell PTY.

| Key   | Action                                   |
|-------|------------------------------------------|
| `Esc` | Back to Normal                           |
| all others | Sent to the focused session         |

Focus movement is intentionally disabled in Insert — press `Esc` first.
This matches tmux's "prefix mode" behaviour and keeps key passthrough
lossless.

### 6.3 Command mode

| Key         | Action                                  |
|-------------|-----------------------------------------|
| typing      | Add char to command buffer              |
| `Backspace` | Delete previous char                    |
| `Tab`       | Accept highlighted completion           |
| `↑/↓`       | Move through completion / history       |
| `Enter`     | Run command, back to Normal             |
| `Esc`       | Cancel, back to Normal                  |

See §4.10 for the initial command list.

### 6.4 Auto-demotion

When focus changes to a cell that doesn't support the active mode, the
mode drops to `Normal`. E.g.: typing in `shell` (Insert) → `Ctrl-h` to
`git` → mode becomes `Normal` automatically. This is app-level; the
user never sees a "mode unavailable" error.

---

## 7. Data Model Sketch

```
App
 ├── projects: Vec<Project>
 ├── active_project: usize
 └── ui: UiState { focus, selection, tray, overlays, mode, theme }

Project
 ├── root: PathBuf
 ├── layout: SplitTree
 ├── sessions: Slab<Session>
 ├── file_tree: FileTreeCache
 ├── git: GitState
 └── aggregate_state: ProjectState   // derived: ○ ● ◐ ◉ ✕

ProjectState = NoSession | Idle | Working | Attention | Error

SplitTree = Leaf(CellId) | Split { dir, ratio, children }

Session = Claude(Pty) | Shell(Pty) | Editor(Buffer) | Diff(DiffView) | Preview(View)
         └─ each session carries its own SessionState that rolls up into ProjectState
```

Rendering: full re-layout every frame, diffed to the terminal by the TUI
crate. Async tasks (PTY output, file watchers, git refresh) push events
into a single channel; event loop drains, mutates state, redraws.

---

## 8. Language / Framework Recommendation

### 8.1 Options considered

| Stack                          | Pros                                                      | Cons                                                         |
|--------------------------------|-----------------------------------------------------------|--------------------------------------------------------------|
| **Rust + ratatui + crossterm** | Windows + Linux + macOS first-class. Mature widgets. Tokio fits PTY multiplexing. Precedent: zellij, helix, gitui. | Compile times; Rust learning curve.       |
| Go + Bubbletea/Lipgloss        | Fast to prototype, great styling.                          | Goroutine-per-thing awkward with many PTYs.                  |
| C + ncurses                    | Lowest level, tiny binary.                                 | Windows = pain. Hand-roll PTY multiplex, async, Unicode, widgets. Months before first pixel. |
| C + notcurses                  | Beautiful output.                                          | Windows second-class. Same "build everything" problem.       |
| C++ + FTXUI                    | Declarative, cross-platform.                               | Manual async; PTY + HTTP need extras.                        |
| Zig + libvaxis                 | Modern systems lang.                                       | Small ecosystem, no editor/tree widgets.                     |

### 8.2 Recommendation: **Rust + ratatui + crossterm**

Core crates:

- `ratatui` — layout, widgets, frame diffing
- `crossterm` — input events, raw mode, Windows + Unix
- `tokio` — async runtime
- `portable-pty` — spawn `claude`, shells, anything else, cross-platform
- `vt100` / `alacritty_terminal` — parse PTY output (ANSI, colour)
- `tui-textarea` — base of the editor cell
- `tree-sitter` + `tree-sitter-highlight` — syntax highlighting (§3.5)
- `git2` (libgit2) — git panel without shelling out
- `notify` — file-tree invalidation
- `ignore` — fuzzy-finder backing

Because the Claude cell is a PTY running the `claude` binary, there's
**no HTTP / SSE / auth code** to write in ACodeTerm. That work stays in
the CLI.

### 8.3 Milestones

1. Static layout skeleton + heavy-border focus + `Ctrl-h/j/k/l`.
2. Theme loader (§3 tokens) + status bar driven by focus + git state.
3. Shell cell via `portable-pty` + `vt100`.
4. Split tree + minimize tray.
5. Claude cell = same PTY path with `claude` command.
6. Editor cell (tui-textarea) + vim modes + tree-sitter colouring.
7. File explorer + git-status colouring + open → editor.
8. Git panel via `git2` (small + grow-upward).
9. Projects + project rail (normal + wide) + state-dot aggregator.
10. Command mode (`:` line) with core commands + swap mode + per-project layout persistence.
11. Theming config, optional plugin hooks.

---

## 9. Config (`.acoderc`)

ACodeTerm reads two config files, in order, with the second overriding the
first:

1. `~/.acoderc`   — user-level defaults
2. `./.acoderc`   — project-level override (CWD at launch)

Format is a TOML-compatible subset (`key = "value"`, `#` comments,
plus array form for argv). The implementation is hand-parsed in
`src/config.rs`; when we need nesting we upgrade to a real TOML parser
without breaking existing files.

**Shell resolution order** (see `session::default_shell_command`):

1. `ACODETERM_SHELL` env var (highest — handy for one-off testing)
2. `shell = "…"` (or array) in `.acoderc`
3. Platform default: `powershell -NoLogo` on Windows, `$SHELL` on Unix

**Claude resolution order** mirrors shell, using `ACODETERM_CLAUDE` /
`claude = "…"` / bare `claude`. When `claude_skip_permissions = true`,
`--dangerously-skip-permissions` is appended to the resolved argv.

**Scalar vs array argv.** A scalar value is CMD-style split: quoted
substrings (`"…"` or `'…'`) keep their whitespace, bare substrings split
on whitespace. Backslashes are preserved literally — unlike a POSIX
shell, `\P` means `\P`, not `P`. For a path whose whole value contains
spaces you can just quote the whole scalar. If you need to mix a spaced
path with separate args, use the array form instead.

```toml
# ~/.acoderc
shell  = "powershell -NoLogo"                                   # 2 tokens
shell  = "C:\Program Files\Git\usr\bin\bash.exe"                # 1 token (quoted whole)
shell  = ["C:\Program Files\Git\usr\bin\bash.exe", "-i"]        # 2 tokens (array)
claude = "claude"
claude_skip_permissions = true

# shell = "wsl"          # use WSL
# shell = "cmd.exe"      # old-school
```

Future keys (not yet implemented): `theme`, `leader`, editor bindings,
per-project session defaults.

---

## 10. Open Questions

- **Persistence** — serialize layout + sessions per project to
  `.acodeterm/session.toml`?
- **Config format** — stay with hand-parsed TOML-subset vs. pull in the
  `toml` crate when the schema grows?
- **LSP** — first-class or plugin-only?
- **Remote sessions** — SSH-hosted cells? (Probably later.)
- **Binary name** — `acodeterm` long; `act` as the short alias?

---

## 11. Status

Shipped so far: milestones 1–3 from §8.3 (layout skeleton + focus, themed
status bar, shell PTY with DSR handshake + size-tracking), plus the vim
mode system (§2.8) and `.acoderc` config (§9).

Next: milestone 5 (claude cell — reuses the PTY pipeline, just spawns
`claude`). Then milestone 4 (split tree + tray).
