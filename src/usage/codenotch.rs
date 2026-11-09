//! Leitura dos arquivos que o Codenotch grava em `%APPDATA%\codenotch`: `usage.json`
//! (Claude) e `codex.json`. Não têm segredo: só as janelas já prontas.
//!
//! Formato (o do `UsageSnapshot` dele, que não é contrato): `{status, windows:[{id,
//! label, used (0–1), resets_at (ms), duration_ms, count, group}], fetched_at (ms),
//! backoff_until (ms)}`. Janelas de outras contas do Claude têm `@slug` no id; as do
//! Spark e da revisão de código do Codex têm `group`; nenhuma das duas entra aqui.

use std::path::PathBuf;

use serde_json::Value;

use super::{Limits, Provider, Source, Window};

/// Leitura mais nova que isso: o Codenotch está aberto e consultando, e a rede não é
/// consultada daqui.
pub const FRESH_SECS: i64 = 10 * 60;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub limits: Option<Limits>,
    /// O Codenotch está esperando o servidor liberar (segundos Unix).
    pub backoff_until: Option<i64>,
}

impl Reading {
    /// Com o Codenotch cuidando (leitura recente ou recuo por 429), a consulta própria
    /// só gastaria o mesmo limite de requisições.
    pub fn holds_network(&self, now: i64) -> bool {
        self.limits.as_ref().is_some_and(|l| now - l.fetched_at < FRESH_SECS) || self.backoff_until.is_some_and(|b| b > now)
    }
}

fn dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("codenotch"))
}

pub fn read(provider: Provider) -> Reading {
    let file = match provider {
        Provider::Claude => "usage.json",
        Provider::Codex => "codex.json",
    };
    dir().and_then(|d| super::read_json(&d.join(file))).map(|v| parse(provider, &v)).unwrap_or_default()
}

fn ms(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_f64).filter(|n| *n > 0.0).map(|n| (n / 1000.0) as i64)
}

pub fn parse(provider: Provider, v: &Value) -> Reading {
    let backoff_until = ms(v.get("backoff_until"));
    let fetched_at = ms(v.get("fetched_at"));
    let windows: Vec<Window> = v
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let id = w.get("id")?.as_str()?;
            // Contagem pura, sem limite conhecido (não é porcentagem).
            if w.get("count").is_some_and(|c| !c.is_null()) {
                return None;
            }
            // Claude: `@slug` é outra conta (o `group` aqui é só o nome da conta, e vem em
            // todas as janelas quando há mais de uma). Codex: `group` é o Spark ou a
            // revisão de código, que não são o limite do plano.
            let other = match provider {
                Provider::Claude => id.contains('@'),
                Provider::Codex => w.get("group").is_some_and(|g| !g.is_null()),
            };
            if other {
                return None;
            }
            let used = w.get("used")?.as_f64()? as f32 * 100.0;
            let resets_at = ms(w.get("resets_at"));
            let duration = ms(w.get("duration_ms"));
            let id = match provider {
                Provider::Claude => super::claude_window_id(id),
                Provider::Codex => super::codex_window_id(duration, id),
            };
            let duration = duration.or_else(|| super::claude_window_duration(&id));
            Some(Window::new(&id, used, resets_at, duration))
        })
        .collect();
    let limits = match fetched_at {
        Some(fetched_at) if !windows.is_empty() => {
            let mut l = Limits { windows, source: Source::Codenotch, fetched_at, plan: None };
            l.sort();
            Some(l)
        }
        _ => None,
    };
    Reading { limits, backoff_until }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOUR, SEVEN_DAY};

    #[test]
    fn usage_json_do_claude() {
        let v = serde_json::json!({
            "status": "ok", "fetched_at": 1_800_000_000_000u64, "backoff_until": 0, "note": "",
            "windows": [
                {"id": "session", "label": "Current session", "used": 0.62, "resets_at": 1_800_010_000_000u64, "duration_ms": 18_000_000, "group": "Claude · pro"},
                {"id": "seven_day", "label": "Weekly", "used": 0.2, "resets_at": 1_800_500_000_000u64},
                {"id": "session@work", "label": "Current session", "used": 0.9, "resets_at": 1_800_010_000_000u64, "group": "work · max"},
                {"id": "spend", "label": "Spend", "used": 0.1}
            ]
        });
        let r = parse(Provider::Claude, &v);
        let l = r.limits.clone().unwrap();
        assert_eq!(l.fetched_at, 1_800_000_000);
        assert_eq!(l.windows.len(), 3);
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert!((l.windows[0].used - 62.0).abs() < 0.01);
        assert_eq!(l.windows[0].resets_at, Some(1_800_010_000));
        assert_eq!(l.windows[1].id, SEVEN_DAY);
        assert_eq!(l.windows[1].duration_secs, Some(super::super::SEVEN_DAYS_SECS));
        assert_eq!(r.backoff_until, None);
        assert!(r.holds_network(1_800_000_100));
        assert!(!r.holds_network(1_800_000_000 + FRESH_SECS + 1));
    }

    #[test]
    fn codex_json_sem_spark_e_em_recuo() {
        let v = serde_json::json!({
            "status": "stale", "fetched_at": 1_700_000_000_000u64, "backoff_until": 1_800_000_600_000u64,
            "windows": [
                {"id": "primary", "label": "5h limit", "used": 0.18, "resets_at": 1_800_000_000_000u64, "duration_ms": 18_000_000},
                {"id": "secondary", "label": "Weekly limit", "used": 0.4, "duration_ms": 604_800_000},
                {"id": "spark", "label": "5h limit", "used": 0.99, "group": "Spark"}
            ]
        });
        let r = parse(Provider::Codex, &v);
        let l = r.limits.clone().unwrap();
        assert_eq!(l.windows.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), [FIVE_HOUR, SEVEN_DAY]);
        // Leitura velha, mas em recuo: a rede espera.
        assert!(r.holds_network(1_800_000_000));
        assert!(!r.holds_network(1_800_000_700));
    }

    #[test]
    fn arquivo_vazio_ou_estranho() {
        assert_eq!(parse(Provider::Claude, &serde_json::json!({})), Reading::default());
        assert_eq!(parse(Provider::Claude, &serde_json::json!({"windows": [], "fetched_at": 5})).limits, None);
    }
}
