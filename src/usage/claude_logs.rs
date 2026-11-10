//! Consumo do Claude Code pelos `.jsonl` de `<CLAUDE_CONFIG_DIR ou ~\.claude>\projects`:
//! cada resposta do modelo (`type: assistant`) traz `message.usage`, o `model`, a pasta
//! (`cwd`), a sessão e a hora. Dá o ranking por projeto e sessão, não a % do limite: a
//! Anthropic não publica quantos tokens valem 100%.
//!
//! - Leitura aos pedaços: guarda até onde leu cada arquivo e lê só o que cresceu, até
//!   a última linha completa. Arquivos sem mudança há mais de [`KEEP_SECS`] nem abrem.
//! - A mesma resposta aparece em várias linhas (uma por bloco de conteúdo), e uma
//!   sessão retomada copia as linhas da anterior: a chave é `message.id` + `requestId`.
//! - Linha corrompida, cortada ou em outro formato é pulada; uma linha gigante (maior que
//!   a leitura máxima) é descartada inteira, sem travar o arquivo.
//! - Quem gerou: [`Kind::from_claude`] com o `entrypoint` e o `isSidechain` da linha, e a
//!   pasta `subagents` do arquivo.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::{Kind, Record, Tokens};

/// Guarda os registros da última semana, com folga.
pub const KEEP_SECS: i64 = 8 * 86_400;
/// Uma leitura não passa disso por arquivo: a primeira varredura não trava a thread
/// num arquivo enorme, e o resto vem na rodada seguinte.
pub const MAX_READ: u64 = 64 * 1024 * 1024;

#[derive(Default)]
struct FileState {
    offset: u64,
}

pub struct Scanner {
    files: HashMap<PathBuf, FileState>,
    /// Chave de cada resposta já contada → hora (para podar junto com os registros).
    seen: HashMap<String, i64>,
    records: Vec<Record>,
    /// Nomes repetidos em milhares de registros: uma cópia de cada.
    names: HashMap<String, Arc<str>>,
    max_read: u64,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::with_max_read(MAX_READ)
    }
}

