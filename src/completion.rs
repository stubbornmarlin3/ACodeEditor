//! Tab-completion for the `:`-command line.
//!
//! Pure function: `complete(buffer, ctx)` returns a list of candidate
//! replacements for the buffer's trailing token. The caller splices the
//! chosen candidate into `buffer[start..]`. The function never reads or
//! mutates app state beyond what the `CompletionCtx` hands it.
//!
//! What's covered:
//!   * top-level commands (when no space yet)
//!   * `:new <kind>` / `:tab <kind>` → shell|claude|edit, then path
//!   * `:e <path>` / `:w <path>` / `:edit <path>` / `:write <path>` (+ `!`)
//!   * `:set <wrap|nowrap|list|nolist>`
//!   * `:layout <master-bottom|mb|master-right|mr>`
//!   * `:proj <sub> [name|path]`
//!   * `:git <sub> [branch|path|stash-sub]`
//!
//! Everything else returns an empty completion (no candidates).

use std::path::Path;

/// Context the caller supplies. Kept small — borrows slices so no
/// allocation is needed to invoke us.
pub struct CompletionCtx<'a> {
    pub cwd: &'a Path,
    pub projects: &'a [String],
    pub branches: &'a [String],
    pub changed_paths: &'a [String],
}

/// Result of a completion attempt.
#[derive(Default, Debug, Clone)]
pub struct Completion {
    /// Byte offset into `buffer` where the replacement starts. Splice
    /// `options[i]` into `buffer[start..]`.
    pub start: usize,
    /// Candidates, already sorted case-insensitively. Each candidate is
    /// a *full* replacement string (path completion returns the whole
    /// path from `start`, not just the leaf).
    pub options: Vec<String>,
}

/// Top-level ex commands we complete. Order here becomes presentation
/// order, so put the common ones first. (We sort before returning, so
/// this really is just a listing — filtering still happens.)
const TOP_LEVEL: &[&str] = &[
    "c", "claude",
    "close", "bd", "bdelete",
    "conflict", "resolve",
    "edit", "edit!", "e", "e!",
    "git",
    "help", "h",
    "layout",
    "new",
    "nohl", "nohlsearch", "noh",
    "proj", "project", "projects",
    "q", "q!", "Q", "Q!", "quit", "quit!", "Quit", "Quit!",
    "set",
    "min", "minimize",
    "restore",
    "s", "shell",
    "split",
    "sudo",
    "swap",
    "tab",
    "w", "w!", "w!q", "write", "write!",
    "wq", "wQ", "wQ!", "x", "x!",
];

/// `:sudo <sub>` sub-commands. Matches the set that `parse_sudo_command`
/// in app.rs accepts, minus the bang aliases (those live at top level).
const SUDO_SUBS: &[&str] = &["w", "wq", "wQ", "x"];

