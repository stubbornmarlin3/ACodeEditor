use ratatui::style::Color;
use std::path::Path;
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};
use tree_sitter_language_pack::detect_language_from_extension;

/// Highlight capture names we recognize. Helix queries emit a rich taxonomy
/// (`function.macro`, `type.builtin`, `keyword.control.return`, ...); if a
/// capture isn't listed here `tree_sitter_highlight` falls back to its
/// longest-matching prefix, so entries like `keyword` catch everything
/// keyword-ish that we don't break out explicitly.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",              // 0
    "comment",                // 1
    "constant",               // 2
    "constant.builtin",       // 3
    "constructor",            // 4
    "escape",                 // 5
    "function",               // 6
    "function.builtin",       // 7
    "function.method",        // 8
    "keyword",                // 9
    "keyword.function",       // 10
    "keyword.operator",       // 11
    "keyword.return",         // 12
    "label",                  // 13
    "namespace",              // 14
    "number",                 // 15
    "operator",               // 16
    "property",               // 17
    "punctuation",            // 18
    "punctuation.bracket",    // 19
    "punctuation.delimiter",  // 20
    "string",                 // 21
    "string.special",         // 22
    "tag",                    // 23
    "type",                   // 24
    "type.builtin",           // 25
    "variable",               // 26
    "variable.builtin",       // 27
    "variable.parameter",     // 28
    "markup.heading",         // 29
    "markup.heading.1",       // 30
    "markup.heading.2",       // 31
    "markup.heading.3",       // 32
    "markup.heading.4",       // 33
    "markup.heading.5",       // 34
    "markup.heading.6",       // 35
    "markup.heading.marker",  // 36
    "markup.raw",             // 37
    "markup.raw.block",       // 38
    "markup.raw.inline",      // 39
    "markup.link",            // 40
    "markup.link.label",      // 41
    "markup.link.url",        // 42
    "markup.list",            // 43
    "markup.list.numbered",   // 44
    "markup.list.unnumbered", // 45
    "markup.list.checked",    // 46
    "markup.list.unchecked",  // 47
    "markup.quote",           // 48
    "markup.bold",            // 49
    "markup.italic",          // 50
    "markup.strikethrough",   // 51
    "string.escape",          // 52
    "string.special.url",     // 53
    "punctuation.special",    // 54
    "constant.numeric",          // 55 — helix queries never use @number
    "constant.numeric.float",    // 56
    "constant.numeric.integer",  // 57
    "constant.character",        // 58
    "constant.character.escape", // 59
    "comment.documentation",     // 60
    "function.macro",            // 61
    "type.enum.variant",         // 62
    "variable.other.member",     // 63
    "string.regexp",             // 64
    "apply",                     // 65 — CSS @apply
    "import",                    // 66 — CSS @import
    "charset",                   // 67 — CSS @charset
    "keyframes",                 // 68 — CSS @keyframes
    "media",                     // 69 — CSS @media
    "supports",                  // 70 — CSS @supports
    "error",                     // 71
    "warning",                   // 72
    "info",                      // 73
    "embedded",                  // 74
    "special",                   // 75
    "name",                      // 76 — SQL identifier
];

