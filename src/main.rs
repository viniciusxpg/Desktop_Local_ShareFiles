//! Disponibilizador de download local.
//!
//! Serve SOMENTE o conteúdo da pasta `compartilhar`, que fica dentro do
//! diretório de onde o programa é executado (a raiz do projeto).
//! Nada fora dela é acessível: `../`, links simbólicos, caminhos absolutos
//! e arquivos ocultos são bloqueados. Apenas leitura (GET/HEAD).
//!
//! Uso:
//!   cargo run --release
//!   cargo run --release -- --porta 9000
//!   cargo run --release -- --host 192.168.56.1   (escutar só em uma interface)

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const PASTA: &str = "compartilhar";
const CHUNK: usize = 256 * 1024;
const MAX_CONEXOES: usize = 128;
static ATIVAS: AtomicUsize = AtomicUsize::new(0);

struct Guarda;
impl Drop for Guarda {
    fn drop(&mut self) {
        ATIVAS.fetch_sub(1, Ordering::SeqCst);
    }
}

// ───────────── utilidades ─────────────

fn escapar_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn codificar_url(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{:02X}", b));
        }
    }
    o
}

fn decodificar_url(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s.get(i + 1..i + 3)?;
            if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            o.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(o).ok()
}

fn tamanho_legivel(n: u64) -> String {
    let un = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < un.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", n)
    } else {
        format!("{:.1} {}", v, un[i])
    }
}

// ───────────── HTTP básico ─────────────

struct Req {
    metodo: String,
    alvo: String,
    range: Option<String>,
}

fn ler_requisicao(stream: &TcpStream) -> Option<Req> {
    // limita o cabeçalho a 16 KB
    let mut r = BufReader::new(stream.take(16 * 1024));
    let mut linha = String::new();
    if r.read_line(&mut linha).ok()? == 0 {
        return None;
    }
    let mut p = linha.split_whitespace();
    let metodo = p.next()?.to_string();
    let alvo = p.next()?.to_string();
    let mut range = None;
    for _ in 0..100 {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            return Some(Req { metodo, alvo, range });
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }
    None
}

fn enviar_cabecalho(
    s: &mut TcpStream,
    codigo: u16,
    razao: &str,
    extras: &[(&str, String)],
    tam: u64,
) -> io::Result<()> {
    let mut h = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         X-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n",
        codigo, razao, tam
    );
    for (k, v) in extras {
        h.push_str(&format!("{}: {}\r\n", k, v));
    }
    h.push_str("\r\n");
    s.write_all(h.as_bytes())
}

fn responder_html(s: &mut TcpStream, codigo: u16, razao: &str, corpo: &str, head: bool) -> io::Result<()> {
    enviar_cabecalho(
        s,
        codigo,
        razao,
        &[("Content-Type", "text/html; charset=utf-8".to_string())],
        corpo.len() as u64,
    )?;
    if !head {
        s.write_all(corpo.as_bytes())?;
    }
    Ok(())
}

fn erro(s: &mut TcpStream, codigo: u16, razao: &str, msg: &str, head: bool) -> io::Result<()> {
    let corpo = format!("<h1>{} {}</h1><p>{}</p>", codigo, razao, escapar_html(msg));
    responder_html(s, codigo, razao, &corpo, head)
}

// ───────────── resolução segura de caminho ─────────────

/// Converte o alvo da requisição em um caminho REAL (sem symlinks) dentro de `raiz`.
fn resolver(alvo: &str, raiz: &Path) -> Option<PathBuf> {
    let caminho = alvo.split(['?', '#']).next()?;
    if !caminho.starts_with('/') {
        return None;
    }
    let dec = decodificar_url(caminho)?;
    if dec.contains('\0') || dec.contains('\\') {
        return None;
    }
    let mut p = raiz.to_path_buf();
    for parte in dec.split('/') {
        if parte.is_empty() || parte == "." {
            continue;
        }
        if parte == ".." || parte.starts_with('.') || parte.contains(':') {
            return None;
        }
        p.push(parte);
    }
    let real = p.canonicalize().ok()?;
    // Path::starts_with compara por componentes, então "compartilhar2" não passa.
    if real.starts_with(raiz) {
        Some(real)
    } else {
        None
    }
}

fn url_da_pasta(dir: &Path, raiz: &Path) -> String {
    let rel = dir.strip_prefix(raiz).unwrap_or_else(|_| Path::new(""));
    let mut u = String::from("/");
    for c in rel.components() {
        u.push_str(&codificar_url(&c.as_os_str().to_string_lossy()));
        u.push('/');
    }
    u
}

