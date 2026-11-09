//! Consulta própria do Claude: `GET https://api.anthropic.com/api/oauth/usage`, a mesma
//! do `/usage` do Claude Code. **Não é oficial**: muda sem aviso e responde 429 se for
//! chamada com frequência. Desligada por padrão.
//!
//! - O token vem do `~/.claude/.credentials.json` do próprio Claude Code, lido só na
//!   hora da requisição. Nunca é renovado (renovar concorreria com o Claude Code e
//!   poderia derrubar a sessão dele), nunca é gravado, nunca vai para log ou mensagem.
//! - Token expirado não é enviado: o servidor responde a ele com 429 e `Retry-After` de
//!   uma hora, não com 401, e o app ficaria uma hora parado à toa.
//! - 401: relê o arquivo uma vez (o Claude Code pode ter acabado de renovar).
//!
//! Formato e regras como os do Codenotch (github.com/vinzdg/codenotch, MIT).

use std::path::PathBuf;

use serde_json::Value;

use super::{Limits, Source, Window};
use crate::http::{self, Client};

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";

/// Credencial do Claude Code. O `Debug` não mostra o token.
pub struct Credential {
    token: String,
    /// Segundos Unix.
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").field("token", &"<oculto>").field("expires_at", &self.expires_at).finish()
    }
}

impl Credential {
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }

    fn same(&self, other: &Credential) -> bool {
        self.token == other.token
    }
}

fn credentials_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| super::home().map(|h| h.join(".claude")))
}

pub fn read_credential() -> Option<Credential> {
    let dir = credentials_dir()?;
    [".credentials.json", "credentials.json"].iter().find_map(|name| parse_credential(&super::read_json(&dir.join(name))?))
}

pub fn parse_credential(v: &Value) -> Option<Credential> {
    let oauth = v.get("claudeAiOauth").unwrap_or(v);
    let token = oauth.get("accessToken")?.as_str()?.trim();
    if token.is_empty() {
        return None;
    }
    Some(Credential {
        token: token.to_owned(),
        expires_at: oauth.get("expiresAt").and_then(super::parse_time),
        plan: oauth.get("subscriptionType").and_then(Value::as_str).map(str::to_owned),
    })
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Sem credencial do Claude Code nesta máquina.
    NoCredential,
    /// Token vencido: o Claude Code renova quando abre no terminal.
    Expired,
    Unauthorized,
    Forbidden,
    RateLimited(Option<i64>),
    Http(u32),
    Network(String),
    Parse,
}

impl Error {
    pub fn describe(&self) -> String {
        match self {
            Error::NoCredential => "sem login do Claude Code nesta máquina".to_owned(),
            Error::Expired => "o login do Claude Code venceu; abra o claude no terminal para renovar".to_owned(),
            Error::Unauthorized => "o servidor recusou o login do Claude Code (trocou de conta?)".to_owned(),
            Error::Forbidden => "acesso negado pelo servidor (403)".to_owned(),
            Error::RateLimited(_) => "o servidor pediu para esperar (429)".to_owned(),
            Error::Http(code) => format!("o servidor respondeu {code}"),
            Error::Network(e) => e.clone(),
            Error::Parse => "resposta em formato desconhecido".to_owned(),
        }
    }
}

/// Uma leitura: credencial, requisição e, num 401, a releitura do arquivo.
pub fn fetch(now: i64) -> Result<Limits, Error> {
    let cred = read_credential().ok_or(Error::NoCredential)?;
    if cred.expired(now) {
        return Err(Error::Expired);
    }
    let plan = cred.plan.clone();
    let result = match request(&cred) {
        Err(Error::Unauthorized) => match read_credential() {
            Some(again) if !again.same(&cred) && !again.expired(now) => request(&again),
            _ => Err(Error::Unauthorized),
        },
        other => other,
    };
    drop(cred);
    let windows = parse_response(&result?);
    if windows.is_empty() {
        return Err(Error::Parse);
    }
    let mut l = Limits { windows, source: Source::Api, fetched_at: now, plan };
    l.sort();
    Ok(l)
}