fn color_for(h: Highlight) -> Color {
    match h.0 {
        0  => Color::Rgb(0xff, 0x88, 0x44), // attribute        orange
        1  => Color::Rgb(0x6a, 0x72, 0x8a), // comment          gray-blue
        2  => Color::Rgb(0xd0, 0x95, 0xff), // constant         lavender
        3  => Color::Rgb(0xd0, 0x95, 0xff), // constant.builtin
        4  => Color::Rgb(0x6a, 0x9c, 0xff), // constructor      blue
        5  => Color::Rgb(0xff, 0xd0, 0x80), // escape           amber
        6  => Color::Rgb(0x6a, 0x9c, 0xff), // function         blue
        7  => Color::Rgb(0x4f, 0xd1, 0xff), // function.builtin cyan
        8  => Color::Rgb(0x6a, 0x9c, 0xff), // function.method  blue
        9  => Color::Rgb(0xc0, 0x68, 0xff), // keyword          violet
        10 => Color::Rgb(0xc0, 0x68, 0xff), // keyword.function
        11 => Color::Rgb(0xc0, 0x68, 0xff), // keyword.operator
        12 => Color::Rgb(0xc0, 0x68, 0xff), // keyword.return
        13 => Color::Rgb(0xe6, 0xe8, 0xee), // label            fg
        14 => Color::Rgb(0x4f, 0xd1, 0xff), // namespace        cyan
        15 => Color::Rgb(0xf0, 0xa8, 0x50), // number           amber
        16 => Color::Rgb(0xe6, 0xe8, 0xee), // operator         fg
        17 => Color::Rgb(0xe6, 0xe8, 0xee), // property         fg
        18 => Color::Rgb(0xe6, 0xe8, 0xee), // punctuation      fg
        19 => Color::Rgb(0xe6, 0xe8, 0xee), // punctuation.bracket
        20 => Color::Rgb(0xe6, 0xe8, 0xee), // punctuation.delimiter
        21 => Color::Rgb(0x7c, 0xd9, 0x92), // string           green
        22 => Color::Rgb(0xf0, 0xa8, 0x50), // string.special   amber
        23 => Color::Rgb(0x6a, 0x9c, 0xff), // tag              blue
        24 => Color::Rgb(0x4f, 0xd1, 0xff), // type             cyan
        25 => Color::Rgb(0x4f, 0xd1, 0xff), // type.builtin
        26 => Color::Rgb(0xe6, 0xe8, 0xee), // variable         fg
        27 => Color::Rgb(0xd0, 0x95, 0xff), // variable.builtin lavender
        28 => Color::Rgb(0xff, 0xa0, 0x70), // variable.parameter salmon
        29 => Color::Rgb(0x6a, 0x9c, 0xff), // markup.heading           blue
        30 => Color::Rgb(0xff, 0xa8, 0x60), // markup.heading.1         orange (h1)
        31 => Color::Rgb(0xff, 0xc0, 0x70), // markup.heading.2         amber  (h2)
        32 => Color::Rgb(0xff, 0xd8, 0x80), // markup.heading.3         gold   (h3)
        33 => Color::Rgb(0xc0, 0x68, 0xff), // markup.heading.4         violet (h4)
        34 => Color::Rgb(0x6a, 0x9c, 0xff), // markup.heading.5         blue   (h5)
        35 => Color::Rgb(0x4f, 0xd1, 0xff), // markup.heading.6         cyan   (h6)
        36 => Color::Rgb(0x6a, 0x72, 0x8a), // markup.heading.marker    dim
        37 => Color::Rgb(0x7c, 0xd9, 0x92), // markup.raw               green
        38 => Color::Rgb(0x7c, 0xd9, 0x92), // markup.raw.block
        39 => Color::Rgb(0x7c, 0xd9, 0x92), // markup.raw.inline
        40 => Color::Rgb(0x4f, 0xd1, 0xff), // markup.link              cyan
        41 => Color::Rgb(0x6a, 0x9c, 0xff), // markup.link.label        blue
        42 => Color::Rgb(0x4f, 0xd1, 0xff), // markup.link.url          cyan
        43 => Color::Rgb(0xff, 0x88, 0x44), // markup.list              orange
        44 => Color::Rgb(0xff, 0x88, 0x44), // markup.list.numbered
        45 => Color::Rgb(0xff, 0x88, 0x44), // markup.list.unnumbered
        46 => Color::Rgb(0x7c, 0xd9, 0x92), // markup.list.checked      green
        47 => Color::Rgb(0xff, 0xa8, 0x60), // markup.list.unchecked    amber
        48 => Color::Rgb(0x6a, 0x72, 0x8a), // markup.quote             dim
        49 => Color::Rgb(0xe6, 0xe8, 0xee), // markup.bold              fg (bold modifier not applied, color only)
        50 => Color::Rgb(0xe6, 0xe8, 0xee), // markup.italic
        51 => Color::Rgb(0x6a, 0x72, 0x8a), // markup.strikethrough     dim
        52 => Color::Rgb(0xff, 0xd0, 0x80), // string.escape            amber
        53 => Color::Rgb(0x4f, 0xd1, 0xff), // string.special.url       cyan
        54 => Color::Rgb(0xff, 0x88, 0x44), // punctuation.special      orange
        55 => Color::Rgb(0xf0, 0xa8, 0x50), // constant.numeric         amber
        56 => Color::Rgb(0xf0, 0xa8, 0x50), // constant.numeric.float
        57 => Color::Rgb(0xf0, 0xa8, 0x50), // constant.numeric.integer
        58 => Color::Rgb(0x7c, 0xd9, 0x92), // constant.character       green (char literal)
        59 => Color::Rgb(0xff, 0xd0, 0x80), // constant.character.escape amber
        60 => Color::Rgb(0x8a, 0x96, 0xb0), // comment.documentation    brighter than plain comment
        61 => Color::Rgb(0xff, 0x88, 0x44), // function.macro           orange
        62 => Color::Rgb(0xd0, 0x95, 0xff), // type.enum.variant        lavender
        63 => Color::Rgb(0xe6, 0xe8, 0xee), // variable.other.member    fg (struct field)
        64 => Color::Rgb(0xff, 0xa0, 0x70), // string.regexp            salmon
        65 => Color::Rgb(0xc0, 0x68, 0xff), // apply                    violet (keyword-like)
        66 => Color::Rgb(0xc0, 0x68, 0xff), // import
        67 => Color::Rgb(0xc0, 0x68, 0xff), // charset
        68 => Color::Rgb(0xc0, 0x68, 0xff), // keyframes
        69 => Color::Rgb(0xc0, 0x68, 0xff), // media
        70 => Color::Rgb(0xc0, 0x68, 0xff), // supports
        71 => Color::Rgb(0xff, 0x6a, 0x6a), // error                    red
        72 => Color::Rgb(0xf0, 0xa8, 0x50), // warning                  amber
        73 => Color::Rgb(0x4f, 0xd1, 0xff), // info                     cyan
        74 => Color::Rgb(0xe6, 0xe8, 0xee), // embedded                 fg
        75 => Color::Rgb(0xff, 0x88, 0x44), // special                  orange
        76 => Color::Rgb(0xe6, 0xe8, 0xee), // name                     fg
        _  => Color::Reset,
    }
}

