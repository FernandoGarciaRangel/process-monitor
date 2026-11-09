//! Limites de uso do Claude e do Codex: quanto de cada janela (5 h e semanal) já foi
//! usado, quando ela renova, em que ritmo está acabando e o que está gastando
//! (projeto, sessão e modelo).
//!
//! A thread `usage` junta as fontes, da mais barata para a mais cara:
//! 1. os arquivos do Codenotch (`codenotch`), se ele estiver aberto: recentes ou em
//!    recuo por 429, a rede não é consultada, porque os dois apps dividiriam o limite;
//! 2. o arquivo-ponte que a statusline do Claude Code grava (`bridge`), que é oficial;
//! 3. os registros do Codex CLI (`codex_logs`), que trazem a % de cada rodada;
//! 4. a consulta própria (`claude_api`, `codex_api`), desligada por padrão, com piso de
//!    5 min, recuo exponencial com o `Retry-After` gravado em disco e sem nunca renovar
//!    nem gravar token.
//!
//! O consumo por projeto sai dos `.jsonl` do Claude Code (`claude_logs`) e dos rollouts
//! do Codex CLI, lidos aos pedaços (só o que cresceu) e só com a tela "IA" aberta.
//!
//! A disciplina da consulta (piso, recuo, não mandar token expirado, os formatos das
//! respostas) segue a do Codenotch (github.com/vinzdg/codenotch, licença MIT), que
//! resolveu esses mesmos problemas antes. O código aqui é próprio.

pub mod bridge;
pub mod claude_api;
pub mod claude_logs;
pub mod codenotch;
pub mod codex_api;
pub mod codex_logs;
pub mod gate;
mod monitor;

use std::path::PathBuf;

use serde_json::Value;

pub use monitor::Monitor;

/// Janela de 5 h e semanal: os ids que todas as fontes usam aqui.
pub const FIVE_HOUR: &str = "five_hour";
pub const SEVEN_DAY: &str = "seven_day";
pub const FIVE_HOURS_SECS: i64 = 5 * 3600;
pub const SEVEN_DAYS_SECS: i64 = 7 * 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// De onde veio a leitura das janelas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Codenotch,
    StatusLine,
    Api,
    AppServer,
    CodexLog,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Codenotch => "Codenotch",
            Source::StatusLine => "statusline do Claude Code",
            Source::Api => "consulta própria",
            Source::AppServer => "Codex app-server",
            Source::CodexLog => "último uso do Codex CLI",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Source::Codenotch => "codenotch",
            Source::StatusLine => "statusline",
            Source::Api => "api",
            Source::AppServer => "app-server",
            Source::CodexLog => "codex-log",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        [Source::Codenotch, Source::StatusLine, Source::Api, Source::AppServer, Source::CodexLog]
            .into_iter()
            .find(|s| s.key() == key)
    }
}

/// No ritmo atual, quando a janela acaba.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Pace {
    /// Sem dado para estimar.
    #[default]
    Unknown,
    /// Renova antes de acabar.
    Safe,
    /// Acaba nesse instante (segundos Unix), antes de renovar.
    RunsOutAt(i64),
    /// Já chegou a 100%.
    Exhausted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// [`FIVE_HOUR`], [`SEVEN_DAY`] ou outro (`seven_day_opus`, `monthly`).
    pub id: String,
    /// 0–100.
    pub used: f32,
    /// Segundos Unix.
    pub resets_at: Option<i64>,
    pub duration_secs: Option<i64>,
    pub pace: Pace,
}

impl Window {
    pub fn new(id: &str, used: f32, resets_at: Option<i64>, duration_secs: Option<i64>) -> Self {
        Self { id: id.to_owned(), used: used.clamp(0.0, 100.0), resets_at, duration_secs, pace: Pace::Unknown }
    }

    /// Nome para a tela e os avisos.
    pub fn label(&self) -> String {
        window_label(&self.id, self.duration_secs)
    }

    /// Início da janela atual, se a duração for conhecida.
    pub fn starts_at(&self) -> Option<i64> {
        Some(self.resets_at? - self.duration_secs?)
    }

    /// A leitura é de uma janela que já renovou: a % dela não vale mais.
    pub fn expired(&self, now: i64) -> bool {
        self.resets_at.is_some_and(|r| r <= now)
    }
}

