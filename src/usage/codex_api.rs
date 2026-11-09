//! Consulta própria do Codex. Desligada por padrão.
//!
//! 1. `GET https://chatgpt.com/backend-api/wham/usage` com o token do `~/.codex/auth.json`
//!    (`tokens.access_token` e `ChatGPT-Account-Id: tokens.account_id`). **Não é
//!    oficial.** O token é só lido: nunca renovado, nunca gravado, nunca mostrado.
//!    Vencido (pelo `exp` do JWT), não é enviado: quem renova é o próprio Codex.
//! 2. Se a leitura direta falhar (fora de um recuo por 429): o `codex app-server` do
//!    próprio app, por JSON-RPC na entrada e saída padrão (`initialize` →
//!    `account/rateLimits/read`), que usa o login do Codex do jeito dele. Sem janela,
//!    no máximo 20 s; se falhar, não tenta de novo por 30 min.
//!
//! Formatos e regras como os do Codenotch (github.com/vinzdg/codenotch, MIT).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Limits, Source, Window};
use crate::http::{self, Client};

const URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| super::home().map(|h| h.join(".codex")))
}

/// Login do Codex. O `Debug` não mostra o token.
pub struct Credential {
    token: String,
    account: String,
    /// `exp` do JWT, segundos Unix.
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").field("token", &"<oculto>").field("expires_at", &self.expires_at).finish()
    }
}

pub fn read_credential() -> Option<Credential> {
    parse_credential(&super::read_json(&codex_home()?.join("auth.json"))?)
}

pub fn parse_credential(v: &Value) -> Option<Credential> {
    let tokens = v.get("tokens")?;
    let token = tokens.get("access_token")?.as_str()?.trim().to_owned();
    let account = tokens.get("account_id")?.as_str()?.trim().to_owned();
    if token.is_empty() || account.is_empty() {
        return None;
    }
    let expires_at = jwt_claims(&token).and_then(|c| c.get("exp").and_then(Value::as_f64)).map(|e| e as i64);
    let plan = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .and_then(|c| c.get("https://api.openai.com/auth")?.get("chatgpt_plan_type")?.as_str().map(str::to_owned));
    Some(Credential { token, account, expires_at, plan })
}

/// Segundo trecho do JWT (base64url) como JSON. Só para a validade e o plano; quem
/// confere a assinatura é o servidor.
fn jwt_claims(token: &str) -> Option<Value> {
    serde_json::from_slice(&base64url(token.split('.').nth(1)?)?).ok()
}

fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[derive(Debug, PartialEq)]
pub enum Error {
    NoCredential,
    Expired,
    Unauthorized,
    RateLimited(Option<i64>),
    Http(u32),
    Network(String),
    Parse,
}

impl Error {
    pub fn describe(&self) -> String {
        match self {
            Error::NoCredential => "sem login do Codex nesta máquina".to_owned(),
            Error::Expired => "o login do Codex venceu; abra o Codex para renovar".to_owned(),
            Error::Unauthorized => "o servidor recusou o login do Codex".to_owned(),
            Error::RateLimited(_) => "o servidor pediu para esperar (429)".to_owned(),
            Error::Http(code) => format!("o servidor respondeu {code}"),
            Error::Network(e) => e.clone(),
            Error::Parse => "resposta em formato desconhecido".to_owned(),
        }
    }
}

pub fn fetch(now: i64) -> Result<Limits, Error> {
    let cred = read_credential().ok_or(Error::NoCredential)?;
    if cred.expires_at.is_some_and(|e| e <= now) {
        return Err(Error::Expired);
    }
    let client = Client::new([10_000, 10_000, 15_000, 15_000]).map_err(Error::Network)?;
    let bearer = format!("Bearer {}", cred.token);
    let response = client
        .get_with_headers(
            URL,
            true,
            &[
                ("Authorization", &bearer),
                ("ChatGPT-Account-Id", &cred.account),
                ("Accept", "application/json"),
                ("Cache-Control", "no-cache"),
            ],
        )
        .map_err(Error::Network)?;
    drop(bearer);
    let v: Value = match response.status {
        200 => serde_json::from_slice(&response.body(256 * 1024).map_err(Error::Network)?).map_err(|_| Error::Parse)?,
        401 | 403 => return Err(Error::Unauthorized),
        429 => {
            let after = response.header(http::QUERY_RETRY_AFTER).and_then(|h| super::gate::parse_retry_after(&h, now));
            return Err(Error::RateLimited(after));
        }
        code => return Err(Error::Http(code)),
    };
    let windows = parse_usage(&v, now);
    if windows.is_empty() {
        return Err(Error::Parse);
    }
    let plan = v.get("plan_type").and_then(Value::as_str).map(str::to_owned).or(cred.plan);
    let mut l = Limits { windows, source: Source::Api, fetched_at: now, plan };
    l.sort();
    Ok(l)
}

