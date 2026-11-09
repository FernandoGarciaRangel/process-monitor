//! Arquivo-ponte da statusline do Claude Code. O Claude Code entrega ao script da
//! statusline, a cada atualização, o `rate_limits` da conta (`five_hour` e `seven_day`,
//! com `used_percentage` e `resets_at`). O `~/.claude/statusline.js` grava só isso, sem
//! nenhum segredo, em `%LOCALAPPDATA%\Process Monitor\claude-limits.json`:
//!
//! `{"version":1, "written_at": <ms>, "rate_limits": {"five_hour": {"used_percentage": 62,
//! "resets_at": …}, "seven_day": {…}}}`
//!
//! É a fonte oficial e de graça, mas só anda com uma sessão do Claude Code aberta. O
//! caminho é o mesmo para o build de debug e o instalado: o script não sabe qual dos
//! dois está rodando.

use std::path::PathBuf;

use serde_json::Value;

use super::{Limits, Source, Window};

/// Leitura mais nova que isso dispensa a consulta própria.
pub const FRESH_SECS: i64 = 5 * 60;

pub fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Process Monitor").join("claude-limits.json"))
}

pub fn read() -> Option<Limits> {
    parse(&super::read_json(&path()?)?)
}

pub fn parse(v: &Value) -> Option<Limits> {
    let written = super::parse_time(v.get("written_at")?)?;
    let limits = v.get("rate_limits")?.as_object()?;
    let windows: Vec<Window> = limits
        .iter()
        .filter_map(|(kind, w)| {
            let used = w.get("used_percentage").or_else(|| w.get("utilization"))?.as_f64()? as f32;
            let id = super::claude_window_id(kind);
            let resets_at = w.get("resets_at").and_then(super::parse_time);
            Some(Window::new(&id, used, resets_at, super::claude_window_duration(&id)))
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    let mut l = Limits { windows, source: Source::StatusLine, fetched_at: written, plan: None };
    l.sort();
    Some(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOUR, SEVEN_DAY};

    #[test]
    fn arquivo_da_statusline() {
        let v = serde_json::json!({
            "version": 1, "written_at": 1_800_000_000_500u64,
            "rate_limits": {
                "seven_day": {"used_percentage": 20.5, "resets_at": "2027-01-15T10:00:00Z"},
                "five_hour": {"used_percentage": 62, "resets_at": 1_800_010_000}
            }
        });
        let l = parse(&v).unwrap();
        assert_eq!(l.fetched_at, 1_800_000_000);
        assert_eq!(l.source, Source::StatusLine);
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert_eq!(l.windows[0].resets_at, Some(1_800_010_000));
        assert_eq!(l.windows[0].duration_secs, Some(crate::usage::FIVE_HOURS_SECS));
        assert_eq!(l.windows[1].id, SEVEN_DAY);
        assert!(l.windows[1].resets_at.is_some());
    }

    #[test]
    fn sem_limites_nao_e_leitura() {
        assert!(parse(&serde_json::json!({"written_at": 1, "rate_limits": {}})).is_none());
        assert!(parse(&serde_json::json!({"rate_limits": {"five_hour": {"used_percentage": 1}}})).is_none());
    }
}
