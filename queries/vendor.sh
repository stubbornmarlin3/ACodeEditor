#!/usr/bin/env bash
# Vendor tree-sitter query files from helix-editor/helix runtime/queries.
# Flattens `; inherits: X[,Y]` directives by recursively inlining parent
# query files at vendor time, so the shipped .scm files are self-contained
# and the Rust code can just `include_str!` them.
#
# Run from the project root: bash queries/vendor.sh

set -euo pipefail

HELIX_REF="${HELIX_REF:-master}"
BASE="https://raw.githubusercontent.com/helix-editor/helix/${HELIX_REF}/runtime/queries"
OUT_DIR="queries"

# Map: our-name -> helix-name. asm is intentionally skipped — tree-sitter-asm
# uses a different grammar than helix's nasm and their node types don't align.
declare -A LANGS=(
    [bash]=bash
    [c]=c
    [csharp]=c-sharp
    [cpp]=cpp
    [css]=css
    [dart]=dart
    [dockerfile]=dockerfile
    [go]=go
    [haskell]=haskell
    [html]=html
    [java]=java
    [javascript]=javascript
    [json]=json
    [lua]=lua
    [make]=make
    [markdown]=markdown
    [markdown_inline]=markdown.inline
    [nix]=nix
    [php]=php
    [python]=python
    [regex]=regex
    [ruby]=ruby
    [rust]=rust
    [sql]=sql
    [swift]=swift
    [toml]=toml
    [tsx]=tsx
    [typescript]=typescript
    [yaml]=yaml
    [zig]=zig
)

# In-memory cache of raw (un-flattened) files fetched so far, keyed by
# "<helix-lang>/<file>" -> contents. Avoids re-fetching parents.
declare -A CACHE=()

fetch_raw() {
    local helix_lang="$1" fname="$2" key="$helix_lang/$fname"
    if [[ -n "${CACHE[$key]+set}" ]]; then
        printf '%s' "${CACHE[$key]}"
        return 0
    fi
    local url="$BASE/$helix_lang/$fname"
    local tmp
    tmp="$(mktemp)"
    local code
    code=$(curl -s -o "$tmp" -w "%{http_code}" "$url")
    if [[ "$code" == "200" ]]; then
        CACHE[$key]="$(cat "$tmp")"
        rm -f "$tmp"
        printf '%s' "${CACHE[$key]}"
        return 0
    fi
    rm -f "$tmp"
    CACHE[$key]=""
    return 1
}

# Flatten ; inherits: parent[,parent2...] by prepending each parent's
# already-flattened content. Duplicates are harmless (HighlightConfiguration
# accepts them); we just want full coverage.
flatten() {
    local helix_lang="$1" fname="$2"
    local raw
    if ! raw="$(fetch_raw "$helix_lang" "$fname")"; then
        return 1
    fi
    # Strip the inherits directive from the body we emit and resolve parents.
    local inherits_line
    inherits_line="$(printf '%s\n' "$raw" | grep -m1 -E '^\s*;\s*inherits:' || true)"
    local body
    body="$(printf '%s\n' "$raw" | sed -E '/^\s*;\s*inherits:/d')"
    if [[ -z "$inherits_line" ]]; then
        printf '%s\n' "$body"
        return 0
    fi
    local parents
    parents="$(printf '%s' "$inherits_line" | sed -E 's/^\s*;\s*inherits:\s*//' | tr ',' ' ')"
    local combined=""
    for parent in $parents; do
        parent="$(printf '%s' "$parent" | tr -d '[:space:]')"
        [[ -z "$parent" ]] && continue
        local parent_body
        if parent_body="$(flatten "$parent" "$fname")"; then
            combined+="; --- inherited from: $parent ---"$'\n'
            combined+="$parent_body"$'\n'
        fi
    done
    combined+="; --- $helix_lang ($fname) ---"$'\n'
    combined+="$body"$'\n'
    printf '%s' "$combined"
}

for our_name in "${!LANGS[@]}"; do
    helix_lang="${LANGS[$our_name]}"
    dest="$OUT_DIR/$our_name"
    mkdir -p "$dest"
    echo "==> $our_name ($helix_lang)"
    for fname in highlights.scm injections.scm locals.scm; do
        out="$dest/$fname"
        if content="$(flatten "$helix_lang" "$fname" 2>/dev/null)" && [[ -n "$content" ]]; then
            header="; Vendored from helix-editor/helix (MPL-2.0) runtime/queries/$helix_lang/$fname at ref $HELIX_REF."$'\n'
            header+="; Inheritance via ';inherits:' has been flattened at vendor time."$'\n'
            printf '%s\n%s\n' "$header" "$content" > "$out"
            echo "    $fname ($(wc -l < "$out") lines)"
        else
            # Not all grammars have all three files — that's normal.
            rm -f "$out" 2>/dev/null || true
        fi
    done
done

echo "Done. Query files written under $OUT_DIR/"