pub fn window_label(id: &str, duration_secs: Option<i64>) -> String {
    match id {
        FIVE_HOUR => "5 h".to_owned(),
        SEVEN_DAY => "semanal".to_owned(),
        "seven_day_opus" => "semanal (Opus)".to_owned(),
        "seven_day_sonnet" => "semanal (Sonnet)".to_owned(),
        "seven_day_scoped" => "semanal (por modelo)".to_owned(),
        _ => match duration_secs {
            Some(d) if d >= 28 * 86_400 => "mensal".to_owned(),
            Some(d) if d >= 86_400 => format!("{} dias", d / 86_400),
            Some(d) if d >= 3600 => format!("{} h", d / 3600),
            Some(d) if d > 0 => format!("{} min", d / 60),
            _ => id.replace('_', " "),
        },
    }
}

/// "do limite de 5 h", "do limite semanal": o nome com a preposição certa.
pub fn window_phrase(id: &str, duration_secs: Option<i64>) -> String {
    let label = window_label(id, duration_secs);
    if label.starts_with(|c: char| c.is_ascii_digit()) { format!("de {label}") } else { label }
}

/// Janela do Codex pela duração: o primário nem sempre é de 5 h (o plano grátis já
/// mostrou 30 dias), então a posição não diz qual é.
pub fn codex_window_id(duration_secs: Option<i64>, fallback: &str) -> String {
    match duration_secs {
        Some(d) if (d - FIVE_HOURS_SECS).abs() <= 60 => FIVE_HOUR.to_owned(),
        Some(d) if (d - SEVEN_DAYS_SECS).abs() <= 3600 => SEVEN_DAY.to_owned(),
        Some(d) if d >= 28 * 86_400 => "monthly".to_owned(),
        Some(d) if d > 0 => format!("window_{}", d / 60),
        _ => fallback.to_owned(),
    }
}

/// Janelas do Claude: os nomes mudam conforme a fonte e a versão.
pub fn claude_window_id(kind: &str) -> String {
    match kind {
        "session" | "five_hour" => FIVE_HOUR.to_owned(),
        "seven_day" | "weekly_all" | "weekly" => SEVEN_DAY.to_owned(),
        "seven_day_opus" | "weekly_opus" => "seven_day_opus".to_owned(),
        "seven_day_sonnet" | "weekly_sonnet" => "seven_day_sonnet".to_owned(),
        "weekly_scoped" => "seven_day_scoped".to_owned(),
        other => other.to_owned(),
    }
}

