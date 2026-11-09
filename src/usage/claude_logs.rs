//! Consumo do Claude Code pelos `.jsonl` de `~/.claude/projects`: cada resposta do
//! modelo (`type: assistant`) traz `message.usage`, o `model`, a pasta (`cwd`), a sessão
//! e a hora. Dá o ranking por projeto e sessão, não a % do limite: a Anthropic não
//! publica quantos tokens valem 100%.
//!
//! - Leitura aos pedaços: guarda até onde leu cada arquivo e lê só o que cresceu, até
//!   a última linha completa. Arquivos sem mudança há mais de [`KEEP_SECS`] nem abrem.
//! - A mesma resposta aparece em várias linhas (uma por bloco de conteúdo), e uma
//!   sessão retomada copia as linhas da anterior: a chave é `message.id` + `requestId`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::{Record, Tokens};

/// Guarda os registros da última semana, com folga.
pub const KEEP_SECS: i64 = 8 * 86_400;
/// Uma leitura não passa disso por arquivo: a primeira varredura não trava a thread
/// num arquivo enorme, e o resto vem na rodada seguinte.
const MAX_READ: u64 = 64 * 1024 * 1024;

#[derive(Default)]
struct FileState {
    offset: u64,
}

#[derive(Default)]
pub struct Scanner {
    files: HashMap<PathBuf, FileState>,
    /// Chave de cada resposta já contada → hora (para podar junto com os registros).
    seen: HashMap<String, i64>,
    records: Vec<Record>,
    /// Nomes repetidos em milhares de registros: uma cópia de cada.
    names: HashMap<String, Arc<str>>,
}

impl Scanner {
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Lê o que mudou desde a última vez nos `.jsonl` de `root` (`~/.claude/projects`).
    pub fn update(&mut self, root: &Path, now: i64) {
        let mut files = Vec::new();
        collect(root, 0, &mut files);
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(KEEP_SECS as u64);
        for (path, len, modified) in files {
            let tracked = self.files.contains_key(&path);
            if !tracked && modified.is_some_and(|m| m < cutoff) {
                continue;
            }
            let state = self.files.entry(path.clone()).or_default();
            // Encolheu: foi reescrito. Recomeça (o `seen` evita contar de novo).
            if len < state.offset {
                state.offset = 0;
            }
            if len == state.offset {
                continue;
            }
            let Some(chunk) = read_from(&path, state.offset, (len - state.offset).min(MAX_READ)) else { continue };
            // Só até a última linha completa: a que está sendo escrita fica para depois.
            let Some(end) = chunk.iter().rposition(|b| *b == b'\n') else { continue };
            state.offset += end as u64 + 1;
            for line in chunk[..end].split(|b| *b == b'\n') {
                if let Some((key, record)) = parse_line(line, &mut self.names)
                    && record.ts >= now - KEEP_SECS
                    && !self.seen.contains_key(&key)
                {
                    self.seen.insert(key, record.ts);
                    self.records.push(record);
                }
            }
        }
        self.prune(now);
    }

    fn prune(&mut self, now: i64) {
        let cutoff = now - KEEP_SECS;
        self.records.retain(|r| r.ts >= cutoff);
        self.seen.retain(|_, ts| *ts >= cutoff);
        self.files.retain(|p, _| p.exists());
        if self.names.len() > 4096 {
            self.names.retain(|_, name| Arc::strong_count(name) > 1);
        }
    }
}