/// Public entry point.
pub fn complete(buffer: &str, ctx: &CompletionCtx) -> Completion {
    if buffer.is_empty() { return Completion::default(); }
    // Search prompts (`/pat` / `?pat`) are free-text — no completion.
    let Some(first) = buffer.chars().next() else { return Completion::default(); };
    if first == '/' || first == '?' { return Completion::default(); }
    // Plain `:42` — numeric goto, nothing to complete.
    if buffer.chars().all(|c| c.is_ascii_digit()) { return Completion::default(); }

    let (token_start, token) = last_token(buffer);
    let stem = buffer[..token_start].trim_end();

    // No stem → top-level command.
    if stem.is_empty() {
        return Completion {
            start: token_start,
            options: filter_sorted(TOP_LEVEL, token),
        };
    }

    // Parse the stem into head + preceding args (only fully-typed words
    // appear in `args`; the incomplete trailing token is `token`).
    let mut words = stem.split_whitespace();
    let head = words.next().unwrap_or("");
    let args: Vec<&str> = words.collect();

    match head {
        "set"    => starts_with_into(&["wrap", "nowrap", "list", "nolist"], token, token_start),
        "layout" => starts_with_into(
            &["master-bottom", "mb", "master-right", "mr", "master-stack", "ms"],
            token, token_start,
        ),

        // `:e`/`:edit` opens files — directories can't be opened, so we
        // don't surface them. `:w <path>` still shows dirs (you might
        // want to drill into `some/dir/` and write a new file inside).
        "e" | "edit" | "e!" | "edit!" => {
            Completion { start: token_start, options: path_options(token, ctx.cwd, true) }
        }
        "w" | "write" | "w!" | "write!" => {
            Completion { start: token_start, options: path_options(token, ctx.cwd, false) }
        }

        "new" | "tab" => {
            if args.is_empty() {
                starts_with_into(&["shell", "claude", "edit"], token, token_start)
            } else {
                // `edit` takes a file (directories aren't openable);
                // `shell`/`claude` aren't wired to a cwd arg yet so we
                // show nothing rather than hint at a feature that
                // silently no-ops.
                match args[0] {
                    "edit" => Completion { start: token_start, options: path_options(token, ctx.cwd, true) },
                    _      => Completion::default(),
                }
            }
        }

        "proj" | "project" | "projects" => complete_proj(&args, token, token_start, ctx),

        "git" => complete_git(&args, token, token_start, ctx),

        // `:sudo <sub>` — only the first arg is suggested; the sub-command
        // itself doesn't take a path (the focused editor's path is used).
        "sudo" if args.is_empty() => starts_with_into(SUDO_SUBS, token, token_start),

        _ => Completion::default(),
    }
}

// ── sub-dispatchers ──────────────────────────────────────────────────

fn complete_proj(args: &[&str], token: &str, start: usize, ctx: &CompletionCtx) -> Completion {
    const SUBS: &[&str] = &[
        "add", "cd", "list", "ls", "refresh", "remove", "rename", "rm",
        "switch", "sw", "use",
    ];
    if args.is_empty() {
        return starts_with_into(SUBS, token, start);
    }
    match args[0] {
        "add" => Completion { start, options: path_options(token, ctx.cwd, false) },
        "rm" | "remove" | "switch" | "sw" | "use" | "cd" => {
            let names: Vec<&str> = ctx.projects.iter().map(String::as_str).collect();
            starts_with_into(&names, token, start)
        }
        _ => Completion::default(),
    }
}

fn complete_git(args: &[&str], token: &str, start: usize, ctx: &CompletionCtx) -> Completion {
    const SUBS: &[&str] = &[
        "abort", "add", "amend", "branch", "branches", "cherry-pick",
        "checkout", "commit", "continue", "cp", "delete", "delete!",
        "discard", "fetch", "init", "log", "merge", "pull", "push",
        "rebase", "refresh", "remote", "reset", "revert", "stage",
        "stage-all", "stash", "status", "switch", "unstage", "unstage-all",
    ];
    if args.is_empty() {
        return starts_with_into(SUBS, token, start);
    }
    match args[0] {
        "switch" | "checkout" | "delete" | "delete!" | "branch" | "merge" | "rebase" | "cherry-pick" | "cp" | "revert" => {
            let names: Vec<&str> = ctx.branches.iter().map(String::as_str).collect();
            starts_with_into(&names, token, start)
        }
        "stage" | "add" | "unstage" | "reset" | "discard" => {
            let paths: Vec<&str> = ctx.changed_paths.iter().map(String::as_str).collect();
            starts_with_into(&paths, token, start)
        }
        "stash" if args.len() == 1 => {
            starts_with_into(&["apply", "drop", "list", "pop", "push", "save"], token, start)
        }
        _ => Completion::default(),
    }
}

// ── helpers ──────────────────────────────────────────────────────────

/// Return `(byte_start_of_token, token)` for the trailing word of `buf`.
/// "Token" means everything after the last ASCII space. If the buffer
/// ends with a space (or is empty), the token is empty and `start` is
/// `buf.len()` — which is what we want for "complete from scratch here".
fn last_token(buf: &str) -> (usize, &str) {
    match buf.rfind(' ') {
        Some(i) => (i + 1, &buf[i + 1..]),
        None    => (0, buf),
    }
}