pub fn claude_window_duration(id: &str) -> Option<i64> {
    if id == FIVE_HOUR {
        Some(FIVE_HOURS_SECS)
    } else if id.starts_with("seven_day") {
        Some(SEVEN_DAYS_SECS)
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub windows: Vec<Window>,
    pub source: Source,
    /// Quando a fonte leu (segundos Unix): é a idade mostrada na tela.
    pub fetched_at: i64,
    pub plan: Option<String>,
}

impl Limits {
    pub fn window(&self, id: &str) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Janelas na ordem da tela: 5 h, semanal e o resto.
    pub fn sort(&mut self) {
        let rank = |w: &Window| match w.id.as_str() {
            FIVE_HOUR => 0,
            SEVEN_DAY => 1,
            _ => 2,
        };
        self.windows.sort_by_key(rank);
    }

    /// O maior uso entre as janelas que ainda valem, para o contador da barra lateral.
    pub fn peak(&self, now: i64) -> Option<f32> {
        self.windows.iter().filter(|w| !w.expired(now)).map(|w| w.used).reduce(f32::max)
    }

    pub fn to_json(&self) -> Value {
        let windows: Vec<Value> = self
            .windows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id, "used": w.used, "resets_at": w.resets_at, "duration_secs": w.duration_secs,
                })
            })
            .collect();
        serde_json::json!({
            "windows": windows, "source": self.source.key(), "fetched_at": self.fetched_at, "plan": self.plan,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let windows: Vec<Window> = v
            .get("windows")?
            .as_array()?
            .iter()
            .filter_map(|w| {
                Some(Window::new(
                    w.get("id")?.as_str()?,
                    w.get("used")?.as_f64()? as f32,
                    w.get("resets_at").and_then(Value::as_i64),
                    w.get("duration_secs").and_then(Value::as_i64),
                ))
            })
            .collect();
        Some(Self {
            windows,
            source: Source::from_key(v.get("source")?.as_str()?)?,
            fetched_at: v.get("fetched_at")?.as_i64()?,
            plan: v.get("plan").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// Tokens de um conjunto de respostas. Cache criado de 1 h e de 5 min ficam separados:
/// custam diferente, e a Anthropic não publica quanto cada um pesa no limite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_create_5m: u64,
    pub cache_create_1h: u64,
    pub cache_read: u64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.requests += o.requests;
        self.input += o.input;
        self.output += o.output;
        self.cache_create_5m += o.cache_create_5m;
        self.cache_create_1h += o.cache_create_1h;
        self.cache_read += o.cache_read;
    }

    pub fn cache_create(&self) -> u64 {
        self.cache_create_5m + self.cache_create_1h
    }

    /// Para ordenar o ranking: tudo o que o modelo processou de novo. O cache lido fica de
    /// fora, porque é barato e dominaria a soma.
    pub fn fresh(&self) -> u64 {
        self.input + self.output + self.cache_create()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionUse {
    pub id: String,
    pub first: i64,
    pub last: i64,
    /// O modelo que mais gastou na sessão.
    pub model: String,
    pub tokens: Tokens,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectUse {
    /// Pasta do projeto (`cwd`).
    pub path: String,
    pub tokens: Tokens,
    pub sessions: Vec<SessionUse>,
    pub models: Vec<(String, Tokens)>,
}

impl ProjectUse {
    pub fn name(&self) -> &str {
        let trimmed = self.path.trim_end_matches(['\\', '/']);
        trimmed.rsplit(['\\', '/']).next().filter(|n| !n.is_empty()).unwrap_or(trimmed)
    }
}

/// Consumo num período: projetos, do que mais gastou para o que menos.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Consumption {
    /// Início do período (segundos Unix).
    pub since: i64,
    /// O período é a janela atual (o início veio do `resets_at`), não as últimas horas.
    pub aligned: bool,
    pub total: Tokens,
    pub projects: Vec<ProjectUse>,
}

/// Um registro de consumo: uma resposta do modelo.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub ts: i64,
    pub project: std::sync::Arc<str>,
    pub session: std::sync::Arc<str>,
    pub model: std::sync::Arc<str>,
    pub tokens: Tokens,
}

/// Agrupa por projeto e sessão os registros a partir de `since`.
pub fn aggregate<'a>(records: impl Iterator<Item = &'a Record>, since: i64, aligned: bool) -> Consumption {
    use std::collections::HashMap;
    use std::sync::Arc;
    #[derive(Default)]
    struct Session {
        first: i64,
        last: i64,
        tokens: Tokens,
        models: HashMap<Arc<str>, Tokens>,
    }
    struct Acc {
        tokens: Tokens,
        sessions: HashMap<Arc<str>, Session>,
        models: HashMap<Arc<str>, Tokens>,
    }
    let mut total = Tokens::default();
    let mut projects: HashMap<Arc<str>, Acc> = HashMap::new();
    for r in records.filter(|r| r.ts >= since) {
        total.add(&r.tokens);
        let acc = projects
            .entry(r.project.clone())
            .or_insert_with(|| Acc { tokens: Tokens::default(), sessions: HashMap::new(), models: HashMap::new() });
        acc.tokens.add(&r.tokens);
        acc.models.entry(r.model.clone()).or_default().add(&r.tokens);
        let s = acc.sessions.entry(r.session.clone()).or_insert_with(|| Session { first: r.ts, last: r.ts, ..Session::default() });
        s.first = s.first.min(r.ts);
        s.last = s.last.max(r.ts);
        s.tokens.add(&r.tokens);
        s.models.entry(r.model.clone()).or_default().add(&r.tokens);
    }
    let mut out: Vec<ProjectUse> = projects
        .into_iter()
        .map(|(path, acc)| {
            let mut sessions: Vec<SessionUse> = acc
                .sessions
                .into_iter()
                .map(|(id, s)| SessionUse {
                    id: id.to_string(),
                    first: s.first,
                    last: s.last,
                    model: s.models.into_iter().max_by_key(|(_, t)| t.fresh()).map(|(m, _)| m.to_string()).unwrap_or_default(),
                    tokens: s.tokens,
                })
                .collect();
            sessions.sort_by(|a, b| b.tokens.fresh().cmp(&a.tokens.fresh()).then(b.last.cmp(&a.last)));
            let mut models: Vec<(String, Tokens)> = acc.models.into_iter().map(|(m, t)| (m.to_string(), t)).collect();
            models.sort_by_key(|(_, t)| std::cmp::Reverse(t.fresh()));
            ProjectUse { path: path.to_string(), tokens: acc.tokens, sessions, models }
        })
        .collect();
    out.sort_by(|a, b| b.tokens.fresh().cmp(&a.tokens.fresh()).then_with(|| a.path.cmp(&b.path)));
    Consumption { since, aligned, total, projects: out }
}

/// Estado de um provedor publicado para a interface.
#[derive(Clone, Debug, Default)]
pub struct ProviderState {
    /// A leitura mais recente entre as fontes.
    pub limits: Option<Limits>,
    /// O que impede uma leitura melhor (token expirado, recuo por 429, nenhuma fonte).
    pub note: Option<String>,
    /// Uso de cada janela a cada rodada, para o gráfico (id → série).
    pub history: Vec<(String, crate::sampler::Series)>,
    /// Janela de 5 h atual (ou últimas 5 h) e semana.
    pub recent: Option<Consumption>,
    pub week: Option<Consumption>,
    /// A fonte de consumo existe nesta máquina (pasta de registros).
    pub has_logs: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub claude: ProviderState,
    pub codex: ProviderState,
    /// A consulta própria está ligada nas configurações.
    pub network: bool,
    /// Já leu os registros de consumo pelo menos uma vez.
    pub scanned: bool,
}

impl Usage {
    pub fn get(&self, p: Provider) -> &ProviderState {
        match p {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }
}

/// Previsão pelo ritmo. Com pontos que cobrem pelo menos 10 min, usa a inclinação
/// deles (o ritmo de agora); senão, a média desde o início da janela.
pub fn forecast(w: &Window, points: &[(i64, f32)], now: i64) -> Pace {
    if w.used >= 99.95 {
        return Pace::Exhausted;
    }
    let recent = points.iter().filter(|(t, _)| now - t <= 3600).collect::<Vec<_>>();
    let slope = match (recent.first(), recent.last()) {
        (Some((t0, u0)), Some((t1, u1))) if t1 - t0 >= 600 && u1 > u0 => Some((u1 - u0) as f64 / (t1 - t0) as f64),
        (Some((t0, _)), Some((t1, _))) if t1 - t0 >= 600 => Some(0.0),
        _ => None,
    };
    let rate = slope.or_else(|| {
        let start = w.starts_at()?;
        let elapsed = now - start;
        (elapsed >= 300).then(|| w.used as f64 / elapsed as f64)
    });
    let Some(rate) = rate else { return Pace::Unknown };
    if rate <= 0.0 {
        return if w.resets_at.is_some() { Pace::Safe } else { Pace::Unknown };
    }
    let eta = now + ((100.0 - w.used as f64) / rate).round() as i64;
    match w.resets_at {
        Some(r) if eta >= r => Pace::Safe,
        _ => Pace::RunsOutAt(eta),
    }
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Segundos Unix de um número (segundos ou milissegundos) ou de um texto (número ou
/// RFC 3339). As fontes não combinam o formato entre si.
pub fn parse_time(v: &Value) -> Option<i64> {
    fn from_number(n: f64) -> Option<i64> {
        if !n.is_finite() || n <= 0.0 {
            return None;
        }
        // Acima de 1e11 só pode ser milissegundo (1e11 s é o ano 5138).
        Some(if n > 1e11 { (n / 1000.0) as i64 } else { n as i64 })
    }
    match v {
        Value::Number(n) => from_number(n.as_f64()?),
        Value::String(s) => {
            let s = s.trim();
            match s.parse::<f64>() {
                Ok(n) => from_number(n),
                Err(_) => chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp()),
            }
        }
        _ => None,
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Pasta de dados do app (`%LOCALAPPDATA%\Process Monitor`, ou a do dev).
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(crate::platform::APP_NAME)
}

/// Grava num temporário e troca: quem lê nunca pega o arquivo pela metade.
pub fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Lê um JSON pequeno, aceitando o BOM que o PowerShell e o Bloco de Notas põem.
pub fn read_json(path: &std::path::Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_em_qualquer_formato() {
        assert_eq!(parse_time(&serde_json::json!(1_800_000_000)), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!(1_800_000_000_123u64)), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!("1800000000")), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!("2026-10-10T00:00:00Z")), Some(1_791_590_400));
        assert_eq!(parse_time(&serde_json::json!("2026-10-10T00:00:00.5+00:00")), Some(1_791_590_400));
        assert_eq!(parse_time(&serde_json::json!(null)), None);
        assert_eq!(parse_time(&serde_json::json!(-5)), None);
        assert_eq!(parse_time(&serde_json::json!("amanhã")), None);
    }

    #[test]
    fn janelas_do_codex_pela_duracao() {
        assert_eq!(codex_window_id(Some(18_000), "primary"), FIVE_HOUR);
        assert_eq!(codex_window_id(Some(604_800), "secondary"), SEVEN_DAY);
        assert_eq!(codex_window_id(Some(2_592_000), "primary"), "monthly");
        assert_eq!(codex_window_id(None, "primary"), "primary");
        assert_eq!(window_label("monthly", Some(2_592_000)), "mensal");
        assert_eq!(window_label("window_90", Some(5400)), "1 h");
        assert_eq!(claude_window_id("session"), FIVE_HOUR);
        assert_eq!(claude_window_id("weekly_all"), SEVEN_DAY);
        assert_eq!(claude_window_duration("seven_day_opus"), Some(SEVEN_DAYS_SECS));
    }

    #[test]
    fn leitura_vai_e_volta_em_json() {
        let mut l = Limits {
            windows: vec![Window::new(SEVEN_DAY, 20.0, Some(2_000), Some(SEVEN_DAYS_SECS)), Window::new(FIVE_HOUR, 61.5, None, None)],
            source: Source::Api,
            fetched_at: 1_000,
            plan: Some("pro".into()),
        };
        l.sort();
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert_eq!(Limits::from_json(&l.to_json()), Some(l.clone()));
        assert_eq!(l.peak(1_500), Some(61.5));
        // A semanal renovou em 2000: não conta mais.
        assert_eq!(l.peak(2_500), Some(61.5));
    }

    #[test]
    fn previsao_pelo_ritmo() {
        let now = 1_000_000;
        // 50% em 2 h de uma janela de 5 h: acaba em mais 2 h, antes de renovar (3 h).
        let w = Window::new(FIVE_HOUR, 50.0, Some(now + 3 * 3600), Some(FIVE_HOURS_SECS));
        assert_eq!(forecast(&w, &[], now), Pace::RunsOutAt(now + 2 * 3600));
        // Ritmo recente mais lento (10 pontos em 30 min): não acaba antes de renovar.
        let points = [(now - 1800, 45.0), (now, 50.0)];
        assert_eq!(forecast(&w, &points, now), Pace::Safe);
        // Parado há 20 min: seguro.
        assert_eq!(forecast(&w, &[(now - 1200, 50.0), (now, 50.0)], now), Pace::Safe);
        // Ritmo recente rápido: 20 pontos em 20 min → acaba em 50 min.
        assert_eq!(forecast(&w, &[(now - 1200, 30.0), (now, 50.0)], now), Pace::RunsOutAt(now + 3000));
        // Cheia.
        assert_eq!(forecast(&Window::new(FIVE_HOUR, 100.0, None, None), &[], now), Pace::Exhausted);
        // Sem início conhecido e sem pontos: não dá para estimar.
        assert_eq!(forecast(&Window::new(FIVE_HOUR, 10.0, None, None), &[], now), Pace::Unknown);
    }

    fn rec(ts: i64, project: &str, session: &str, model: &str, output: u64) -> Record {
        Record {
            ts,
            project: project.into(),
            session: session.into(),
            model: model.into(),
            tokens: Tokens { requests: 1, output, cache_read: 1000, ..Tokens::default() },
        }
    }

    #[test]
    fn ranking_por_projeto_e_sessao() {
        let records = [
            rec(100, r"D:\Repos\a", "s1", "opus", 10),
            rec(200, r"D:\Repos\a", "s1", "sonnet", 50),
            rec(300, r"D:\Repos\a", "s2", "opus", 5),
            rec(400, r"D:\Repos\b\", "s3", "opus", 100),
            rec(50, r"D:\Repos\c", "s4", "opus", 999),
        ];
        let c = aggregate(records.iter(), 100, false);
        assert_eq!(c.total.requests, 4);
        assert_eq!(c.total.output, 165);
        assert_eq!(c.projects.len(), 2);
        assert_eq!(c.projects[0].name(), "b");
        assert_eq!(c.projects[1].name(), "a");
        let a = &c.projects[1];
        assert_eq!(a.sessions[0].id, "s1");
        assert_eq!((a.sessions[0].first, a.sessions[0].last), (100, 200));
        assert_eq!(a.sessions[0].model, "sonnet");
        assert_eq!(a.models[0].0, "sonnet");
    }
}