/// `projects/<projeto>/<sessão>.jsonl` e `projects/<projeto>/<sessão>/subagents/*.jsonl`.
pub(super) fn collect(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64, Option<std::time::SystemTime>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let path = e.path();
        if meta.is_dir() {
            if depth < 3 {
                collect(&path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|x| x == "jsonl") {
            out.push((path, meta.len(), meta.modified().ok()));
        }
    }
}

pub(super) fn read_from(path: &Path, offset: u64, len: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

pub(super) fn intern(names: &mut HashMap<String, Arc<str>>, s: &str) -> Arc<str> {
    if let Some(a) = names.get(s) {
        return a.clone();
    }
    let a: Arc<str> = s.into();
    names.insert(s.to_owned(), a.clone());
    a
}

/// Uma linha do `.jsonl` → (chave de deduplicação, registro).
pub fn parse_line(line: &[u8], names: &mut HashMap<String, Arc<str>>) -> Option<(String, Record)> {
    // Filtro barato antes do JSON: a maioria das linhas não é resposta do modelo.
    let text = std::str::from_utf8(line).ok()?;
    if !text.contains("\"usage\"") || !text.contains("\"assistant\"") {
        return None;
    }
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = v.get("message")?;
    let usage = message.get("usage")?;
    let model = message.get("model").and_then(Value::as_str).unwrap_or("");
    // Respostas sintéticas (erro, interrupção) não foram ao modelo.
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }
    let id = message.get("id").and_then(Value::as_str).unwrap_or("");
    let request = v.get("requestId").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() && request.is_empty() {
        return None;
    }
    let ts = super::parse_time(v.get("timestamp")?)?;
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let created = n(usage, "cache_creation_input_tokens");
    let (c5, c1) = match usage.get("cache_creation").filter(|c| c.is_object()) {
        Some(c) => (n(c, "ephemeral_5m_input_tokens"), n(c, "ephemeral_1h_input_tokens")),
        None => (created, 0),
    };
    let tokens = Tokens {
        requests: 1,
        input: n(usage, "input_tokens"),
        output: n(usage, "output_tokens"),
        cache_create_5m: c5,
        cache_create_1h: c1,
        cache_read: n(usage, "cache_read_input_tokens"),
    };
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("?");
    let session = v.get("sessionId").and_then(Value::as_str).unwrap_or("?");
    let record = Record {
        ts,
        project: intern(names, cwd),
        session: intern(names, session),
        model: intern(names, model),
        tokens,
    };
    Some((format!("{id}:{request}"), record))
}

pub fn root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| super::home().map(|h| h.join(".claude")))
        .map(|d| d.join("projects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"parentUuid":"x","type":"assistant","requestId":"req_1","timestamp":"2026-11-09T10:04:16.182Z","cwd":"D:\\Repos\\process-monitor","sessionId":"s1","message":{"model":"claude-opus-5-5","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"oi"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":28638,"cache_read_input_tokens":14392,"output_tokens":452,"cache_creation":{"ephemeral_1h_input_tokens":28638,"ephemeral_5m_input_tokens":0}}}}"#;

    #[test]
    fn linha_de_resposta() {
        let mut names = HashMap::new();
        let (key, r) = parse_line(LINE.as_bytes(), &mut names).unwrap();
        assert_eq!(key, "msg_1:req_1");
        assert_eq!(&*r.project, r"D:\Repos\process-monitor");
        assert_eq!(&*r.model, "claude-opus-5-5");
        assert_eq!(r.tokens, Tokens { requests: 1, input: 2, output: 452, cache_create_5m: 0, cache_create_1h: 28638, cache_read: 14392 });
        assert_eq!(Some(r.ts), super::super::parse_time(&serde_json::json!("2026-11-09T10:04:16Z")));
        // Linhas que não são resposta do modelo.
        assert!(parse_line(br#"{"type":"user","message":{"content":"usage"}}"#, &mut names).is_none());
        let synthetic = LINE.replace("claude-opus-5-5", "<synthetic>");
        assert!(parse_line(synthetic.as_bytes(), &mut names).is_none());
        // Sem o detalhe do cache: tudo como 5 min.
        let old = LINE.replace(r#","cache_creation":{"ephemeral_1h_input_tokens":28638,"ephemeral_5m_input_tokens":0}"#, "");
        assert_eq!(parse_line(old.as_bytes(), &mut names).unwrap().1.tokens.cache_create_5m, 28638);
    }

    #[test]
    fn leitura_incremental_e_sem_duplicar() {
        let dir = std::env::temp_dir().join(format!("pm-claude-logs-{}", std::process::id()));
        let project = dir.join("D--Repos-x");
        std::fs::create_dir_all(project.join("s1").join("subagents")).unwrap();
        let file = project.join("s1.jsonl");
        let now = super::super::parse_time(&serde_json::json!("2026-11-09T12:00:00Z")).unwrap();
        // A mesma resposta em duas linhas, mais uma linha pela metade.
        std::fs::write(&file, format!("{LINE}\n{LINE}\n{}", &LINE[..40])).unwrap();
        let mut s = Scanner::default();
        s.update(&dir, now);
        assert_eq!(s.records().len(), 1);
        // A linha terminou e chegou outra resposta; o subagente também conta.
        let second = LINE.replace("msg_1", "msg_2");
        std::fs::write(&file, format!("{LINE}\n{LINE}\n{second}\n")).unwrap();
        std::fs::write(project.join("s1").join("subagents").join("a.jsonl"), format!("{}\n", LINE.replace("msg_1", "msg_3"))).unwrap();
        s.update(&dir, now);
        assert_eq!(s.records().len(), 3);
        // Nada mudou: nada novo.
        s.update(&dir, now);
        assert_eq!(s.records().len(), 3);
        // Uma semana depois, tudo podado.
        s.update(&dir, now + KEEP_SECS + 60);
        assert!(s.records().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