impl Scanner {
    pub fn with_max_read(max_read: u64) -> Self {
        Self { files: HashMap::new(), seen: HashMap::new(), records: Vec::new(), names: HashMap::new(), max_read: max_read.max(1) }
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Lê o que mudou desde a última vez nos `.jsonl` de `root`. Pasta ausente ou vazia:
    /// nada, sem erro.
    pub fn update(&mut self, root: &Path, now: i64) {
        let mut files = Vec::new();
        collect(root, 0, &mut files);
        let cutoff = recent_cutoff();
        for (path, len, modified) in files {
            let tracked = self.files.contains_key(&path);
            if !tracked && modified.zip(cutoff).is_some_and(|(m, c)| m < c) {
                continue;
            }
            let subagent = in_subagents(&path);
            let max_read = self.max_read;
            let state = self.files.entry(path.clone()).or_default();
            // Encolheu: foi reescrito. Recomeça (o `seen` evita contar de novo).
            if len < state.offset {
                state.offset = 0;
            }
            if len == state.offset {
                continue;
            }
            let Some(chunk) = next_lines(&path, &mut state.offset, len, max_read) else { continue };
            for line in chunk.split(|b| *b == b'\n') {
                if let Some((key, record)) = parse_line(line, subagent, &mut self.names)
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

/// Arquivos sem mudança desde antes disso não têm registro na janela de uma semana.
pub(super) fn recent_cutoff() -> Option<std::time::SystemTime> {
    std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(KEEP_SECS as u64))
}

fn in_subagents(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str().eq_ignore_ascii_case("subagents"))
}

/// As linhas completas a partir de `offset`, avançando o `offset`. A linha que ainda está
/// sendo escrita fica para a próxima vez; uma linha maior que `max_read` é pulada.
pub(super) fn next_lines(path: &Path, offset: &mut u64, len: u64, max_read: u64) -> Option<Vec<u8>> {
    let want = (len - *offset).min(max_read);
    let mut chunk = read_from(path, *offset, want)?;
    match chunk.iter().rposition(|b| *b == b'\n') {
        Some(end) => {
            *offset += end as u64 + 1;
            chunk.truncate(end);
            Some(chunk)
        }
        // Nenhuma quebra em `max_read` bytes: uma linha gigante. Descarta até a próxima
        // quebra (ou o fim) para não ler o mesmo pedaço para sempre.
        None if want == max_read => {
            *offset += want;
            None
        }
        None => None,
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
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("jsonl")) {
            out.push((path, meta.len(), meta.modified().ok()));
        }
    }
}

pub(super) fn read_from(path: &Path, offset: u64, len: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::with_capacity(len.min(MAX_READ) as usize);
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

/// Uma linha do `.jsonl` → (chave de deduplicação, registro). `subagent`: o arquivo
/// está numa pasta `subagents`.
pub fn parse_line(line: &[u8], subagent: bool, names: &mut HashMap<String, Arc<str>>) -> Option<(String, Record)> {
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
    let usage = message.get("usage").filter(|u| u.is_object())?;
    let model = message.get("model").and_then(Value::as_str).unwrap_or("").trim();
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
    let sidechain = subagent
        || v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
        || v.get("agentId").is_some_and(|a| !a.is_null());
    let kind = Kind::from_claude(v.get("entrypoint").and_then(Value::as_str), sidechain);
    let cwd = v.get("cwd").and_then(Value::as_str).filter(|c| !c.trim().is_empty()).unwrap_or("?");
    let session = v.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("?");
    let record = Record {
        ts,
        project: intern(names, cwd),
        session: intern(names, session),
        model: intern(names, model),
        kind,
        tokens,
    };
    Some((format!("{id}:{request}"), record))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Uma resposta do modelo no formato atual, com pasta de exemplo (espaço, acento e
    /// OneDrive no caminho).
    pub const LINE: &str = r#"{"parentUuid":"x","isSidechain":false,"type":"assistant","requestId":"req_1","timestamp":"2026-11-09T10:04:16.182Z","userType":"external","entrypoint":"cli","cwd":"C:\\Users\\Usuária Teste\\OneDrive - Empresa\\Projetos\\Loja Online","sessionId":"s1","message":{"model":"claude-opus-5-5","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"oi"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":28638,"cache_read_input_tokens":14392,"output_tokens":452,"cache_creation":{"ephemeral_1h_input_tokens":28638,"ephemeral_5m_input_tokens":0}}}}"#;

    /// A mesma resposta num formato antigo: sem `entrypoint`, `userType`, `isSidechain` nem o
    /// detalhe do cache.
    const OLD: &str = r#"{"type":"assistant","requestId":"req_9","timestamp":"2025-03-01T08:00:00-03:00","cwd":"/home/dev/app","sessionId":"s9","message":{"model":"claude-3-5-sonnet-20241022","id":"msg_9","usage":{"input_tokens":10,"cache_creation_input_tokens":300,"cache_read_input_tokens":0,"output_tokens":20}}}"#;

    pub fn now() -> i64 {
        super::super::parse_time(&serde_json::json!("2026-11-09T12:00:00Z")).unwrap()
    }

    #[test]
    fn linha_de_resposta() {
        let mut names = HashMap::new();
        let (key, r) = parse_line(LINE.as_bytes(), false, &mut names).unwrap();
        assert_eq!(key, "msg_1:req_1");
        assert_eq!(&*r.project, r"C:\Users\Usuária Teste\OneDrive - Empresa\Projetos\Loja Online");
        assert_eq!(super::super::project_name(&r.project), "Loja Online");
        assert_eq!(&*r.model, "claude-opus-5-5");
        assert_eq!(r.kind, Kind::Interactive);
        assert_eq!(r.tokens, Tokens { requests: 1, input: 2, output: 452, cache_create_5m: 0, cache_create_1h: 28638, cache_read: 14392 });
        assert_eq!(Some(r.ts), super::super::parse_time(&serde_json::json!("2026-11-09T10:04:16Z")));
        // Linhas que não são resposta do modelo.
        assert!(parse_line(br#"{"type":"user","message":{"content":"usage"}}"#, false, &mut names).is_none());
        let synthetic = LINE.replace("claude-opus-5-5", "<synthetic>");
        assert!(parse_line(synthetic.as_bytes(), false, &mut names).is_none());
        // Formato antigo, com fuso: tudo como cache de 5 min, interativo.
        let (_, old) = parse_line(OLD.as_bytes(), false, &mut names).unwrap();
        assert_eq!(old.tokens.cache_create_5m, 300);
        assert_eq!(old.kind, Kind::Interactive);
        assert_eq!(Some(old.ts), super::super::parse_time(&serde_json::json!("2025-03-01T11:00:00Z")));
    }

    #[test]
    fn interativo_automacao_e_subagente() {
        let mut names = HashMap::new();
        let kind = |line: &str, sub: bool, names: &mut HashMap<String, Arc<str>>| parse_line(line.as_bytes(), sub, names).unwrap().1.kind;
        assert_eq!(kind(&LINE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#), false, &mut names), Kind::Automation);
        assert_eq!(kind(&LINE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-ts""#), false, &mut names), Kind::Automation);
        assert_eq!(kind(&LINE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"SDK-PY""#), false, &mut names), Kind::Automation);
        assert_eq!(kind(&LINE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"claude-desktop""#), false, &mut names), Kind::Interactive);
        assert_eq!(kind(&LINE.replace(r#""isSidechain":false"#, r#""isSidechain":true"#), false, &mut names), Kind::Subagent);
        assert_eq!(kind(&LINE.replace(r#""isSidechain":false"#, r#""agentId":"a1""#), false, &mut names), Kind::Subagent);
        // Subagente de uma automação continua subagente; arquivo na pasta `subagents` também.
        assert_eq!(kind(&LINE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#), true, &mut names), Kind::Subagent);
    }

    #[test]
    fn linhas_estragadas_nao_derrubam() {
        let mut names = HashMap::new();
        for bad in [
            &b""[..],
            b"{",
            b"\xff\xfe\x00 lixo",
            br#"{"type":"assistant","usage":1}"#,
            br#"{"type":"assistant","message":{"model":"m","id":"x","usage":"nao"}}"#,
            br#"{"type":"assistant","message":{"model":"m","id":"x","usage":{}},"timestamp":"ontem"}"#,
            br#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":-5}},"timestamp":1}"#,
        ] {
            assert!(parse_line(bad, false, &mut names).is_none());
        }
        // Números absurdos não estouram a soma.
        let huge = LINE.replace(r#""output_tokens":452"#, r#""output_tokens":18446744073709551615"#);
        let (_, r) = parse_line(huge.as_bytes(), false, &mut names).unwrap();
        let mut total = Tokens::default();
        total.add(&r.tokens);
        total.add(&r.tokens);
        assert_eq!(total.output, u64::MAX);
        assert_eq!(total.fresh(), u64::MAX);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pm-ia {name} ação-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn leitura_incremental_e_sem_duplicar() {
        let dir = temp_dir("incremental");
        let project = dir.join("C--Projetos-Loja Online");
        std::fs::create_dir_all(project.join("s1").join("subagents")).unwrap();
        let file = project.join("s1.jsonl");
        let now = now();
        // A mesma resposta em duas linhas, mais uma linha pela metade.
        std::fs::write(&file, format!("{LINE}\n{LINE}\n{}", &LINE[..40])).unwrap();
        let mut s = Scanner::default();
        s.update(&dir, now);
        assert_eq!(s.records().len(), 1);
        // A linha terminou e chegou outra resposta; o subagente também conta, como subagente.
        let second = LINE.replace("msg_1", "msg_2");
        std::fs::write(&file, format!("{LINE}\n{LINE}\n{second}\n")).unwrap();
        std::fs::write(project.join("s1").join("subagents").join("a.jsonl"), format!("{}\n", LINE.replace("msg_1", "msg_3"))).unwrap();
        s.update(&dir, now);
        assert_eq!(s.records().len(), 3);
        assert_eq!(s.records().iter().filter(|r| r.kind == Kind::Subagent).count(), 1);
        // Nada mudou: nada novo.
        s.update(&dir, now);
        assert_eq!(s.records().len(), 3);
        // O arquivo foi reescrito menor: relê sem contar de novo.
        std::fs::write(&file, format!("{second}\n")).unwrap();
        s.update(&dir, now);
        assert_eq!(s.records().len(), 3);
        // Uma semana depois, tudo podado.
        s.update(&dir, now + KEEP_SECS + 60);
        assert!(s.records().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linha_gigante_e_pastas_vazias() {
        let dir = temp_dir("gigante");
        let project = dir.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let mut s = Scanner::with_max_read(1024);
        // Pasta vazia e pasta que não existe: nada, sem erro.
        s.update(&dir, now());
        s.update(&dir.join("não existe"), now());
        assert!(s.records().is_empty());
        // Uma linha de 5 KB sem quebra, seguida de respostas normais: a gigante é pulada,
        // o resto chega nas rodadas seguintes.
        let giant = "x".repeat(5000);
        let second = LINE.replace("msg_1", "msg_2");
        std::fs::write(project.join("s.jsonl"), format!("{giant}\n{LINE}\n{second}\n")).unwrap();
        for _ in 0..10 {
            s.update(&dir, now());
        }
        assert_eq!(s.records().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
