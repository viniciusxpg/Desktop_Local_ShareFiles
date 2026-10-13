//! Disponibilizador de download local.
//!
//! Serve SOMENTE o conteúdo da pasta `compartilhar`, que fica dentro do
//! diretório de onde o programa é executado (a raiz do projeto).
//! Nada fora dela é acessível: `../`, links simbólicos, caminhos absolutos
//! e arquivos ocultos são bloqueados. Apenas leitura (GET/HEAD).
//!
//! O acesso exige um PIN de 4 dígitos, mostrado no console. Depois de
//! PIN_MAX_ERROS erros o PIN é trocado por outro e as tentativas ficam
//! bloqueadas por PIN_BLOQUEIO.
//!
//! Uso:
//!   cargo run --release
//!   cargo run --release -- --porta 9000
//!   cargo run --release -- --host 192.168.56.1   (escutar só em uma interface)
//!   cargo run --release -- --sem-pin             (acesso livre, sem PIN)

use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PASTA: &str = "compartilhar";
const CHUNK: usize = 256 * 1024;
const MAX_CONEXOES: usize = 128;
/// Tempo total para o cliente enviar o cabeçalho inteiro (contra slowloris).
const PRAZO_CABECALHO: Duration = Duration::from_secs(10);
const COOKIE: &str = "sessao_disponibilizador";
/// Começa com '.', então `resolver` nunca a confunde com um arquivo da pasta.
const ROTA_PIN: &str = "/.entrar";
/// Erros tolerados; o erro seguinte troca o PIN.
const PIN_MAX_ERROS: u32 = 4;
/// Espera a cada PIN errado. Como acontece com a trava do PIN segura,
/// limita o chute a 1 por segundo no servidor inteiro.
const PIN_PAUSA_ERRO: Duration = Duration::from_secs(1);
/// Depois de trocar o PIN, ninguém tenta de novo por este tempo.
const PIN_BLOQUEIO: Duration = Duration::from_secs(30);
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

/// Decompõe um instante em UTC: (ano, mês, dia, hora, min, seg, dia da semana 0=dom).
/// Algoritmo "civil_from_days" de Howard Hinnant.
fn data_utc(t: SystemTime) -> (i64, u32, u32, u32, u32, u32, usize) {
    let seg = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (dias, resto) = (seg.div_euclid(86400), seg.rem_euclid(86400));
    let semana = (dias + 4).rem_euclid(7) as usize; // 1970-01-01 foi quinta
    let z = dias + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dia = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mes = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let ano = yoe + era * 400 + i64::from(mes <= 2);
    let r = resto as u32;
    (ano, mes, dia, r / 3600, r / 60 % 60, r % 60, semana)
}

/// Formato de data do HTTP: "Thu, 29 Feb 2024 00:00:00 GMT".
fn data_http(t: SystemTime) -> String {
    const DIAS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MESES: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (a, m, d, h, mi, s, sem) = data_utc(t);
    format!("{}, {:02} {} {} {:02}:{:02}:{:02} GMT", DIAS[sem], d, MESES[m as usize - 1], a, h, mi, s)
}

/// Data para o log (UTC, a biblioteca padrão não conhece o fuso local).
fn data_log(t: SystemTime) -> String {
    let (a, m, d, h, mi, s, _) = data_utc(t);
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02}Z", a, m, d, h, mi, s)
}

/// Comparação em tempo constante (não revela quantos caracteres batem).
fn iguais(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Número aleatório. Cada `RandomState` nasce com chaves SipHash sorteadas
/// pelo sistema operacional, então o hash de nada já é imprevisível.
fn aleatorio() -> u64 {
    RandomState::new().build_hasher().finish()
}

/// 32 caracteres hex imprevisíveis, para identificar sessões.
fn gerar_id() -> String {
    format!("{:016x}{:016x}", aleatorio(), aleatorio())
}

/// PIN de 4 dígitos, sempre diferente do anterior.
fn gerar_pin(anterior: Option<&str>) -> String {
    loop {
        let p = format!("{:04}", aleatorio() % 10_000);
        if Some(p.as_str()) != anterior {
            return p;
        }
    }
}

/// Valor de um campo de formulário (`pin=1234&volta=%2F`), já decodificado.
fn campo(form: &str, nome: &str) -> Option<String> {
    form.split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == nome).map(|(_, v)| v))
        .and_then(|v| decodificar_url(&v.replace('+', " ")))
}

