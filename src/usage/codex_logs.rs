//! Rollouts do Codex CLI: `<CODEX_HOME ou ~\.codex>\sessions\AAAA\MM\DD\rollout-*.jsonl`.
//! Só existem para conversas locais (o app com conversas na nuvem não grava aqui).
//!
//! - `session_meta` dá a sessão, a pasta e quem abriu (`originator`, `source`):
//!   `exec`/`mcp` é automação (`codex exec`), `source.subagent` é subagente.
//! - `turn_context` dá o modelo.
//! - `event_msg` do tipo `token_count` dá os tokens da rodada (`info.last_token_usage`) e
//!   os limites que o servidor informou nela (`rate_limits.primary/secondary`, com
//!   `used_percent`, `window_minutes` e `resets_at` em segundos ou `resets_in_seconds`).
//!   A % vale como "a do último uso": a hora da linha é a idade da leitura.
//! - O mesmo `token_count` pode vir repetido: só conta quando o total da sessão cresce.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::claude_logs::{KEEP_SECS, MAX_READ, collect, intern, next_lines, recent_cutoff};
use super::{Kind, Limits, Record, Source, Tokens, Window};

#[derive(Default)]
struct FileState {
    offset: u64,
    session: Option<Arc<str>>,
    cwd: Option<Arc<str>>,
    model: Option<Arc<str>>,
    kind: Kind,
    last_total: u64,
}

#[derive(Default)]
pub struct Scanner {
    files: HashMap<PathBuf, FileState>,
    records: Vec<Record>,
    names: HashMap<String, Arc<str>>,
    /// Os limites mais recentes vistos num rollout.
    limits: Option<Limits>,
}

impl Scanner {
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn limits(&self) -> Option<&Limits> {
        self.limits.as_ref()
    }

    pub fn update(&mut self, root: &Path, now: i64) {
        let mut files = Vec::new();
        collect(root, 0, &mut files);
        let cutoff = recent_cutoff();
        for (path, len, modified) in files {
            if !self.files.contains_key(&path) && modified.zip(cutoff).is_some_and(|(m, c)| m < c) {
                continue;
            }
            let state = self.files.entry(path.clone()).or_default();
            if len < state.offset {
                *state = FileState::default();
            }
            if len == state.offset {
                continue;
            }
            let mut offset = state.offset;
            let chunk = next_lines(&path, &mut offset, len, MAX_READ);
            state.offset = offset;
            let Some(chunk) = chunk else { continue };
            for line in chunk.split(|b| *b == b'\n') {
                let Some(v) = std::str::from_utf8(line).ok().and_then(|t| serde_json::from_str::<Value>(t.trim()).ok()) else {
                    continue;
                };
                let (record, limits) = parse_line(&v, state, &mut self.names);
                if let Some(r) = record.filter(|r| r.ts >= now - KEEP_SECS) {
                    self.records.push(r);
                }
                if let Some(l) = limits
                    && self.limits.as_ref().is_none_or(|old| old.fetched_at <= l.fetched_at)
                {
                    self.limits = Some(l);
                }
            }
        }
        let cutoff = now - KEEP_SECS;
        self.records.retain(|r| r.ts >= cutoff);
        self.files.retain(|p, _| p.exists());
    }
}

/// Uma linha do rollout → (consumo da rodada, limites informados nela).
fn parse_line(v: &Value, state: &mut FileState, names: &mut HashMap<String, Arc<str>>) -> (Option<Record>, Option<Limits>) {
    let Some(payload) = v.get("payload") else { return (None, None) };
    let text = |k: &str| payload.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if let Some(id) = text("id") {
                state.session = Some(intern(names, id));
            }
            if let Some(cwd) = text("cwd") {
                state.cwd = Some(intern(names, cwd));
            }
            state.kind = codex_kind(payload);
            (None, None)
        }
        Some("turn_context") => {
            if let Some(model) = text("model") {
                state.model = Some(intern(names, model));
            }
            if let Some(cwd) = text("cwd") {
                state.cwd = Some(intern(names, cwd));
            }
            (None, None)
        }
        Some("event_msg") if text("type") == Some("token_count") => {
            let Some(ts) = v.get("timestamp").and_then(super::parse_time) else { return (None, None) };
            let limits = payload.get("rate_limits").and_then(|r| parse_limits(r, ts));
            (usage_record(payload, ts, state, names), limits)
        }
        _ => (None, None),
    }
}

fn usage_record(payload: &Value, ts: i64, state: &mut FileState, names: &mut HashMap<String, Arc<str>>) -> Option<Record> {
    let info = payload.get("info").filter(|i| i.is_object())?;
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let total = info.get("total_token_usage").map(|t| n(t, "total_tokens")).unwrap_or(0);
    if total > 0 && total <= state.last_total {
        return None;
    }
    state.last_total = total;
    let last = info.get("last_token_usage")?;
    let input = n(last, "input_tokens");
    let cached = n(last, "cached_input_tokens").min(input);
    let tokens = Tokens {
        requests: 1,
        input: input - cached,
        output: n(last, "output_tokens"),
        cache_create_5m: 0,
        cache_create_1h: 0,
        cache_read: cached,
    };
    let mut or_unknown = |v: &Option<Arc<str>>| v.clone().unwrap_or_else(|| intern(names, "?"));
    Some(Record {
        ts,
        project: or_unknown(&state.cwd),
        session: or_unknown(&state.session),
        model: or_unknown(&state.model),
        kind: state.kind,
        tokens,
    })
}