/// Map a file path to a language-pack language name. The extension table
/// comes from tree-sitter-language-pack (covers 305 extensions), with a
/// small override layer for extension-less filenames.
fn detect_language(path: &Path) -> Option<&'static str> {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("Makefile" | "makefile" | "GNUmakefile" | "BSDmakefile") => return Some("make"),
        Some("Dockerfile" | "dockerfile" | "Containerfile") => return Some("dockerfile"),
        _ => {}
    }
    let ext = path.extension().and_then(|e| e.to_str())?;
    detect_language_from_extension(ext)
}

/// Bundled query strings vendored from helix-editor's runtime/queries
/// (MPL-2.0). The vendor script flattens `; inherits:` directives so we
/// don't need any runtime composition.
mod queries {
    macro_rules! q { ($($tt:tt)*) => { include_str!($($tt)*) }; }

    pub mod bash {
        pub const HIGHLIGHTS: &str = q!("../queries/bash/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/bash/injections.scm");
    }
    pub mod c {
        pub const HIGHLIGHTS: &str = q!("../queries/c/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/c/injections.scm");
        pub const LOCALS:     &str = q!("../queries/c/locals.scm");
    }
    pub mod cpp {
        pub const HIGHLIGHTS: &str = q!("../queries/cpp/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/cpp/injections.scm");
    }
    pub mod csharp {
        pub const HIGHLIGHTS: &str = q!("../queries/csharp/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/csharp/injections.scm");
    }
    pub mod css {
        pub const HIGHLIGHTS: &str = q!("../queries/css/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/css/injections.scm");
    }
    pub mod dart {
        pub const HIGHLIGHTS: &str = q!("../queries/dart/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/dart/injections.scm");
        pub const LOCALS:     &str = q!("../queries/dart/locals.scm");
    }
    pub mod dockerfile {
        pub const HIGHLIGHTS: &str = q!("../queries/dockerfile/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/dockerfile/injections.scm");
    }
    pub mod go {
        pub const HIGHLIGHTS: &str = q!("../queries/go/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/go/injections.scm");
        pub const LOCALS:     &str = q!("../queries/go/locals.scm");
    }
    pub mod haskell {
        pub const HIGHLIGHTS: &str = q!("../queries/haskell/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/haskell/injections.scm");
        pub const LOCALS:     &str = q!("../queries/haskell/locals.scm");
    }
    pub mod html {
        pub const HIGHLIGHTS: &str = q!("../queries/html/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/html/injections.scm");
    }
    pub mod java {
        pub const HIGHLIGHTS: &str = q!("../queries/java/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/java/injections.scm");
    }
    pub mod javascript {
        pub const HIGHLIGHTS: &str = q!("../queries/javascript/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/javascript/injections.scm");
        pub const LOCALS:     &str = q!("../queries/javascript/locals.scm");
    }
    pub mod json {
        pub const HIGHLIGHTS: &str = q!("../queries/json/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/json/injections.scm");
    }
    pub mod lua {
        pub const HIGHLIGHTS: &str = q!("../queries/lua/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/lua/injections.scm");
    }
    pub mod make {
        pub const HIGHLIGHTS: &str = q!("../queries/make/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/make/injections.scm");
    }
    pub mod markdown {
        pub const HIGHLIGHTS: &str = q!("../queries/markdown/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/markdown/injections.scm");
    }
    pub mod nix {
        pub const HIGHLIGHTS: &str = q!("../queries/nix/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/nix/injections.scm");
    }
    pub mod php {
        pub const HIGHLIGHTS: &str = q!("../queries/php/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/php/injections.scm");
    }
    pub mod python {
        pub const HIGHLIGHTS: &str = q!("../queries/python/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/python/injections.scm");
        pub const LOCALS:     &str = q!("../queries/python/locals.scm");
    }
    pub mod regex {
        pub const HIGHLIGHTS: &str = q!("../queries/regex/highlights.scm");
    }
    pub mod ruby {
        pub const HIGHLIGHTS: &str = q!("../queries/ruby/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/ruby/injections.scm");
        pub const LOCALS:     &str = q!("../queries/ruby/locals.scm");
    }
    pub mod rust {
        pub const HIGHLIGHTS: &str = q!("../queries/rust/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/rust/injections.scm");
        pub const LOCALS:     &str = q!("../queries/rust/locals.scm");
    }
    pub mod sql {
        pub const HIGHLIGHTS: &str = q!("../queries/sql/highlights.scm");
    }
    pub mod swift {
        pub const HIGHLIGHTS: &str = q!("../queries/swift/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/swift/injections.scm");
        pub const LOCALS:     &str = q!("../queries/swift/locals.scm");
    }
    pub mod toml {
        pub const HIGHLIGHTS: &str = q!("../queries/toml/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/toml/injections.scm");
    }
    pub mod tsx {
        pub const HIGHLIGHTS: &str = q!("../queries/tsx/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/tsx/injections.scm");
        pub const LOCALS:     &str = q!("../queries/tsx/locals.scm");
    }
    pub mod typescript {
        pub const HIGHLIGHTS: &str = q!("../queries/typescript/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/typescript/injections.scm");
        pub const LOCALS:     &str = q!("../queries/typescript/locals.scm");
    }
    pub mod yaml {
        pub const HIGHLIGHTS: &str = q!("../queries/yaml/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/yaml/injections.scm");
    }
    pub mod zig {
        pub const HIGHLIGHTS: &str = q!("../queries/zig/highlights.scm");
        pub const INJECTIONS: &str = q!("../queries/zig/injections.scm");
    }
}