/// Para onde voltar depois do PIN. Só caminhos locais: nada de `//outro.site`
/// (redirecionamento aberto) nem caracteres de controle (injeção de cabeçalho).
fn destino_seguro(v: &str) -> String {
    let ok = v.starts_with('/')
        && !v.starts_with("//")
        && !v.starts_with("/\\")
        && v != ROTA_PIN
        && v.bytes().all(|b| b > b' ' && b != 0x7f);
    if ok { v.to_string() } else { "/".to_string() }
}

/// Valor de um cookie dentro do cabeçalho `Cookie: a=1; b=2`.
fn valor_cookie<'a>(cabecalho: &'a str, nome: &str) -> Option<&'a str> {
    cabecalho
        .split(';')
        .find_map(|p| p.trim().split_once('=').filter(|(k, _)| *k == nome).map(|(_, v)| v))
}

// ───────────── HTTP básico ─────────────

struct Req {
    metodo: String,
    alvo: String,
    range: Option<String>,
    if_range: Option<String>,
    cookie: Option<String>,
    /// Corpo do POST (formulário do PIN); vazio nos demais métodos.
    corpo: String,
}

/// Lado de saída da conexão. Lembra o status e conta os bytes do corpo, para o log.
struct Resposta {
    s: TcpStream,
    status: u16,
    corpo: u64,
}

impl Write for Resposta {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.s.write(buf)?;
        self.corpo += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// Configuração e estado compartilhados entre as threads.
struct Config {
    raiz: PathBuf,
    /// `None` com `--sem-pin`.
    pin: Option<Mutex<EstadoPin>>,
    /// Sessões de quem já acertou o PIN. Sobrevivem à troca do PIN, então
    /// alguém errando de propósito não derruba quem já entrou.
    sessoes: Mutex<HashSet<String>>,
}

/// Trava ignorando envenenamento: o estado continua válido mesmo se outra
/// thread entrou em pânico segurando a trava.
fn travar<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Leitor que falha quando o prazo total acaba, não importa quão devagar
/// os bytes cheguem: antes de cada leitura o timeout vira o tempo restante.
struct ComPrazo<'a> {
    s: &'a TcpStream,
    limite: Instant,
}

impl Read for ComPrazo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let resta = self.limite.saturating_duration_since(Instant::now());
        if resta.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.s.set_read_timeout(Some(resta))?;
        self.s.read(buf)
    }
}

fn ler_requisicao(stream: &TcpStream) -> Option<Req> {
    // limita o cabeçalho a 16 KB e a PRAZO_CABECALHO no total
    let leitor = ComPrazo { s: stream, limite: Instant::now() + PRAZO_CABECALHO };
    let mut r = BufReader::new(leitor.take(16 * 1024));
    let mut linha = String::new();
    if r.read_line(&mut linha).ok()? == 0 {
        return None;
    }
    let mut p = linha.split_whitespace();
    let metodo = p.next()?.to_string();
    let alvo = p.next()?.to_string();
    let (mut range, mut if_range, mut cookie, mut tam_corpo) = (None, None, None, 0usize);
    for _ in 0..100 {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            // só o formulário do PIN usa corpo; 1 KB sobra
            let mut corpo = String::new();
            if metodo == "POST" {
                if tam_corpo > 1024 {
                    return None;
                }
                let mut b = vec![0u8; tam_corpo];
                r.read_exact(&mut b).ok()?;
                corpo = String::from_utf8(b).ok()?;
            }
            return Some(Req { metodo, alvo, range, if_range, cookie, corpo });
        }
        let Some((k, v)) = h.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "range" => range = Some(v.to_string()),
            "if-range" => if_range = Some(v.to_string()),
            "cookie" => cookie = Some(v.to_string()),
            "content-length" => tam_corpo = v.parse().ok()?,
            _ => {}
        }
    }
    None
}

