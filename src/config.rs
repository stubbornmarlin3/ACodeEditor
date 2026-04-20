//! User-config loader.
//!
//! Reads `~/.acerc` (user-level) and then `./.acerc` (project-level)
//! from the working directory, with project-level overriding user-level.
//! Format is a small, TOML-compatible `key = value` subset.
//!
//! Supported keys:
//!   shell                   = "powershell -NoLogo"                          (scalar, CMD-style split)
//!   shell                   = ["C:\Program Files\Git\bin\bash.exe", "-i"]   (array, exact argv)
//!   claude                  = "claude"
//!   claude_skip_permissions = true
//!   layout                  = "master-bottom" | "master-right"
//!                                    Default cell tiling. `mb` / `mr`
//!                                    short forms also accepted;
//!                                    `master-stack` / `ms` alias for
//!                                    `master-right` (previous default).
//!   on_launch               = "welcome" | "cwd" | "global"
//!                                    What `ace` with no args opens.
//!                                    • welcome (default) — landing page, no
//!                                      project rail. Add projects manually.
//!                                    • cwd     — cwd becomes the sole session
//!                                      project; global list is ignored.
//!                                    • global  — load `~/.ace/projects.toml`.
//!
//! Scalar form splits on whitespace *outside* quoted substrings, so
//! `shell = "\"C:\Program Files\...\" -i"` won't work (nested `\"`
//! isn't escape-processed) but `shell = "C:\Program Files\..."` DOES
//! stay as one token because the whole value is a single quoted string.
//! Use array form whenever you need to mix a spaced path with extra args.
//! Backslashes are preserved literally throughout.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct Config {
    pub shell:                   Option<Vec<String>>,
    pub claude:                  Option<Vec<String>>,
    pub claude_skip_permissions: Option<bool>,
    /// What `ace` with no args opens. Three values:
    ///   * `"welcome"` (default) — landing page, no project rail.
    ///   * `"cwd"`               — current dir as the sole project.
    ///   * `"global"`            — load the saved `~/.ace/projects.toml`.
    /// Anything else silently falls back to `welcome`.
    pub on_launch:               Option<String>,
    /// Default cell layout. Accepts the same names as `:layout`
    /// (`master-bottom|mb`, `master-right|mr`, plus the alias
    /// `master-stack|ms` for back-compat).
    pub layout:                  Option<String>,
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        if let Some(home) = home_dir() {
            c.merge_file(&home.join(".acerc"));
        }
        c.merge_file(Path::new(".acerc"));
        c
    }

    fn merge_file(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let other = parse(&content);
        if other.shell.is_some() {
            self.shell = other.shell;
        }
        if other.claude.is_some() {
            self.claude = other.claude;
        }
        if other.claude_skip_permissions.is_some() {
            self.claude_skip_permissions = other.claude_skip_permissions;
        }
        if other.on_launch.is_some() {
            self.on_launch = other.on_launch;
        }
        if other.layout.is_some() {
            self.layout = other.layout;
        }
    }
}

fn parse(content: &str) -> Config {
    let mut c = Config::default();
    for raw in content.lines() {
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let raw_value = value.trim();
        match key {
            "shell"  => c.shell  = parse_argv(raw_value),
            "claude" => c.claude = parse_argv(raw_value),
            "claude_skip_permissions" => c.claude_skip_permissions = parse_bool(raw_value),
            "on_launch" => c.on_launch = Some(strip_outer_quotes(raw_value.trim()).trim().to_string()),
            "layout"    => c.layout    = Some(strip_outer_quotes(raw_value.trim()).trim().to_string()),
            _ => {}
        }
    }
    c
}

/// Strip a `#` line comment, respecting quotes so `#` inside a string is kept.
fn strip_line_comment(s: &str) -> &str {
    let mut in_quote = false;
    let mut quote_char = '"';
    for (i, ch) in s.char_indices() {
        if in_quote {
            if ch == quote_char {
                in_quote = false;
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            quote_char = ch;
        } else if ch == '#' {
            return &s[..i];
        }
    }
    s
}

/// Parse a value as an argv list. Accepts either:
///   - scalar: `"powershell -NoLogo"` or `"C:\Path With Space\bash.exe"`
///     → CMD-style split: quoted substrings preserve whitespace, bare
///     substrings split on whitespace.
///   - array:  `["a", "b with space"]` → exact tokens
pub fn parse_argv(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        parse_array(raw)
    } else {
        let parts = split_scalar(raw);
        if parts.is_empty() || parts[0].is_empty() {
            None
        } else {
            Some(parts)
        }
    }
}

/// CMD-style token splitter. `"..."` and `'...'` preserve whitespace
/// within the current token (they also START a token). Backslashes are
/// kept verbatim — we never consume them as escapes, since that would
/// break Windows paths.
fn split_scalar(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut in_quote = false;
    let mut quote_char = '"';
    for ch in s.chars() {
        if in_quote {
            if ch == quote_char {
                in_quote = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            in_token = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if in_token {
                out.push(std::mem::take(&mut cur));
                in_token = false;
            }
        } else {
            cur.push(ch);
            in_token = true;
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

fn parse_array(s: &str) -> Option<Vec<String>> {
    let trimmed = s.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut quote_char = '"';
    let mut had_quote = false;
    for ch in inner.chars() {
        if in_quote {
            if ch == quote_char {
                in_quote = false;
                items.push(std::mem::take(&mut cur));
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            had_quote = true;
            quote_char = ch;
        } else if ch == ',' || ch.is_whitespace() {
            // separator — ignore
        } else {
            // bare tokens inside an array are not supported
            return None;
        }
    }
    if in_quote || !had_quote {
        return None;
    }
    Some(items)
}

pub fn strip_outer_quotes(s: &str) -> &str {
    for (open, close) in [('"', '"'), ('\'', '\'')] {
        if s.len() >= 2 && s.starts_with(open) && s.ends_with(close) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn parse_bool(s: &str) -> Option<bool> {
    let v = strip_outer_quotes(s.trim()).trim().to_ascii_lowercase();
    match v.as_str() {
        "true"  | "yes" | "on"  | "1" => Some(true),
        "false" | "no"  | "off" | "0" => Some(false),
        _ => None,
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