// ───────────── listagem ─────────────

fn listar(s: &mut TcpStream, dir: &Path, raiz: &Path, bruto: &str, head: bool) -> io::Result<()> {
    let url = url_da_pasta(dir, raiz);
    if bruto != url {
        return enviar_cabecalho(s, 301, "Moved Permanently", &[("Location", url)], 0);
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return erro(s, 403, "Forbidden", "Sem permissão.", head),
    };

    // (é_arquivo, nome_minúsculo, nome, tamanho)
    let mut itens: Vec<(bool, String, String, u64)> = Vec::new();
    for e in rd.flatten() {
        let Ok(nome) = e.file_name().into_string() else { continue };
        if nome.starts_with('.') {
            continue;
        }
        let Ok(real) = e.path().canonicalize() else { continue };
        if !real.starts_with(raiz) {
            continue; // link simbólico apontando pra fora: some da lista
        }
        let Ok(m) = std::fs::metadata(&real) else { continue };
        itens.push((!m.is_dir(), nome.to_lowercase(), nome, m.len()));
    }
    itens.sort();

    let mut linhas = String::new();
    if dir != raiz {
        linhas.push_str("<tr><td><a href=\"../\">⬆ voltar</a></td><td></td></tr>");
    }
    for (arquivo, _, nome, tam) in &itens {
        if *arquivo {
            linhas.push_str(&format!(
                "<tr><td>📄 <a href=\"{}\">{}</a></td><td>{}</td></tr>",
                codificar_url(nome),
                escapar_html(nome),
                tamanho_legivel(*tam)
            ));
        } else {
            linhas.push_str(&format!(
                "<tr><td>📁 <a href=\"{}/\">{}/</a></td><td></td></tr>",
                codificar_url(nome),
                escapar_html(nome)
            ));
        }
    }
    if itens.is_empty() {
        linhas.push_str("<tr><td colspan=\"2\"><em>Pasta vazia.</em></td></tr>");
    }

    let corpo = format!(
        "<!doctype html><html lang=\"pt-br\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>Compartilhar</title><style>\
body{{font-family:system-ui,sans-serif;max-width:760px;margin:2rem auto;padding:0 1rem;background:#111;color:#eee}}\
a{{color:#7cc4ff;text-decoration:none}}a:hover{{text-decoration:underline}}\
table{{width:100%;border-collapse:collapse}}td{{padding:.5rem;border-bottom:1px solid #333}}\
td:last-child{{text-align:right;color:#aaa;white-space:nowrap}}\
</style></head><body><h2>Arquivos disponíveis</h2>\
<p style=\"color:#aaa\">{}</p><table>{}</table></body></html>",
        escapar_html(&url),
        linhas
    );
    responder_html(s, 200, "OK", &corpo, head)
}

// ───────────── download (com suporte a retomar) ─────────────

enum Faixa {
    Inteiro,
    Parcial(u64, u64),
    Invalida,
}

fn parse_faixa(h: Option<&str>, tam: u64) -> Faixa {
    let Some(h) = h else { return Faixa::Inteiro };
    let Some(v) = h.trim().strip_prefix("bytes=") else { return Faixa::Inteiro };
    if v.contains(',') {
        return Faixa::Inteiro;
    }
    let Some((a, b)) = v.split_once('-') else { return Faixa::Inteiro };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return Faixa::Inteiro };
        if n == 0 || tam == 0 {
            return Faixa::Invalida;
        }
        return Faixa::Parcial(tam.saturating_sub(n), tam - 1);
    }
    let Ok(ini) = a.parse::<u64>() else { return Faixa::Inteiro };
    let ultimo = tam.saturating_sub(1);
    let fim = if b.is_empty() {
        ultimo
    } else {
        match b.parse::<u64>() {
            Ok(f) => f.min(ultimo),
            Err(_) => return Faixa::Inteiro,
        }
    };
    if ini >= tam || ini > fim {
        Faixa::Invalida
    } else {
        Faixa::Parcial(ini, fim)
    }
}

