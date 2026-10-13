# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-file, zero-dependency (std only) read-only HTTP file server for handing files to VMs / machines on the local network. It serves **only** the `compartilhar/` folder, resolved relative to the **current working directory** (created on startup if missing). Code, comments, CLI flags and UI strings are in Portuguese — keep it that way.

## Commands

```sh
cargo run --release                          # listens on 0.0.0.0:8000
cargo run --release -- --porta 9000          # change port
cargo run --release -- --host 192.168.56.1   # bind a single interface
cargo build --release
cargo clippy
cargo test                                   # unit tests live in `mod tests` at the bottom of src/main.rs
cargo test faixas                            # run a single test (substring match on name)
cargo test -- --nocapture                    # see eprintln! output (e.g. skipped symlink test)
```

Run from the repo root so `compartilhar/` lands there. Binary: `disponibilizador`. Release profile uses LTO + strip.

Tests build real temp directory trees under `std::env::temp_dir()`. The symlink-escape test silently skips on Windows unless running as admin / Developer Mode. `src/LEIA-ME.txt` is a readme meant to sit inside `compartilhar/`.

## Architecture (`src/main.rs`)

- **Concurrency**: blocking `TcpListener`, one `thread::spawn` per connection, capped at `MAX_CONEXOES` (128) via the `ATIVAS` atomic counter; the `Guarda` drop guard decrements it. Excess connections get an immediate 503 from the accept loop (without reading the request, 1s write timeout).
- **HTTP**: hand-rolled, one request per connection (`Connection: close`). `ler_requisicao` reads only method, target and the `Range` header, with a 16 KB header cap, 100-line limit, and a total deadline (`PRAZO_CABECALHO`, enforced by the `ComPrazo` reader wrapper that resets the socket read timeout to the remaining time before every read — a per-read timeout alone would allow slowloris). Only GET/HEAD; anything else → 405.
- **Path security is the core invariant** — `resolver` must keep rejecting: `..`, any segment starting with `.` (hidden files), `:` (Windows drive/ADS), `\`, NUL, and invalid percent-encoding; then it `canonicalize`s and requires the real path to `starts_with` the canonical root (component-wise, so symlinks pointing outside are blocked). `listar` applies the same filters so hidden/escaping entries never appear in listings. Any change touching paths must preserve these checks.
- **Directory listing**: `listar` redirects (301) to the canonical trailing-slash URL, then renders an inline HTML page (HTML-escaped names, percent-encoded links, dirs first then files, case-insensitive sort).
- **Downloads**: `enviar_arquivo` streams in 256 KB chunks, always as `application/octet-stream` + `Content-Disposition: attachment` (RFC 5987 `filename*`). Supports single `Range` (`bytes=a-b`, `a-`, `-n`) for resume → 206/416; multi-range falls back to the full file.
- `ip_local` discovers the LAN IP via a non-sending UDP `connect` purely for the startup banner.
