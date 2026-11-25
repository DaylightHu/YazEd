# YazEd
This is a personal "yazi + text editor" project, intended solely for my own use. I'm building it as a way to learn Rust.

# REFERENCE

https://github.com/sxyazi/yazi

## Definition preview

In the editor, press `gd` over a symbol to preview its definition. Use `j` and
`k` to select a result, `Esc` to dismiss, or `Enter` to open the source. Use
`:q` to close the opened definition and return to the calling buffer.

Rust files use `rust-analyzer` from `PATH` by default. Install the matching
`rust-src` component to navigate to the standard library. The default Rust
configuration skips Cargo build scripts and procedural macros. To resolve
macro-generated definitions, override the server's `initialization_options`.
For other languages,
add a server to `lsp.toml` in your YazEd configuration directory (`~/.config/yazi`
by default on Unix, or `$YAZI_CONFIG_HOME`):

```toml
[[server]]
extensions = ["c", "h"]
language_id = "c"
command = ["clangd"]
root_markers = ["compile_commands.json", ".git"]
```

`command` is the executable followed by its arguments. Use an absolute `root`
to scope an entry to one workspace. The server starts on the first lookup and
is reused within that workspace. The benchmark mode is `yazed-perf --definition BUILD`.