/// Per-grammar resources: the parser language plus its bundled highlight
/// queries. Missing injection/locals queries are `""` — `HighlightConfiguration`
/// accepts that.
struct Grammar {
    language:   Language,
    highlights: &'static str,
    injections: &'static str,
    locals:     &'static str,
}

/// Look up the static grammar for a language-pack name. Queries come from the
/// vendored helix runtime under `queries/`; asm is the lone exception because
/// helix's `nasm` queries don't match the `tree-sitter-asm` grammar's node
/// types, so we keep the crate-bundled asm query there.
fn grammar_for(name: &str) -> Option<Grammar> {
    Some(match name {
        "asm" | "nasm" => Grammar {
            language:   tree_sitter_asm::LANGUAGE.into(),
            highlights: tree_sitter_asm::HIGHLIGHTS_QUERY,
            injections: "",
            locals:     "",
        },
        "bash" => Grammar {
            language:   tree_sitter_bash::LANGUAGE.into(),
            highlights: queries::bash::HIGHLIGHTS,
            injections: queries::bash::INJECTIONS,
            locals:     "",
        },
        "c" => Grammar {
            language:   tree_sitter_c::LANGUAGE.into(),
            highlights: queries::c::HIGHLIGHTS,
            injections: queries::c::INJECTIONS,
            locals:     queries::c::LOCALS,
        },
        "csharp" => Grammar {
            language:   tree_sitter_c_sharp::LANGUAGE.into(),
            highlights: queries::csharp::HIGHLIGHTS,
            injections: queries::csharp::INJECTIONS,
            locals:     "",
        },
        "cpp" => Grammar {
            language:   tree_sitter_cpp::LANGUAGE.into(),
            highlights: queries::cpp::HIGHLIGHTS,
            injections: queries::cpp::INJECTIONS,
            locals:     "",
        },
        "css" => Grammar {
            language:   tree_sitter_css::LANGUAGE.into(),
            highlights: queries::css::HIGHLIGHTS,
            injections: queries::css::INJECTIONS,
            locals:     "",
        },
        "dart" => Grammar {
            language:   tree_sitter_dart::LANGUAGE.into(),
            highlights: queries::dart::HIGHLIGHTS,
            injections: queries::dart::INJECTIONS,
            locals:     queries::dart::LOCALS,
        },
        "dockerfile" => Grammar {
            language:   tree_sitter_dockerfile::language(),
            highlights: queries::dockerfile::HIGHLIGHTS,
            injections: queries::dockerfile::INJECTIONS,
            locals:     "",
        },
        "go" => Grammar {
            language:   tree_sitter_go::LANGUAGE.into(),
            highlights: queries::go::HIGHLIGHTS,
            injections: queries::go::INJECTIONS,
            locals:     queries::go::LOCALS,
        },
        "haskell" => Grammar {
            language:   tree_sitter_haskell::LANGUAGE.into(),
            highlights: queries::haskell::HIGHLIGHTS,
            injections: queries::haskell::INJECTIONS,
            locals:     queries::haskell::LOCALS,
        },
        "html" => Grammar {
            language:   tree_sitter_html::LANGUAGE.into(),
            highlights: queries::html::HIGHLIGHTS,
            injections: queries::html::INJECTIONS,
            locals:     "",
        },
        "java" => Grammar {
            language:   tree_sitter_java::LANGUAGE.into(),
            highlights: queries::java::HIGHLIGHTS,
            injections: queries::java::INJECTIONS,
            locals:     "",
        },
        "javascript" => Grammar {
            language:   tree_sitter_javascript::LANGUAGE.into(),
            highlights: queries::javascript::HIGHLIGHTS,
            injections: queries::javascript::INJECTIONS,
            locals:     queries::javascript::LOCALS,
        },
        "json" => Grammar {
            language:   tree_sitter_json::LANGUAGE.into(),
            highlights: queries::json::HIGHLIGHTS,
            injections: queries::json::INJECTIONS,
            locals:     "",
        },
        "lua" => Grammar {
            language:   tree_sitter_lua::LANGUAGE.into(),
            highlights: queries::lua::HIGHLIGHTS,
            injections: queries::lua::INJECTIONS,
            locals:     "",
        },
        "make" => Grammar {
            language:   tree_sitter_make::LANGUAGE.into(),
            highlights: queries::make::HIGHLIGHTS,
            injections: queries::make::INJECTIONS,
            locals:     "",
        },
        "markdown" => Grammar {
            language:   tree_sitter_md::LANGUAGE.into(),
            highlights: queries::markdown::HIGHLIGHTS,
            injections: queries::markdown::INJECTIONS,
            locals:     "",
        },
        "nix" => Grammar {
            language:   tree_sitter_nix::LANGUAGE.into(),
            highlights: queries::nix::HIGHLIGHTS,
            injections: queries::nix::INJECTIONS,
            locals:     "",
        },
        "php" => Grammar {
            language:   tree_sitter_php::LANGUAGE_PHP.into(),
            highlights: queries::php::HIGHLIGHTS,
            injections: queries::php::INJECTIONS,
            locals:     "",
        },
        "python" => Grammar {
            language:   tree_sitter_python::LANGUAGE.into(),
            highlights: queries::python::HIGHLIGHTS,
            injections: queries::python::INJECTIONS,
            locals:     queries::python::LOCALS,
        },
        "regex" => Grammar {
            language:   tree_sitter_regex::LANGUAGE.into(),
            highlights: queries::regex::HIGHLIGHTS,
            injections: "",
            locals:     "",
        },
        "ruby" => Grammar {
            language:   tree_sitter_ruby::LANGUAGE.into(),
            highlights: queries::ruby::HIGHLIGHTS,
            injections: queries::ruby::INJECTIONS,
            locals:     queries::ruby::LOCALS,
        },
        "rust" => Grammar {
            language:   tree_sitter_rust::LANGUAGE.into(),
            highlights: queries::rust::HIGHLIGHTS,
            injections: queries::rust::INJECTIONS,
            locals:     queries::rust::LOCALS,
        },
        "sql" => Grammar {
            language:   tree_sitter_sequel::LANGUAGE.into(),
            highlights: queries::sql::HIGHLIGHTS,
            injections: "",
            locals:     "",
        },
        "swift" => Grammar {
            language:   tree_sitter_swift::LANGUAGE.into(),
            highlights: queries::swift::HIGHLIGHTS,
            injections: queries::swift::INJECTIONS,
            locals:     queries::swift::LOCALS,
        },
        "toml" => Grammar {
            language:   tree_sitter_toml_ng::LANGUAGE.into(),
            highlights: queries::toml::HIGHLIGHTS,
            injections: queries::toml::INJECTIONS,
            locals:     "",
        },
        "typescript" => Grammar {
            language:   tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            highlights: queries::typescript::HIGHLIGHTS,
            injections: queries::typescript::INJECTIONS,
            locals:     queries::typescript::LOCALS,
        },
        "tsx" => Grammar {
            language:   tree_sitter_typescript::LANGUAGE_TSX.into(),
            highlights: queries::tsx::HIGHLIGHTS,
            injections: queries::tsx::INJECTIONS,
            locals:     queries::tsx::LOCALS,
        },
        "yaml" => Grammar {
            language:   tree_sitter_yaml::LANGUAGE.into(),
            highlights: queries::yaml::HIGHLIGHTS,
            injections: queries::yaml::INJECTIONS,
            locals:     "",
        },
        "zig" => Grammar {
            language:   tree_sitter_zig::LANGUAGE.into(),
            highlights: queries::zig::HIGHLIGHTS,
            injections: queries::zig::INJECTIONS,
            locals:     "",
        },
        _ => return None,
    })
}