fn request(cred: &Credential) -> Result<Value, Error> {
    let client = Client::new([10_000, 10_000, 15_000, 15_000]).map_err(Error::Network)?;
    let bearer = format!("Bearer {}", cred.token);
    let response = client
        .get_with_headers(URL, true, &[("Authorization", &bearer), ("anthropic-beta", BETA), ("Accept", "application/json")])
        .map_err(Error::Network)?;
    match response.status {
        200 => {
            let body = response.body(256 * 1024).map_err(Error::Network)?;
            serde_json::from_slice(&body).map_err(|_| Error::Parse)
        }
        401 => Err(Error::Unauthorized),
        403 => Err(Error::Forbidden),
        429 => Err(Error::RateLimited(
            response.header(http::QUERY_RETRY_AFTER).and_then(|h| super::gate::parse_retry_after(&h, super::now_secs())),
        )),
        code => Err(Error::Http(code)),
    }
}

/// `limits[] {kind, percent, resets_at}` é a forma principal; `five_hour`, `seven_day`
/// (e as semanais por modelo) `{utilization, resets_at}` entram quando faltam lá: a
/// janela que acabou de renovar some de `limits` e continua no campo com nome.
pub fn parse_response(v: &Value) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for l in v.get("limits").and_then(Value::as_array).into_iter().flatten() {
        let (Some(kind), Some(pct)) = (l.get("kind").and_then(Value::as_str), l.get("percent").and_then(Value::as_f64)) else {
            continue;
        };
        // Sem hora de renovar não é uma janela de uso (regra do Codenotch).
        let Some(resets_at) = l.get("resets_at").and_then(super::parse_time) else { continue };
        let id = super::claude_window_id(kind);
        if out.iter().all(|w| w.id != id) {
            out.push(Window::new(&id, pct as f32, Some(resets_at), super::claude_window_duration(&id)));
        }
    }
    for field in ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"] {
        let Some(w) = v.get(field).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("utilization").and_then(Value::as_f64) else { continue };
        let id = super::claude_window_id(field);
        if out.iter().any(|x| x.id == id) {
            continue;
        }
        let resets_at = w.get("resets_at").and_then(super::parse_time);
        out.push(Window::new(&id, used as f32, resets_at, super::claude_window_duration(&id)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOUR, SEVEN_DAY};

    #[test]
    fn resposta_nas_duas_formas() {
        let v = serde_json::json!({
            "limits": [
                {"kind": "session", "percent": 62, "resets_at": "2026-10-10T00:00:00Z"},
                {"kind": "weekly_all", "percent": 20, "resets_at": "2026-10-15T00:00:00Z"},
                {"kind": "weekly_scoped", "percent": 5}
            ],
            "five_hour": {"utilization": 62, "resets_at": "2026-10-10T00:00:00Z"},
            "seven_day": {"utilization": 20, "resets_at": "2026-10-15T00:00:00Z"},
            "seven_day_opus": {"utilization": 3, "resets_at": null},
            "seven_day_sonnet": null
        });
        let ws = parse_response(&v);
        assert_eq!(ws.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), [FIVE_HOUR, SEVEN_DAY, "seven_day_opus"]);
        assert_eq!(ws[0].used, 62.0);
        assert_eq!(ws[0].duration_secs, Some(crate::usage::FIVE_HOURS_SECS));
        assert_eq!(ws[2].resets_at, None);
    }

    #[test]
    fn so_os_campos_com_nome() {
        let v = serde_json::json!({
            "five_hour": {"utilization": 10.5, "resets_at": "2026-10-10T00:00:00Z"},
            "seven_day": {"utilization": 40, "resets_at": "2026-10-15T00:00:00Z"}
        });
        let ws = parse_response(&v);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[1].id, SEVEN_DAY);
        assert!(parse_response(&serde_json::json!({"five_hour": null, "limits": []})).is_empty());
    }

    #[test]
    fn credencial_sem_mostrar_o_token() {
        let v = serde_json::json!({"claudeAiOauth": {
            "accessToken": "sk-ant-oat01-SEGREDO", "refreshToken": "r", "expiresAt": 1_800_000_000_000u64,
            "subscriptionType": "pro"
        }});
        let c = parse_credential(&v).unwrap();
        assert_eq!(c.expires_at, Some(1_800_000_000));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert!(!c.expired(1_799_999_999));
        assert!(c.expired(1_800_000_000));
        assert!(!format!("{c:?}").contains("SEGREDO"));
        assert!(parse_credential(&serde_json::json!({"claudeAiOauth": {"accessToken": " "}})).is_none());
        for e in [Error::Expired, Error::Unauthorized, Error::RateLimited(Some(5)), Error::Http(500)] {
            assert!(!e.describe().contains("SEGREDO"));
        }
    }
}