fn enviar_arquivo(s: &mut TcpStream, arq: &Path, range: Option<&str>, head: bool) -> io::Result<()> {
    let mut f = match File::open(arq) {
        Ok(f) => f,
        Err(_) => return erro(s, 403, "Forbidden", "Sem permissão.", head),
    };
    let tam = f.metadata()?.len();
    let nome = arq
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "arquivo".into());

    let mut extras = vec![
        ("Content-Type", "application/octet-stream".to_string()),
        (
            "Content-Disposition",
            format!("attachment; filename*=UTF-8''{}", codificar_url(&nome)),
        ),
        ("Accept-Ranges", "bytes".to_string()),
    ];

    let (ini, fim, codigo, razao): (u64, u64, u16, &str) = match parse_faixa(range, tam) {
        Faixa::Inteiro => (0, tam.saturating_sub(1), 200, "OK"),
        Faixa::Parcial(a, b) => {
            extras.push(("Content-Range", format!("bytes {}-{}/{}", a, b, tam)));
            (a, b, 206, "Partial Content")
        }
        Faixa::Invalida => {
            return enviar_cabecalho(
                s,
                416,
                "Range Not Satisfiable",
                &[("Content-Range", format!("bytes */{}", tam))],
                0,
            )
        }
    };
    let mut resta: u64 = if tam == 0 { 0 } else { fim - ini + 1 };

    enviar_cabecalho(s, codigo, razao, &extras, resta)?;
    if head || resta == 0 {
        return Ok(());
    }
    f.seek(SeekFrom::Start(ini))?;
    let mut buf = vec![0u8; CHUNK];
    while resta > 0 {
        let cap = (CHUNK as u64).min(resta) as usize;
        let n = f.read(&mut buf[..cap])?;
        if n == 0 {
            break;
        }
        s.write_all(&buf[..n])?;
        resta -= n as u64;
    }
    Ok(())
}

// ───────────── conexão ─────────────

fn processar(s: &mut TcpStream, req: &Req, raiz: &Path) -> io::Result<()> {
    let head = req.metodo == "HEAD";
    if req.metodo != "GET" && !head {
        return enviar_cabecalho(
            s,
            405,
            "Method Not Allowed",
            &[("Allow", "GET, HEAD".to_string())],
            0,
        );
    }
    let bruto = req.alvo.split(['?', '#']).next().unwrap_or("");
    match resolver(&req.alvo, raiz) {
        Some(p) if p.is_dir() => listar(s, &p, raiz, bruto, head),
        Some(p) if p.is_file() => enviar_arquivo(s, &p, req.range.as_deref(), head),
        _ => erro(s, 404, "Not Found", "Não encontrado.", head),
    }
}

fn atender(mut s: TcpStream, raiz: &Path) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let Some(req) = ler_requisicao(&s) else { return };
    println!("[{}] {} {:?}", peer, req.metodo, req.alvo);
    let _ = processar(&mut s, &req, raiz);
}

// ───────────── main ─────────────

fn uso() -> ! {
    eprintln!("Uso: disponibilizador [--host IP] [--porta N]");
    std::process::exit(2);
}

fn ip_local() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?; // não envia nada, só descobre a interface
    Some(s.local_addr().ok()?.ip().to_string())
}

fn main() {
    let mut host = "0.0.0.0".to_string();
    let mut porta: u16 = 8000;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or_else(|| uso()),
            "--porta" => porta = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| uso()),
            _ => uso(),
        }
    }

    let pasta = PathBuf::from(PASTA);
    if let Err(e) = std::fs::create_dir_all(&pasta) {
        eprintln!("Não foi possível criar '{}': {}", PASTA, e);
        std::process::exit(1);
    }
    let raiz = pasta.canonicalize().expect("não foi possível resolver a pasta compartilhar");

    let listener = match TcpListener::bind((host.as_str(), porta)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Não foi possível abrir {}:{} → {}", host, porta, e);
            std::process::exit(1);
        }
    };

    println!("Servindo SOMENTE: {}", raiz.display());
    println!("Acesse da VM em:");
    if host == "0.0.0.0" {
        match ip_local() {
            Some(ip) => println!("   http://{}:{}/", ip, porta),
            None => println!("   http://<ip-desta-maquina>:{}/", porta),
        }
    } else {
        println!("   http://{}:{}/", host, porta);
    }
    println!("Ctrl+C para parar.\n");

    let raiz = Arc::new(raiz);
    for conn in listener.incoming() {
        let Ok(s) = conn else { continue };
        if ATIVAS.fetch_add(1, Ordering::SeqCst) >= MAX_CONEXOES {
            ATIVAS.fetch_sub(1, Ordering::SeqCst);
            continue; // descarta (fecha) a conexão
        }
        let raiz = Arc::clone(&raiz);
        thread::spawn(move || {
            let _g = Guarda;
            atender(s, &raiz);
        });
    }
}