fn filter_sorted(all: &[&str], prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = all
        .iter()
        .filter(|s| s.starts_with(prefix))
        .map(|s| (*s).to_string())
        .collect();
    out.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    out.dedup();
    out
}

fn starts_with_into(all: &[&str], prefix: &str, start: usize) -> Completion {
    Completion { start, options: filter_sorted(all, prefix) }
}

/// Filesystem path completion.
///
/// Splits `token` at the last `/` or `\`. The left side is the directory
/// to list (relative to `cwd` when it's not absolute); the right side is
/// the prefix to filter entries by. Each returned option is the
/// *full* token that should replace the current one: directory portion
/// re-prepended, trailing `/` appended for directories so the user can
/// Tab deeper without typing the separator.
fn path_options(token: &str, cwd: &Path, files_only: bool) -> Vec<String> {
    let (dir_part, leaf_prefix) = split_path_token(token);

    // `~` / `~/…` in the directory portion resolves against the user's
    // home directory so completion mirrors what the command will do when
    // it runs (see `app::expand_tilde`). The returned options keep the
    // `~` prefix verbatim so the user's input shape is preserved.
    let expanded = expand_tilde_for_completion(dir_part);
    let dir_path = if expanded.is_empty() {
        cwd.to_path_buf()
    } else if Path::new(&expanded).is_absolute() {
        Path::new(&expanded).to_path_buf()
    } else {
        cwd.join(&expanded)
    };

    let Ok(rd) = std::fs::read_dir(&dir_path) else {
        return Vec::new();
    };

    let mut out: Vec<(bool, String)> = Vec::new();
    for entry in rd.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !name.starts_with(leaf_prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if files_only && is_dir { continue; }
        let mut combined = String::with_capacity(dir_part.len() + name.len() + 1);
        combined.push_str(dir_part);
        combined.push_str(&name);
        if is_dir { combined.push('/'); }
        out.push((is_dir, combined));
    }
    // Directories first, then files, each group sorted case-insensitively.
    out.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| a.1.to_ascii_lowercase().cmp(&b.1.to_ascii_lowercase()))
    });
    out.into_iter().map(|(_, s)| s).collect()
}

/// Resolve a leading `~` / `~/` / `~\` in the directory portion of a
/// path token. Falls back to the raw string when no home dir is set, so
/// completion degrades gracefully instead of silently listing `cwd`.
fn expand_tilde_for_completion(dir_part: &str) -> String {
    if dir_part == "~" {
        if let Some(h) = home_dir_str() {
            return h;
        }
        return dir_part.to_string();
    }
    if let Some(rest) = dir_part.strip_prefix("~/").or_else(|| dir_part.strip_prefix("~\\")) {
        if let Some(h) = home_dir_str() {
            let sep = if cfg!(windows) { '\\' } else { '/' };
            return format!("{h}{sep}{rest}");
        }
    }
    dir_part.to_string()
}

fn home_dir_str() -> Option<String> {
    std::env::var("HOME").ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())
}