/// Per-line syntax highlight spans for one editor buffer.
pub struct SyntaxHighlighter {
    config:          HighlightConfiguration,
    highlighter:     Highlighter,
    /// `line_highlights[i]` = sorted, non-overlapping (char_start, char_end, Color) spans.
    line_highlights: Vec<Vec<(usize, usize, Color)>>,
}

impl SyntaxHighlighter {
    pub fn new(path: &Path) -> Option<Self> {
        let lang_name = detect_language(path)?;
        let g = grammar_for(lang_name)?;

        let mut config = HighlightConfiguration::new(
            g.language, lang_name, g.highlights, g.injections, g.locals,
        ).ok()?;
        config.configure(HIGHLIGHT_NAMES);

        Some(Self {
            config,
            highlighter: Highlighter::new(),
            line_highlights: Vec::new(),
        })
    }

    /// Re-parse the full buffer and rebuild per-line highlight caches.
    pub fn rehighlight(&mut self, lines: &[String]) {
        self.line_highlights = vec![Vec::new(); lines.len()];

        let mut source = String::new();
        let mut line_byte_starts: Vec<usize> = Vec::with_capacity(lines.len());
        for line in lines {
            line_byte_starts.push(source.len());
            source.push_str(line);
            source.push('\n');
        }

        let events = match self.highlighter.highlight(
            &self.config,
            source.as_bytes(),
            None,
            |_| None,
        ) {
            Ok(e)  => e,
            Err(_) => return,
        };

        let mut color_stack: Vec<Color> = Vec::new();

        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(h)) => {
                    color_stack.push(color_for(h));
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    let Some(&color) = color_stack.last() else { continue };
                    let start_line =
                        line_byte_starts.partition_point(|&b| b <= start).saturating_sub(1);
                    let end_line =
                        line_byte_starts.partition_point(|&b| b < end).saturating_sub(1);

                    for line_idx in start_line..=end_line {
                        let Some(&lb) = line_byte_starts.get(line_idx) else { break };
                        let line_str = &lines[line_idx];

                        let byte_lo = if line_idx == start_line { start - lb } else { 0 };
                        let byte_hi = if line_idx == end_line {
                            (end - lb).min(line_str.len())
                        } else {
                            line_str.len()
                        };
                        if byte_lo >= byte_hi { continue; }

                        let char_lo = line_str[..byte_lo].chars().count();
                        let char_hi = line_str[..byte_hi].chars().count();
                        if char_lo < char_hi {
                            self.line_highlights[line_idx].push((char_lo, char_hi, color));
                        }
                    }
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    color_stack.pop();
                }
                Err(_) => break,
            }
        }
    }

    /// Returns the highlight spans for `line_idx`, sorted left-to-right.
    pub fn get_line(&self, line_idx: usize) -> &[(usize, usize, Color)] {
        self.line_highlights
            .get(line_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}