/// Quem abriu a sessão do Codex: `source` é `"cli"`, `"vscode"`, `"exec"`, `"mcp"` ou
/// `{"subagent": …}`; o `originator` (`codex_cli_rs`, `codex_exec`…) é o recuo.
fn codex_kind(meta: &Value) -> Kind {
    let source = meta.get("source");
    if source.is_some_and(|s| s.get("subagent").is_some() || s.as_str().is_some_and(|t| t.eq_ignore_ascii_case("subagent"))) {
        return Kind::Subagent;
    }
    let automated = |t: &str| {
        let t = t.to_ascii_lowercase();
        t == "exec" || t == "mcp" || t.ends_with("_exec") || t.ends_with("-exec")
    };
    let by_source = source.and_then(Value::as_str).is_some_and(automated);
    let by_originator = meta.get("originator").and_then(Value::as_str).is_some_and(automated);
    if by_source || by_originator { Kind::Automation } else { Kind::Interactive }
}

/// `rate_limits` de um `token_count`. Um balde que não é o `codex` (o Spark) fica de fora.
pub fn parse_limits(r: &Value, ts: i64) -> Option<Limits> {
    let id = r.get("limit_id").or_else(|| r.get("limitId")).and_then(Value::as_str);
    if id.is_some_and(|id| id != "codex") {
        return None;
    }
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(w) = r.get(key).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64) else { continue };
        let duration = w.get("window_minutes").and_then(Value::as_f64).filter(|m| *m > 0.0).map(|m| (m * 60.0) as i64);
        let resets_at = w.get("resets_at").and_then(super::parse_time).or_else(|| {
            w.get("resets_in_seconds").and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e9).map(|s| ts + s as i64)
        });
        let wid = super::codex_window_id(duration, key);
        if windows.iter().all(|x: &Window| x.id != wid) {
            windows.push(Window::new(&wid, used as f32, resets_at, duration));
        }
    }
    if windows.is_empty() {
        return None;
    }
    let plan = r.get("plan_type").and_then(Value::as_str).map(str::to_owned);
    let mut l = Limits { windows, source: Source::CodexLog, fetched_at: ts, plan };
    l.sort();
    Some(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOUR, SEVEN_DAY};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pm-ia codex {name} ção-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn now() -> i64 {
        super::super::parse_time(&serde_json::json!("2026-11-09T12:00:00Z")).unwrap()
    }

    #[test]
    fn rollout_do_codex_cli() {
        let dir = temp_dir("rollout");
        let day = dir.join("2026").join("11").join("09");
        std::fs::create_dir_all(&day).unwrap();
        let lines = [
            r#"{"timestamp":"2026-11-09T10:00:00Z","type":"session_meta","payload":{"id":"sess-1","cwd":"C:\\Users\\Usuário Teste\\Documentos\\Projeto Ação","originator":"codex_cli_rs","source":"cli"}}"#,
            r#"{"timestamp":"2026-11-09T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex","cwd":"C:\\Users\\Usuário Teste\\Documentos\\Projeto Ação"}}"#,
            r#"{"timestamp":"2026-11-09T10:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1500},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":500}},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1800000000},"secondary":{"used_percent":3,"window_minutes":10080,"resets_in_seconds":600},"plan_type":"plus"}}}"#,
            // Repetido: não conta de novo.
            r#"{"timestamp":"2026-11-09T10:01:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1500},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":500}}}}"#,
            // Spark: limites de outro balde.
            r#"{"timestamp":"2026-11-09T10:02:00Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex_bengalfox","primary":{"used_percent":90,"window_minutes":300}}}}"#,
            // Lixo no meio do arquivo.
            r#"{"timestamp":"2026-11-09T10:03:00Z","type":"event_msg","payload":"#,
            "\u{0}\u{1} não é JSON",
        ];
        std::fs::write(day.join("rollout-2026-11-09T10-00-00-sess-1.jsonl"), lines.join("\n") + "\n").unwrap();
        let mut s = Scanner::default();
        s.update(&dir, now());
        assert_eq!(s.records().len(), 1);
        let r = &s.records()[0];
        assert_eq!((&*r.session, &*r.model, r.kind), ("sess-1", "gpt-5-codex", Kind::Interactive));
        assert_eq!(super::super::project_name(&r.project), "Projeto Ação");
        assert_eq!((r.tokens.input, r.tokens.cache_read, r.tokens.output), (200, 800, 500));
        let l = s.limits().unwrap();
        assert_eq!(l.source, Source::CodexLog);
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert_eq!(l.windows[0].used, 12.5);
        assert_eq!(l.windows[1].id, SEVEN_DAY);
        assert_eq!(l.windows[1].resets_at, Some(l.fetched_at + 600));
        assert_eq!(l.plan.as_deref(), Some("plus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quem_abriu_a_sessao() {
        let kind = |meta: serde_json::Value| codex_kind(&meta);
        assert_eq!(kind(serde_json::json!({"originator": "codex_cli_rs", "source": "cli"})), Kind::Interactive);
        assert_eq!(kind(serde_json::json!({"originator": "codex_vscode", "source": "vscode"})), Kind::Interactive);
        assert_eq!(kind(serde_json::json!({"originator": "codex_exec", "source": "exec"})), Kind::Automation);
        assert_eq!(kind(serde_json::json!({"originator": "codex_exec"})), Kind::Automation);
        assert_eq!(kind(serde_json::json!({"source": "mcp"})), Kind::Automation);
        assert_eq!(kind(serde_json::json!({"source": {"subagent": "review"}})), Kind::Subagent);
        // Formato antigo, sem nada disso.
        assert_eq!(kind(serde_json::json!({"id": "x"})), Kind::Interactive);
    }

    #[test]
    fn sem_pasta_ou_vazia() {
        let dir = temp_dir("vazia");
        let mut s = Scanner::default();
        s.update(&dir, now());
        std::fs::create_dir_all(&dir).unwrap();
        s.update(&dir, now());
        assert!(s.records().is_empty() && s.limits().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