/// Split a path-ish token into `(dir_part_including_trailing_sep, leaf_prefix)`.
/// `"src/ma" → ("src/", "ma")`, `"src/" → ("src/", "")`, `"ma" → ("", "ma")`,
/// `"./" → ("./", "")`, `"C:\\Users\\" → ("C:\\Users\\", "")`.
fn split_path_token(token: &str) -> (&str, &str) {
    let last_sep = token.rfind(|c| c == '/' || c == '\\');
    match last_sep {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None    => ("", token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> CompletionCtx<'static> {
        static PROJECTS: &[String] = &[];
        static BRANCHES: &[String] = &[];
        static PATHS: &[String] = &[];
        CompletionCtx {
            cwd: Path::new("."),
            projects: PROJECTS,
            branches: BRANCHES,
            changed_paths: PATHS,
        }
    }

    #[test]
    fn top_level_filters_by_prefix() {
        let c = complete("q", &ctx());
        assert_eq!(c.start, 0);
        assert!(c.options.iter().any(|s| s == "q"));
        assert!(c.options.iter().any(|s| s == "quit"));
        assert!(c.options.iter().all(|s| s.starts_with('q') || s.starts_with('Q')));
    }

    #[test]
    fn search_prefix_gives_nothing() {
        assert!(complete("/foo", &ctx()).options.is_empty());
        assert!(complete("?bar", &ctx()).options.is_empty());
    }

    #[test]
    fn new_kind_suggestions() {
        let c = complete("new ", &ctx());
        assert_eq!(c.start, 4);
        assert_eq!(c.options, vec!["claude", "edit", "shell"]);
        let c = complete("new sh", &ctx());
        assert_eq!(c.options, vec!["shell"]);
    }

    #[test]
    fn set_suggestions() {
        let c = complete("set ", &ctx());
        assert_eq!(c.options, vec!["list", "nolist", "nowrap", "wrap"]);
    }

    #[test]
    fn project_switch_names() {
        let projects = vec!["alpha".to_string(), "beta".to_string(), "alabama".to_string()];
        let ctx = CompletionCtx {
            cwd: Path::new("."),
            projects: &projects,
            branches: &[],
            changed_paths: &[],
        };
        let c = complete("proj switch al", &ctx);
        assert_eq!(c.start, "proj switch ".len());
        assert_eq!(c.options, vec!["alabama", "alpha"]);
    }

    #[test]
    fn path_split() {
        assert_eq!(split_path_token("src/ma"), ("src/", "ma"));
        assert_eq!(split_path_token("src/"), ("src/", ""));
        assert_eq!(split_path_token("ma"), ("", "ma"));
        assert_eq!(split_path_token(""), ("", ""));
    }

    #[test]
    fn last_token_empty_after_trailing_space() {
        let (s, t) = last_token("foo ");
        assert_eq!(s, 4);
        assert_eq!(t, "");
    }

    #[test]
    fn git_subcommands() {
        let c = complete("git st", &ctx());
        assert!(c.options.iter().any(|s| s == "stage"));
        assert!(c.options.iter().any(|s| s == "status"));
        assert!(c.options.iter().any(|s| s == "stash"));
    }

    #[test]
    fn git_stash_subcommands() {
        let c = complete("git stash p", &ctx());
        assert_eq!(c.options, vec!["pop", "push"]);
    }

    #[test]
    fn numeric_goto_no_completion() {
        assert!(complete("42", &ctx()).options.is_empty());
    }

    // Smoke test path completion against a real temp dir so the read_dir
    // branch is exercised.
    #[test]
    fn path_completion_lists_cwd() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("ace-comp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("alpha.txt"), b"").unwrap();
        fs::write(tmp.join("albatross.txt"), b"").unwrap();
        fs::create_dir(tmp.join("almond")).unwrap();

        let ctx = CompletionCtx {
            cwd: &tmp,
            projects: &[],
            branches: &[],
            changed_paths: &[],
        };
        // `:e` is files-only — almond/ must be filtered out.
        let c = complete("e al", &ctx);
        assert_eq!(c.start, "e ".len());
        assert!(!c.options.iter().any(|s| s == "almond/"));
        assert!(c.options.iter().any(|s| s == "alpha.txt"));
        assert!(c.options.iter().any(|s| s == "albatross.txt"));

        // `:w` includes dirs so you can drill into `some/dir/` and
        // write a new file inside.
        let c = complete("w al", &ctx);
        assert!(c.options.iter().any(|s| s == "almond/"));

        // `:new edit` is files-only, same as `:e`.
        let c = complete("new edit al", &ctx);
        assert!(!c.options.iter().any(|s| s == "almond/"));
        assert!(c.options.iter().any(|s| s == "alpha.txt"));

        // `:new shell` doesn't complete a path at all (the arg is
        // currently ignored by build_session).
        let c = complete("new shell al", &ctx);
        assert!(c.options.is_empty());

        fs::remove_dir_all(&tmp).ok();
        let _ = PathBuf::new();
    }
}
