# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-file, zero-dependency (std only) read-only HTTP file server for handing files to VMs / machines on the local network. It serves **only** the `compartilhar/` folder, resolved relative to the **current working directory** (created on startup if missing). Code, comments, CLI flags and UI strings are in Portuguese — keep it that way.

## Commands

```sh
cargo run --release                          # listens on 0.0.0.0:8000
cargo run --release -- --porta 9000          # change port
cargo run --release -- --host 192.168.56.1   # bind a single interface
cargo run --release -- --sem-pin             # disable the PIN (open access)
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
- **HTTP**: hand-rolled, one request per connection (`Connection: close`). `ler_requisicao` reads only method, target, `Range`, `If-Range`, `Cookie`, and (for POST only, ≤1 KB) the body via `Content-Length`, with a 16 KB header cap, 100-line limit, and a total deadline (`PRAZO_CABECALHO`, enforced by the `ComPrazo` reader wrapper that resets the socket read timeout to the remaining time before every read — a per-read timeout alone would allow slowloris). Only GET/HEAD, plus POST to `ROTA_PIN`; anything else → 405.
- **Response plumbing**: handlers take `&mut Resposta` (wraps the `TcpStream`), never the raw stream. `enviar_cabecalho` records the status and writes headers straight to the socket; body writes go through `Resposta`'s `Write` impl, which counts bytes. `atender` uses both for the one-line-per-request log (UTC timestamp, since std has no local timezone).
- **PIN access** (on by default; `Config.pin` is `None` with `--sem-pin`): a random 4-digit PIN printed on the console. Any GET/HEAD without a valid session cookie gets the PIN form (403, so curl never saves it as the file) carrying a `volta` field = the requested path. The form POSTs to `ROTA_PIN` (`/.entrar` — the leading `.` guarantees `resolver` can never map it to a real file). `EstadoPin::tentar` is pure (takes `agora: Instant`) for testability: after `PIN_MAX_ERROS` errors the next one rotates the PIN and blocks all attempts for `PIN_BLOQUEIO`; errors count since the last rotation (a success doesn't reset them). `entrar` sleeps `PIN_PAUSA_ERRO` **while holding the PIN mutex**, which deliberately serializes guesses server-wide to ~1/s. On success a random 128-bit session id goes into `Config.sessoes` and a cookie; sessions survive PIN rotation and die on restart. The post-login redirect goes through `destino_seguro` (rejects `//host`, control chars → no open redirect / header injection).
- **Path security is the core invariant** — `resolver` must keep rejecting: `..`, any segment starting with `.` (hidden files), `:` (Windows drive/ADS), `\`, NUL, and invalid percent-encoding; then it `canonicalize`s and requires the real path to `starts_with` the canonical root (component-wise, so symlinks pointing outside are blocked). `listar` applies the same filters so hidden/escaping entries never appear in listings. Any change touching paths must preserve these checks.
- **Directory listing**: `listar` redirects (301) to the canonical trailing-slash URL, then renders an inline HTML page (HTML-escaped names, percent-encoded links, dirs first then files, case-insensitive sort).
- **Downloads**: `enviar_arquivo` streams in 256 KB chunks, always as `application/octet-stream` + `Content-Disposition: attachment` (RFC 5987 `filename*`). Supports single `Range` (`bytes=a-b`, `a-`, `-n`) for resume → 206/416; multi-range falls back to the full file. Sends a strong `ETag` (size + mtime in ns) and `Last-Modified`; a `Range` with a non-matching `If-Range` is ignored (full 200) so a resumed download can't splice two file versions. Dates are formatted by the hand-written `data_utc` (no chrono).
- `ip_local` discovers the LAN IP via a non-sending UDP `connect` purely for the startup banner.
