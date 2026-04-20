# ACodeEditor

A TUI terminal code editor and dev environment, powered by Rust.

## Install

### macOS / Linux

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/stubbornmarlin3/ACodeEditor/releases/latest/download/acodeeditor-installer.sh | sh
```

### Windows (PowerShell)

```powershell
powershell -c "irm https://github.com/stubbornmarlin3/ACodeEditor/releases/latest/download/acodeeditor-installer.ps1 | iex"
```

After installing, run `ace` in any terminal to launch.

## Usage

```sh
ace [path]    # open a file or directory
ace           # open current directory
```

## Build from source

Requires Rust 1.85+:

```sh
cargo build --release
# binary at target/release/ace
```

## License

MIT