fn enviar_cabecalho(
    s: &mut Resposta,
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
    s.status = codigo;
    s.s.write_all(h.as_bytes()) // direto no socket: cabeçalho não conta como corpo
}

fn responder_html(s: &mut Resposta, codigo: u16, razao: &str, corpo: &str, head: bool) -> io::Result<()> {
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

fn erro(s: &mut Resposta, codigo: u16, razao: &str, msg: &str, head: bool) -> io::Result<()> {
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

fn listar(s: &mut Resposta, dir: &Path, raiz: &Path, bruto: &str, head: bool) -> io::Result<()> {
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

    let corpo = pagina(
        "Compartilhar",
        &format!(
            "<h2>Arquivos disponíveis</h2><p class=\"fraco\">{}</p><table>{}</table>",
            escapar_html(&url),
            linhas
        ),
    );
    responder_html(s, 200, "OK", &corpo, head)
}

const ESTILO: &str = "\
body{font-family:system-ui,sans-serif;max-width:760px;margin:2rem auto;padding:0 1rem;background:#111;color:#eee}\
a{color:#7cc4ff;text-decoration:none}a:hover{text-decoration:underline}\
table{width:100%;border-collapse:collapse}td{padding:.5rem;border-bottom:1px solid #333}\
td:last-child{text-align:right;color:#aaa;white-space:nowrap}\
.fraco{color:#aaa}.aviso{color:#ffb86b}\
form{display:flex;gap:.5rem;margin-top:1rem}\
input{font:inherit;font-size:1.6rem;letter-spacing:.4em;width:6.5em;padding:.4rem .6rem;\
background:#1b1b1b;color:#eee;border:1px solid #444;border-radius:6px;text-align:center}\
button{font:inherit;padding:.4rem 1.2rem;background:#7cc4ff;color:#111;border:0;border-radius:6px;cursor:pointer}";

/// Página HTML completa com o estilo comum.
fn pagina(titulo: &str, conteudo: &str) -> String {
    format!(
        "<!doctype html><html lang=\"pt-br\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{}</style></head><body>{}</body></html>",
        titulo, ESTILO, conteudo
    )
}

// ───────────── acesso por PIN ─────────────

struct EstadoPin {
    pin: String,
    /// Erros desde a última troca de PIN.
    erros: u32,
    bloqueado_ate: Option<Instant>,
}

#[derive(Debug, PartialEq)]
enum Tentativa {
    Certo,
    /// Quantos erros ainda cabem antes da troca (contando o que troca).
    Errado { restam: u32 },
    /// Errou demais: PIN novo e bloqueio de PIN_BLOQUEIO.
    Trocado,
    /// Ainda dentro do bloqueio; nem confere o PIN.
    Bloqueado(Duration),
}

impl EstadoPin {
    fn novo() -> Self {
        EstadoPin { pin: gerar_pin(None), erros: 0, bloqueado_ate: None }
    }

    fn tentar(&mut self, chute: &str, agora: Instant) -> Tentativa {
        if let Some(ate) = self.bloqueado_ate {
            if agora < ate {
                return Tentativa::Bloqueado(ate - agora);
            }
            self.bloqueado_ate = None;
        }
        if iguais(chute, &self.pin) {
            return Tentativa::Certo;
        }
        self.erros += 1;
        if self.erros > PIN_MAX_ERROS {
            self.pin = gerar_pin(Some(&self.pin));
            self.erros = 0;
            self.bloqueado_ate = Some(agora + PIN_BLOQUEIO);
            Tentativa::Trocado
        } else {
            Tentativa::Errado { restam: PIN_MAX_ERROS + 1 - self.erros }
        }
    }
}

fn sessao_valida(req: &Req, cfg: &Config) -> bool {
    req.cookie
        .as_deref()
        .and_then(|c| valor_cookie(c, COOKIE))
        .is_some_and(|id| travar(&cfg.sessoes).contains(id))
}

/// Formulário do PIN. `volta` é para onde ir depois de acertar
/// (a pasta ou o arquivo que a pessoa tentou abrir).
fn pagina_pin(s: &mut Resposta, msg: &str, volta: &str, head: bool) -> io::Result<()> {
    let aviso = if msg.is_empty() {
        String::new()
    } else {
        format!("<p class=\"aviso\">{}</p>", escapar_html(msg))
    };
    let corpo = pagina(
        "PIN de acesso",
        &format!(
            "<h2>PIN de acesso</h2>\
<p class=\"fraco\">Digite o PIN de 4 dígitos mostrado no console do servidor.</p>{}\
<form method=\"post\" action=\"{}\">\
<input type=\"hidden\" name=\"volta\" value=\"{}\">\
<input name=\"pin\" inputmode=\"numeric\" pattern=\"[0-9]{{4}}\" maxlength=\"4\" \
autocomplete=\"off\" autofocus required placeholder=\"0000\">\
<button>Entrar</button></form>",
            aviso,
            ROTA_PIN,
            escapar_html(volta)
        ),
    );
    // 403, não 200: um curl sem sessão não pode salvar esta página achando que é o arquivo
    responder_html(s, 403, "Forbidden", &corpo, head)
}

/// POST do formulário do PIN.
fn entrar(s: &mut Resposta, req: &Req, cfg: &Config, pin: &Mutex<EstadoPin>) -> io::Result<()> {
    let chute = campo(&req.corpo, "pin").unwrap_or_default();
    let volta = destino_seguro(&campo(&req.corpo, "volta").unwrap_or_default());

    let resultado = {
        let mut est = travar(pin);
        let r = est.tentar(chute.trim(), Instant::now());
        match r {
            Tentativa::Trocado => println!(
                "{} PIN errado {} vezes: trocado. NOVO PIN: {}",
                data_log(SystemTime::now()),
                PIN_MAX_ERROS + 1,
                est.pin
            ),
            // dorme segurando a trava: chutes em paralelo esperam na fila
            Tentativa::Errado { .. } => thread::sleep(PIN_PAUSA_ERRO),
            _ => {}
        }
        r
    };

    match resultado {
        Tentativa::Certo => {
            let id = gerar_id();
            travar(&cfg.sessoes).insert(id.clone());
            let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", COOKIE, id);
            enviar_cabecalho(s, 303, "See Other", &[("Set-Cookie", cookie), ("Location", volta)], 0)
        }
        Tentativa::Errado { restam } => {
            let msg = if restam == 1 {
                "PIN incorreto. Mais 1 erro e o PIN será trocado.".to_string()
            } else {
                format!("PIN incorreto. Mais {} erros e o PIN será trocado.", restam)
            };
            pagina_pin(s, &msg, &volta, false)
        }
        Tentativa::Trocado => pagina_pin(
            s,
            &format!(
                "PIN incorreto {} vezes. Um novo PIN foi gerado: veja no console do servidor \
e tente de novo em {} segundos.",
                PIN_MAX_ERROS + 1,
                PIN_BLOQUEIO.as_secs()
            ),
            &volta,
            false,
        ),
        Tentativa::Bloqueado(falta) => pagina_pin(
            s,
            &format!(
                "Muitas tentativas erradas. Aguarde {} segundos e use o novo PIN do console.",
                falta.as_secs() + 1
            ),
            &volta,
            false,
        ),
    }
}

// ───────────── download (com suporte a retomar) ─────────────

#[derive(Debug, PartialEq)]
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

/// ETag forte derivada de tamanho + data de modificação: muda se o arquivo mudar.
fn etag(tam: u64, modificado: SystemTime) -> String {
    let ns = modificado.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("\"{:x}-{:x}\"", tam, ns)
}

/// `If-Range` diz "só me mande a faixa se o arquivo ainda for este".
/// Sem o cabeçalho a faixa vale; com ele, precisa bater com a ETag (comparação
/// forte, então ETag fraca `W/` nunca bate) ou com o Last-Modified exato.
fn if_range_confere(if_range: Option<&str>, etag: &str, ultima: &str) -> bool {
    let Some(v) = if_range.map(str::trim) else { return true };
    if v.starts_with('"') {
        v == etag
    } else if v.starts_with("W/") {
        false
    } else {
        v == ultima
    }
}

fn enviar_arquivo(s: &mut Resposta, arq: &Path, req: &Req, head: bool) -> io::Result<()> {
    let mut f = match File::open(arq) {
        Ok(f) => f,
        Err(_) => return erro(s, 403, "Forbidden", "Sem permissão.", head),
    };
    let meta = f.metadata()?;
    let tam = meta.len();
    let modificado = meta.modified().unwrap_or(UNIX_EPOCH);
    let etag = etag(tam, modificado);
    let ultima = data_http(modificado);
    // arquivo mudou desde o download parcial → manda inteiro em vez de emendar errado
    let range = req
        .range
        .as_deref()
        .filter(|_| if_range_confere(req.if_range.as_deref(), &etag, &ultima));
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
        ("ETag", etag),
        ("Last-Modified", ultima),
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

fn processar(s: &mut Resposta, req: &Req, cfg: &Config) -> io::Result<()> {
    let head = req.metodo == "HEAD";
    let bruto = req.alvo.split(['?', '#']).next().unwrap_or("");
    if let Some(pin) = &cfg.pin
        && req.metodo == "POST"
        && bruto == ROTA_PIN
    {
        return entrar(s, req, cfg, pin);
    }
    if req.metodo != "GET" && !head {
        return enviar_cabecalho(
            s,
            405,
            "Method Not Allowed",
            &[("Allow", "GET, HEAD".to_string())],
            0,
        );
    }

    // Sem sessão: pede o PIN e depois volta para o que foi pedido.
    if cfg.pin.is_some() && !sessao_valida(req, cfg) {
        return pagina_pin(s, "", &destino_seguro(bruto), head);
    }

    let raiz = cfg.raiz.as_path();
    match resolver(&req.alvo, raiz) {
        Some(p) if p.is_dir() => listar(s, &p, raiz, bruto, head),
        Some(p) if p.is_file() => enviar_arquivo(s, &p, req, head),
        _ => erro(s, 404, "Not Found", "Não encontrado.", head),
    }
}

fn atender(s: TcpStream, cfg: &Config) {
    let inicio = Instant::now();
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let Some(req) = ler_requisicao(&s) else { return };
    let mut r = Resposta { s, status: 0, corpo: 0 };
    let resultado = processar(&mut r, &req, cfg);

    let bruto = req.alvo.split(['?', '#']).next().unwrap_or("");
    let caminho = decodificar_url(bruto).unwrap_or_else(|| bruto.to_string());
    println!(
        "{} {} {} {:?} {} {} {:.1}s{}",
        data_log(SystemTime::now()),
        peer,
        req.metodo,
        caminho,
        r.status,
        tamanho_legivel(r.corpo),
        inicio.elapsed().as_secs_f64(),
        if resultado.is_err() { " (interrompido)" } else { "" }
    );
}

// ───────────── main ─────────────

fn uso() -> ! {
    eprintln!("Uso: disponibilizador [--host IP] [--porta N] [--sem-pin]");
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
    let mut com_pin = true;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or_else(|| uso()),
            "--porta" => porta = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| uso()),
            "--sem-pin" => com_pin = false,
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

    let pin = com_pin.then(EstadoPin::novo);
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
    match &pin {
        Some(p) => println!(
            "\nPIN DE ACESSO: {}   (troca depois de {} erros; o novo aparece aqui)",
            p.pin,
            PIN_MAX_ERROS + 1
        ),
        None if host == "0.0.0.0" => {
            println!("\nAVISO: sem PIN e escutando em todas as interfaces.");
            println!("       Qualquer máquina desta rede pode baixar os arquivos.");
        }
        None => {}
    }
    println!("Ctrl+C para parar.\n");

    let cfg = Arc::new(Config { raiz, pin: pin.map(Mutex::new), sessoes: Mutex::new(HashSet::new()) });
    for conn in listener.incoming() {
        let Ok(s) = conn else { continue };
        if ATIVAS.fetch_add(1, Ordering::SeqCst) >= MAX_CONEXOES {
            ATIVAS.fetch_sub(1, Ordering::SeqCst);
            // resposta curta, sem ler a requisição, para não travar o loop de aceite
            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
            let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let mut r = Resposta { s, status: 0, corpo: 0 };
            let _ = erro(&mut r, 503, "Service Unavailable", "Servidor ocupado, tente novamente.", false);
            println!("{} {} 503 (limite de {} conexões)", data_log(SystemTime::now()), peer, MAX_CONEXOES);
            continue;
        }
        let cfg = Arc::clone(&cfg);
        thread::spawn(move || {
            let _g = Guarda;
            atender(s, &cfg);
        });
    }
}

// ───────────── testes ─────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta uma árvore temporária:
    ///   <tmp>/compartilhar/{a.txt, .oculto, sub/b.txt, minha pasta/c.txt}
    ///   <tmp>/fora.txt
    fn arvore(nome: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("disponibilizador-{}-{}", nome, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let raiz = base.join("compartilhar");
        std::fs::create_dir_all(raiz.join("sub")).unwrap();
        std::fs::create_dir_all(raiz.join("minha pasta")).unwrap();
        std::fs::write(raiz.join("a.txt"), "a").unwrap();
        std::fs::write(raiz.join(".oculto"), "x").unwrap();
        std::fs::write(raiz.join("sub").join("b.txt"), "b").unwrap();
        std::fs::write(raiz.join("minha pasta").join("c.txt"), "c").unwrap();
        std::fs::write(base.join("fora.txt"), "fora").unwrap();
        raiz.canonicalize().unwrap()
    }

    #[test]
    fn resolver_aceita_caminhos_validos() {
        let raiz = arvore("validos");
        assert_eq!(resolver("/", &raiz), Some(raiz.clone()));
        assert_eq!(resolver("/a.txt", &raiz), Some(raiz.join("a.txt")));
        assert_eq!(resolver("/sub/b.txt", &raiz), Some(raiz.join("sub").join("b.txt")));
        assert_eq!(resolver("/sub//./b.txt", &raiz), Some(raiz.join("sub").join("b.txt")));
        assert_eq!(resolver("/minha%20pasta/c.txt", &raiz), Some(raiz.join("minha pasta").join("c.txt")));
        assert_eq!(resolver("/a.txt?x=1#y", &raiz), Some(raiz.join("a.txt")));
    }

    #[test]
    fn resolver_bloqueia_fuga_e_entradas_invalidas() {
        let raiz = arvore("bloqueios");
        for alvo in [
            "a.txt",              // sem barra inicial
            "/../fora.txt",       // subir de pasta
            "/sub/../a.txt",      // ".." mesmo voltando para dentro
            "/%2e%2e/fora.txt",   // ".." codificado
            "/%2E%2E%2Ffora.txt", // "../" inteiro codificado
            "/.oculto",           // arquivo oculto
            "/a.txt%00",          // byte nulo
            "/sub%5Cb.txt",       // barra invertida
            "/C:/Windows",        // letra de unidade
            "/a.txt:stream",      // ADS do NTFS
            "/%zz",               // % inválido
            "/%4",                // % incompleto
            "/%FF",               // UTF-8 inválido
            "/nao_existe",
        ] {
            assert_eq!(resolver(alvo, &raiz), None, "deveria bloquear {alvo:?}");
        }
    }

    #[test]
    fn resolver_bloqueia_link_simbolico_para_fora() {
        let raiz = arvore("symlink");
        let fora = raiz.parent().unwrap().join("fora.txt");
        let link = raiz.join("atalho.txt");
        #[cfg(unix)]
        let criado = std::os::unix::fs::symlink(&fora, &link);
        #[cfg(windows)]
        let criado = std::os::windows::fs::symlink_file(&fora, &link);
        if criado.is_err() {
            // no Windows criar symlink exige admin ou modo desenvolvedor
            eprintln!("symlink não criado; teste ignorado");
            return;
        }
        assert_eq!(resolver("/atalho.txt", &raiz), None);
    }

    #[test]
    fn faixas() {
        use Faixa::*;
        let f = |h: Option<&str>, tam| parse_faixa(h, tam);
        assert_eq!(f(None, 100), Inteiro);
        assert_eq!(f(Some("bytes=0-"), 100), Parcial(0, 99));
        assert_eq!(f(Some("bytes=10-19"), 100), Parcial(10, 19));
        assert_eq!(f(Some(" bytes=10-19 "), 100), Parcial(10, 19));
        assert_eq!(f(Some("bytes=90-500"), 100), Parcial(90, 99));
        assert_eq!(f(Some("bytes=-10"), 100), Parcial(90, 99));
        assert_eq!(f(Some("bytes=-500"), 100), Parcial(0, 99));
        assert_eq!(f(Some("bytes=-0"), 100), Invalida);
        assert_eq!(f(Some("bytes=100-"), 100), Invalida);
        assert_eq!(f(Some("bytes=5-2"), 100), Invalida);
        assert_eq!(f(Some("bytes=0-"), 0), Invalida);
        assert_eq!(f(Some("bytes=-5"), 0), Invalida);
        // formatos não suportados caem para o arquivo inteiro
        assert_eq!(f(Some("bytes=a-b"), 100), Inteiro);
        assert_eq!(f(Some("bytes=0-1,5-6"), 100), Inteiro);
        assert_eq!(f(Some("items=0-1"), 100), Inteiro);
    }

    #[test]
    fn url_ida_e_volta() {
        assert_eq!(decodificar_url("/a%20b/%C3%A9"), Some("/a b/é".into()));
        assert_eq!(decodificar_url("%e9"), None); // byte solto não é UTF-8
        assert_eq!(decodificar_url("a%2"), None);
        assert_eq!(codificar_url("minha pasta/é.txt"), "minha%20pasta%2F%C3%A9.txt");
        let nome = "relatório (final) & cia.pdf";
        assert_eq!(decodificar_url(&codificar_url(nome)).as_deref(), Some(nome));
    }

    #[test]
    fn datas() {
        let t = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(data_http(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(data_http(t(951_782_400)), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(data_http(t(1_709_164_800)), "Thu, 29 Feb 2024 00:00:00 GMT");
        assert_eq!(data_http(t(4_102_444_799)), "Thu, 31 Dec 2099 23:59:59 GMT");
        assert_eq!(data_log(t(1_709_164_800 + 3661)), "2024-02-29 01:01:01Z");
    }

    #[test]
    fn if_range() {
        let (etag, ultima) = ("\"a-1f\"", "Thu, 29 Feb 2024 00:00:00 GMT");
        assert!(if_range_confere(None, etag, ultima));
        assert!(if_range_confere(Some("\"a-1f\""), etag, ultima));
        assert!(if_range_confere(Some(ultima), etag, ultima));
        assert!(!if_range_confere(Some("\"a-20\""), etag, ultima)); // arquivo mudou
        assert!(!if_range_confere(Some("W/\"a-1f\""), etag, ultima)); // ETag fraca
        assert!(!if_range_confere(Some("Fri, 01 Mar 2024 00:00:00 GMT"), etag, ultima));
        // a ETag muda quando muda o tamanho ou a data
        let m = UNIX_EPOCH + Duration::from_secs(1000);
        assert_ne!(super::etag(10, m), super::etag(11, m));
        assert_ne!(super::etag(10, m), super::etag(10, m + Duration::from_micros(1)));
    }

    #[test]
    fn formulario_e_cookie() {
        assert_eq!(campo("pin=1234&volta=%2Fpasta%2F", "pin").as_deref(), Some("1234"));
        assert_eq!(campo("pin=1234&volta=%2Fpasta%2F", "volta").as_deref(), Some("/pasta/"));
        assert_eq!(campo("volta=%2Fminha+pasta%2F", "volta").as_deref(), Some("/minha pasta/"));
        assert_eq!(campo("xpin=1234", "pin"), None);
        assert_eq!(campo("", "pin"), None);
        assert_eq!(valor_cookie("a=1; sessao_disponibilizador=xyz", COOKIE), Some("xyz"));
        assert_eq!(valor_cookie("xsessao_disponibilizador=xyz", COOKIE), None);
        assert!(iguais("1234", "1234"));
        assert!(!iguais("1234", "1235"));
        assert!(!iguais("1234", "12345"));
    }

    #[test]
    fn destino_depois_do_pin() {
        assert_eq!(destino_seguro("/pasta/arq%20x.txt"), "/pasta/arq%20x.txt");
        assert_eq!(destino_seguro("/"), "/");
        for ruim in ["", "pasta/", "//evil.com/", "/\\evil.com", "http://evil.com", "/a\r\nSet-Cookie: x", "/a b", ROTA_PIN] {
            assert_eq!(destino_seguro(ruim), "/", "deveria recusar {ruim:?}");
        }
    }

    #[test]
    fn ids_e_pins_aleatorios() {
        let (a, b) = (gerar_id(), gerar_id());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        for _ in 0..200 {
            let p = gerar_pin(Some("0000"));
            assert_eq!(p.len(), 4);
            assert!(p.bytes().all(|c| c.is_ascii_digit()));
            assert_ne!(p, "0000");
        }
    }

    #[test]
    fn pin_troca_depois_de_errar_demais() {
        let mut e = EstadoPin { pin: "1234".into(), erros: 0, bloqueado_ate: None };
        let t0 = Instant::now();
        assert_eq!(e.tentar("1234", t0), Tentativa::Certo);
        assert_eq!(e.tentar("0000", t0), Tentativa::Errado { restam: 4 });
        assert_eq!(e.tentar("", t0), Tentativa::Errado { restam: 3 });
        assert_eq!(e.tentar("1234", t0), Tentativa::Certo); // acertar não zera os erros
        assert_eq!(e.tentar("0000", t0), Tentativa::Errado { restam: 2 });
        assert_eq!(e.tentar("0000", t0), Tentativa::Errado { restam: 1 });
        assert_eq!(e.tentar("0000", t0), Tentativa::Trocado); // 5º erro
        assert_ne!(e.pin, "1234");
        assert_eq!(e.erros, 0);

        // durante o bloqueio nem o PIN novo entra, e o velho também não
        let novo = e.pin.clone();
        assert!(matches!(e.tentar(&novo, t0 + Duration::from_secs(10)), Tentativa::Bloqueado(_)));
        assert!(matches!(e.tentar("1234", t0 + Duration::from_secs(29)), Tentativa::Bloqueado(_)));
        assert_eq!(e.erros, 0, "tentativas no bloqueio não contam");

        // depois do bloqueio: o velho não vale mais, o novo vale
        let depois = t0 + PIN_BLOQUEIO;
        assert_eq!(e.tentar("1234", depois), Tentativa::Errado { restam: 4 });
        assert_eq!(e.tentar(&novo, depois), Tentativa::Certo);
    }
}