/// `rate_limit.primary_window` / `secondary_window` com `used_percent`,
/// `limit_window_seconds` e `reset_at` (segundos) ou `reset_after_seconds`. Os extras
/// (Spark, revisão de código) ficam de fora: não são o limite do plano.
pub fn parse_usage(v: &Value, now: i64) -> Vec<Window> {
    let mut out = Vec::new();
    for (fallback, key) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        let Some(w) = v.pointer(&format!("/rate_limit/{key}")).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64) else { continue };
        let duration = w.get("limit_window_seconds").and_then(Value::as_f64).filter(|d| *d > 0.0).map(|d| d as i64);
        let resets_at = w.get("reset_at").and_then(super::parse_time).or_else(|| {
            w.get("reset_after_seconds").and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e9).map(|s| now + s as i64)
        });
        let id = super::codex_window_id(duration, fallback);
        if out.iter().all(|x: &Window| x.id != id) {
            out.push(Window::new(&id, used as f32, resets_at, duration));
        }
    }
    out
}

/// O `codex.exe` nativo: o do app (`%LOCALAPPDATA%\OpenAI\Codex\bin\<versão>`), o do npm,
/// o de `~/.codex/bin` ou um no PATH. Nada de `.cmd` nem de Node no meio.
pub fn find_codex() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        let mut bins: Vec<PathBuf> = std::fs::read_dir(local.join(r"OpenAI\Codex\bin"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("codex.exe"))
            .filter(|p| p.is_file())
            .collect();
        bins.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));
        candidates.extend(bins);
    }
    if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
        let vendor = appdata.join(r"npm\node_modules\@openai\codex\vendor");
        for e in std::fs::read_dir(vendor).into_iter().flatten().flatten() {
            candidates.push(e.path().join(r"codex\codex.exe"));
        }
    }
    if let Some(home) = codex_home() {
        candidates.push(home.join(r"bin\codex.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join("codex.exe")));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Lê os limites pelo `codex app-server`. Só `initialize` e `account/rateLimits/read`:
/// nenhum comando de login ou de modelo.
pub fn read_app_server(now: i64) -> Option<Limits> {
    let exe = find_codex()?;
    let mut command = Command::new(exe);
    command.arg("app-server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().ok()?;
    let result = (|| {
        let mut input = child.stdin.take()?;
        let output = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(32);
        std::thread::Builder::new()
            .name("codex-app-server".into())
            .spawn(move || {
                for line in BufReader::new(output).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        let init = serde_json::json!({"id": 1, "method": "initialize",
            "params": {"clientInfo": {"name": "process-monitor", "version": env!("CARGO_PKG_VERSION")}}});
        writeln!(input, "{init}").ok()?;
        input.flush().ok()?;
        let deadline = Instant::now() + APP_SERVER_TIMEOUT;
        loop {
            let line = rx.recv_timeout(deadline.checked_duration_since(Instant::now())?).ok()?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match v.get("id").and_then(Value::as_u64) {
                Some(1) => {
                    v.get("result")?;
                    writeln!(input, "{}", serde_json::json!({"method": "initialized", "params": {}})).ok()?;
                    writeln!(input, "{}", serde_json::json!({"id": 2, "method": "account/rateLimits/read"})).ok()?;
                    input.flush().ok()?;
                }
                Some(2) => return parse_app_server(v.get("result")?, now),
                _ => {}
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// `rateLimitsByLimitId.codex`, ou `rateLimits` quando o mapa não vem. Com o mapa
/// presente, um balde que não é o `codex` (o Spark) nunca entra no lugar dele.
pub fn parse_app_server(result: &Value, now: i64) -> Option<Limits> {
    let core = match result.get("rateLimitsByLimitId").filter(|v| !v.is_null()) {
        Some(buckets) => buckets.get("codex")?,
        None => result.get("rateLimits")?,
    };
    if core.get("limitId").and_then(Value::as_str).is_some_and(|id| id != "codex") {
        return None;
    }
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(w) = core.get(key).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("usedPercent").and_then(Value::as_f64) else { continue };
        let duration = w.get("windowDurationMins").and_then(Value::as_f64).filter(|m| *m > 0.0).map(|m| (m * 60.0) as i64);
        let resets_at = w.get("resetsAt").and_then(super::parse_time);
        let id = super::codex_window_id(duration, key);
        windows.push(Window::new(&id, used as f32, resets_at, duration));
    }
    if windows.is_empty() {
        return None;
    }
    let plan = result.get("planType").or_else(|| core.get("planType")).and_then(Value::as_str).map(str::to_owned);
    let mut l = Limits { windows, source: Source::AppServer, fetched_at: now, plan };
    l.sort();
    Some(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOUR, SEVEN_DAY};

    fn b64(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        s
    }

    #[test]
    fn login_do_codex_sem_mostrar_o_token() {
        let access = format!("h.{}.s", b64(br#"{"exp":1800000000}"#));
        let id = format!("h.{}.s", b64(br#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}"#));
        let v = serde_json::json!({"tokens": {"access_token": access, "account_id": "acc", "id_token": id, "refresh_token": "SEGREDO"}});
        let c = parse_credential(&v).unwrap();
        assert_eq!(c.expires_at, Some(1_800_000_000));
        assert_eq!(c.plan.as_deref(), Some("plus"));
        assert!(!format!("{c:?}").contains("SEGREDO") && !format!("{c:?}").contains(&access));
        assert!(parse_credential(&serde_json::json!({"tokens": {"access_token": "x", "account_id": ""}})).is_none());
    }

    #[test]
    fn resposta_do_wham() {
        let v = serde_json::json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 25, "limit_window_seconds": 18000, "reset_at": 1_800_001_000},
            "secondary_window": {"used_percent": 10, "limit_window_seconds": 604800, "reset_after_seconds": 600}
        }, "additional_rate_limits": [{"limit_name": "Spark", "rate_limit": {"primary_window": {"used_percent": 99}}}]});
        let ws = parse_usage(&v, 1_000);
        assert_eq!(ws.len(), 2);
        assert_eq!((ws[0].id.as_str(), ws[0].resets_at), (FIVE_HOUR, Some(1_800_001_000)));
        assert_eq!((ws[1].id.as_str(), ws[1].resets_at), (SEVEN_DAY, Some(1_600)));
        // Plano grátis: primário de 30 dias, sem secundário.
        let free = serde_json::json!({"rate_limit": {"primary_window": {"used_percent": 16, "limit_window_seconds": 2_592_000}, "secondary_window": null}});
        assert_eq!(parse_usage(&free, 0)[0].id, "monthly");
        assert!(parse_usage(&serde_json::json!({"rate_limit": {"primary_window": {"used_percent": null}}}), 0).is_empty());
    }

    #[test]
    fn resposta_do_app_server() {
        let v = serde_json::json!({
            "rateLimits": {"limitId": "codex_bengalfox", "primary": {"usedPercent": 0}},
            "rateLimitsByLimitId": {"codex": {"limitId": "codex",
                "primary": {"usedPercent": 32, "windowDurationMins": 300, "resetsAt": 1_789_878_630},
                "secondary": {"usedPercent": 5, "windowDurationMins": 10080}}}
        });
        let l = parse_app_server(&v, 7).unwrap();
        assert_eq!(l.source, Source::AppServer);
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert_eq!(l.windows[0].resets_at, Some(1_789_878_630));
        assert_eq!(l.windows[1].id, SEVEN_DAY);
        // Só o Spark: não é o limite do plano.
        assert!(parse_app_server(&serde_json::json!({"rateLimits": {"limitId": "codex_bengalfox", "primary": {"usedPercent": 1}}}), 0).is_none());
        assert!(parse_app_server(&serde_json::json!({"rateLimitsByLimitId": {}, "rateLimits": {"primary": {"usedPercent": 7}}}), 0).is_none());
        // Formato antigo, sem o mapa.
        let old = serde_json::json!({"rateLimits": {"primary": {"usedPercent": 25, "windowDurationMins": 300}}});
        assert_eq!(parse_app_server(&old, 0).unwrap().windows[0].used, 25.0);
    }
}
