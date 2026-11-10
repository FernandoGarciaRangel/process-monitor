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
    /// Ritmo recente, em pontos percentuais por segundo (preenchido pela thread).
    pub rate: Option<f64>,
}

/// Datas fora disso são lixo de uma fonte (ano 2000 a 2100): somar ou subtrair com elas
/// poderia estourar o `i64`.
const SANE_TIME: std::ops::Range<i64> = 946_684_800..4_102_444_800;
/// Nenhuma janela de uso passa de um ano.
const SANE_DURATION: std::ops::RangeInclusive<i64> = 1..=366 * 86_400;

impl Window {
    pub fn new(id: &str, used: f32, resets_at: Option<i64>, duration_secs: Option<i64>) -> Self {
        let used = if used.is_finite() { used.clamp(0.0, 100.0) } else { 0.0 };
        Self {
            id: id.to_owned(),
            used,
            resets_at: resets_at.filter(|r| SANE_TIME.contains(r)),
            duration_secs: duration_secs.filter(|d| SANE_DURATION.contains(d)),
            pace: Pace::Unknown,
            rate: None,
        }
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

    /// 5 h e semanal: as janelas que todo plano tem.
    pub fn is_main(&self) -> bool {
        matches!(self.id.as_str(), FIVE_HOUR | SEVEN_DAY)
    }

    /// Tem uso que aparece na tela (`format::percent` mostra "0%" abaixo de 0,05) numa
    /// leitura que ainda vale.
    pub fn has_use(&self, now: i64) -> bool {
        !self.expired(now) && self.used >= 0.05
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

    /// Janelas da tela: 5 h e semanal sempre; as extras (semanal por modelo, Opus,
    /// Sonnet) só com uso, porque em 0% ou já renovadas não dizem nada. Sem nenhuma das
    /// principais (Codex grátis, só com a mensal), todas ficam: senão o cartão sairia vazio.
    pub fn shown(&self, now: i64) -> impl Iterator<Item = &Window> {
        let has_main = self.windows.iter().any(Window::is_main);
        self.windows.iter().filter(move |w| !has_main || w.is_main() || w.has_use(now))
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
    /// Soma saturando: um registro corrompido com número absurdo não derruba a thread.
    pub fn add(&mut self, o: &Tokens) {
        self.requests = self.requests.saturating_add(o.requests);
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_create_5m = self.cache_create_5m.saturating_add(o.cache_create_5m);
        self.cache_create_1h = self.cache_create_1h.saturating_add(o.cache_create_1h);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
    }

    pub fn cache_create(&self) -> u64 {
        self.cache_create_5m.saturating_add(self.cache_create_1h)
    }

    /// Para ordenar o ranking: tudo o que o modelo processou de novo. O cache lido fica de
    /// fora, porque é barato e dominaria a soma.
    pub fn fresh(&self) -> u64 {
        self.input.saturating_add(self.output).saturating_add(self.cache_create())
    }
}

/// Quem gerou o consumo. Vem de campos genéricos dos registros, nunca do nome de uma
/// ferramenta: no Claude Code, `isSidechain`/`agentId`/pasta `subagents` (subagente) e
/// `entrypoint` começando com `sdk` (`sdk-cli` é o `claude -p`, `sdk-ts`, `sdk-py`);
/// no Codex, `originator`/`source` `exec` ou `mcp`, e `source.subagent`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    #[default]
    Interactive,
    Automation,
    Subagent,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Interactive, Kind::Automation, Kind::Subagent];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Interactive => "Interativo",
            Kind::Automation => "Automação (claude -p / SDK)",
            Kind::Subagent => "Subagentes",
        }
    }

    /// Claude Code: o `entrypoint` de cada linha. Sem o campo (formatos antigos), conta
    /// como interativo, que era o único jeito de usar.
    pub fn from_claude(entrypoint: Option<&str>, sidechain: bool) -> Self {
        if sidechain {
            Kind::Subagent
        } else if entrypoint.is_some_and(|e| e.trim().to_ascii_lowercase().starts_with("sdk")) {
            Kind::Automation
        } else {
            Kind::Interactive
        }
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
        project_name(&self.path)
    }
}

/// Última parte do caminho, com `\` ou `/`: `C:\Projetos\Loja Online` → `Loja Online`.
pub fn project_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().filter(|n| !n.is_empty()).unwrap_or(trimmed)
}

/// Uma das sessões que mais gastaram no período.
#[derive(Clone, Debug, PartialEq)]
pub struct TopSession {
    pub project: String,
    pub session: SessionUse,
    /// A pasta ainda existe (conferido pela thread, não a cada quadro).
    pub folder_exists: bool,
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
    /// Por família de modelo (Opus, Sonnet, Haiku…), do que mais gastou para o que menos.
    pub models: Vec<(String, Tokens)>,
    /// Interativo, automação e subagentes, sempre nessa ordem.
    pub kinds: Vec<(Kind, Tokens)>,
    /// As 5 sessões mais pesadas.
    pub top: Vec<TopSession>,
}

/// Um registro de consumo: uma resposta do modelo.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub ts: i64,
    pub project: std::sync::Arc<str>,
    pub session: std::sync::Arc<str>,
    pub model: std::sync::Arc<str>,
    pub kind: Kind,
    pub tokens: Tokens,
}

/// Família do modelo para o peso: `claude-opus-5-5` e `claude-3-5-sonnet-20241022` →
/// `Opus`, `Sonnet`. Nomes sem o padrão do Claude (`gpt-5-codex`) ficam como estão.
pub fn model_family(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("claude") {
        let family = rest.split(['-', '_', '.', ' ']).find(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphabetic()));
        if let Some(f) = family {
            let mut c = f.chars();
            return c.next().map(|h| h.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default();
        }
    }
    if model.trim().is_empty() || model == "?" { "Desconhecido".to_owned() } else { model.trim().to_owned() }
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
    let mut families: HashMap<String, Tokens> = HashMap::new();
    let mut kinds: HashMap<Kind, Tokens> = HashMap::new();
    let mut projects: HashMap<Arc<str>, Acc> = HashMap::new();
    for r in records.filter(|r| r.ts >= since) {
        total.add(&r.tokens);
        families.entry(model_family(&r.model)).or_default().add(&r.tokens);
        kinds.entry(r.kind).or_default().add(&r.tokens);
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
                    model: s.models.into_iter().max_by(|a, b| a.1.fresh().cmp(&b.1.fresh()).then(b.0.cmp(&a.0))).map(|(m, _)| m.to_string()).unwrap_or_default(),
                    tokens: s.tokens,
                })
                .collect();
            sessions.sort_by(|a, b| b.tokens.fresh().cmp(&a.tokens.fresh()).then(b.last.cmp(&a.last)).then(a.id.cmp(&b.id)));
            let mut models: Vec<(String, Tokens)> = acc.models.into_iter().map(|(m, t)| (m.to_string(), t)).collect();
            models.sort_by(|a, b| b.1.fresh().cmp(&a.1.fresh()).then(a.0.cmp(&b.0)));
            ProjectUse { path: path.to_string(), tokens: acc.tokens, sessions, models }
        })
        .collect();
    out.sort_by(|a, b| b.tokens.fresh().cmp(&a.tokens.fresh()).then_with(|| a.path.cmp(&b.path)));
    let mut top: Vec<TopSession> = out
        .iter()
        .flat_map(|p| p.sessions.iter().map(|s| TopSession { project: p.path.clone(), session: s.clone(), folder_exists: false }))
        .collect();
    top.sort_by(|a, b| b.session.tokens.fresh().cmp(&a.session.tokens.fresh()).then(b.session.last.cmp(&a.session.last)));
    top.truncate(5);
    let mut models: Vec<(String, Tokens)> = families.into_iter().collect();
    models.sort_by(|a, b| b.1.fresh().cmp(&a.1.fresh()).then(a.0.cmp(&b.0)));
    let kinds = Kind::ALL.iter().map(|k| (*k, kinds.get(k).copied().unwrap_or_default())).collect();
    Consumption { since, aligned, total, projects: out, models, kinds, top }
}

/// Parte de um todo, em %, sem dividir por zero.
pub fn share(part: u64, total: u64) -> f32 {
    if total == 0 { 0.0 } else { (part as f64 / total as f64 * 100.0) as f32 }
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
    /// A pasta de registros de consumo existe nesta máquina.
    pub has_logs: bool,
    /// O programa está instalado (a pasta de configuração dele existe).
    pub installed: bool,
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

/// Ritmo, em pontos percentuais por segundo. Com pontos que cobrem pelo menos 10 min,
/// usa a inclinação deles (o ritmo de agora); senão, a média desde o início da janela.
pub fn rate(w: &Window, points: &[(i64, f32)], now: i64) -> Option<f64> {
    let recent = points.iter().filter(|(t, _)| now.saturating_sub(*t) <= 3600).collect::<Vec<_>>();
    let slope = match (recent.first(), recent.last()) {
        (Some((t0, u0)), Some((t1, u1))) if t1.saturating_sub(*t0) >= 600 && u1 > u0 => {
            Some((u1 - u0) as f64 / t1.saturating_sub(*t0) as f64)
        }
        (Some((t0, _)), Some((t1, _))) if t1.saturating_sub(*t0) >= 600 => Some(0.0),
        _ => None,
    };
    slope.or_else(|| {
        let start = w.starts_at()?;
        let elapsed = now.saturating_sub(start);
        (elapsed >= 300).then(|| w.used as f64 / elapsed as f64)
    })
}

/// Previsão pelo ritmo ([`rate`]).
pub fn forecast(w: &Window, points: &[(i64, f32)], now: i64) -> Pace {
    if w.used >= 99.95 {
        return Pace::Exhausted;
    }
    let Some(rate) = rate(w, points, now) else { return Pace::Unknown };
    if rate <= 0.0 {
        return if w.resets_at.is_some() { Pace::Safe } else { Pace::Unknown };
    }
    let eta = now.saturating_add(((100.0 - w.used as f64) / rate).round().min(1e12) as i64);
    match w.resets_at {
        Some(r) if eta >= r => Pace::Safe,
        _ => Pace::RunsOutAt(eta),
    }
}

/// Unidade do orçamento: a janela semanal (ou maior) por dia, as curtas por hora.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetUnit {
    Hour,
    Day,
}

impl BudgetUnit {
    pub fn secs(self) -> i64 {
        match self {
            BudgetUnit::Hour => 3600,
            BudgetUnit::Day => 86_400,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BudgetUnit::Hour => "hora",
            BudgetUnit::Day => "dia",
        }
    }
}

/// Quanto dá para gastar por hora (ou por dia) para a janela durar até renovar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub unit: BudgetUnit,
    /// Pontos percentuais por unidade.
    pub per_unit: f32,
    /// Falta menos de uma unidade até renovar: `per_unit` é tudo o que resta.
    pub last_stretch: bool,
    /// O ritmo atual, na mesma unidade.
    pub current: Option<f32>,
}

pub fn budget(w: &Window, now: i64) -> Option<Budget> {
    let reset = w.resets_at.filter(|r| *r > now)?;
    let left = reset.saturating_sub(now);
    let unit = if w.duration_secs.unwrap_or(0) >= 2 * 86_400 || left > 2 * 86_400 { BudgetUnit::Day } else { BudgetUnit::Hour };
    let units_left = left as f64 / unit.secs() as f64;
    let remaining = (100.0 - w.used).max(0.0) as f64;
    let last_stretch = units_left <= 1.0;
    let per_unit = if last_stretch { remaining } else { remaining / units_left };
    Some(Budget {
        unit,
        per_unit: per_unit as f32,
        last_stretch,
        current: w.rate.map(|r| (r * unit.secs() as f64) as f32),
    })
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Segundos Unix de um número (segundos ou milissegundos) ou de um texto (número ou
/// RFC 3339, com qualquer fuso). As fontes não combinam o formato entre si.
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

/// Pastas do Windows que não estão numa variável de ambiente (raro: serviço, conta
/// sem perfil carregado). Lidas pelo `SHGetKnownFolderPath`, sem administrador.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownFolder {
    Profile,
    RoamingAppData,
    LocalAppData,
}

pub fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    let id = match folder {
        KnownFolder::Profile => &FOLDERID_Profile,
        KnownFolder::RoamingAppData => &FOLDERID_RoamingAppData,
        KnownFolder::LocalAppData => &FOLDERID_LocalAppData,
    };
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as *const _));
        path
    }
}

/// Onde ficam as coisas nesta máquina. Tudo sai das variáveis do Windows (`USERPROFILE`,
/// `APPDATA`, `LOCALAPPDATA`) ou das pastas conhecidas, e respeita `CLAUDE_CONFIG_DIR` e
/// `CODEX_HOME`. Nenhum caminho fixo: vale em qualquer usuário, com espaço, acento ou
/// OneDrive no caminho.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Paths {
    pub home: Option<PathBuf>,
    pub roaming: Option<PathBuf>,
    pub local: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR` ou `~\.claude`.
    pub claude_dir: Option<PathBuf>,
    /// `CODEX_HOME` ou `~\.codex`.
    pub codex_home: Option<PathBuf>,
    /// `%APPDATA%\codenotch`.
    pub codenotch_dir: Option<PathBuf>,
    /// `%LOCALAPPDATA%\Process Monitor\claude-limits.json` (o mesmo no dev e no instalado:
    /// o script da statusline não sabe qual dos dois está rodando).
    pub bridge_file: Option<PathBuf>,
    /// Estado da consulta própria e dos avisos.
    pub state_file: PathBuf,
    /// `PATH`, para achar o `codex.exe`.
    pub path_var: Option<std::ffi::OsString>,
}

impl Paths {
    pub fn detect() -> Self {
        Self::from_env(|k| std::env::var_os(k), known_folder)
    }

    /// `env` e `known` separados para os testes não dependerem desta máquina.
    pub fn from_env(env: impl Fn(&str) -> Option<std::ffi::OsString>, known: impl Fn(KnownFolder) -> Option<PathBuf>) -> Self {
        let var = |k: &str| env(k).filter(|v| !v.to_string_lossy().trim().is_empty()).map(PathBuf::from);
        let home = var("USERPROFILE").or_else(|| known(KnownFolder::Profile));
        let roaming = var("APPDATA").or_else(|| known(KnownFolder::RoamingAppData));
        let local = var("LOCALAPPDATA").or_else(|| known(KnownFolder::LocalAppData));
        // `~\pasta` numa variável: o shell não expande quando ela é definida no Windows.
        let expand = |p: PathBuf| -> Option<PathBuf> {
            let text = p.to_string_lossy().into_owned();
            match text.strip_prefix('~') {
                Some(rest) => Some(home.clone()?.join(rest.trim_start_matches(['\\', '/']))),
                None => Some(p),
            }
        };
        let claude_dir = var("CLAUDE_CONFIG_DIR").and_then(expand).or_else(|| home.as_ref().map(|h| h.join(".claude")));
        let codex_home = var("CODEX_HOME").and_then(expand).or_else(|| home.as_ref().map(|h| h.join(".codex")));
        let state_file = local.clone().unwrap_or_else(std::env::temp_dir).join(crate::platform::APP_NAME).join("usage-state.json");
        Self {
            codenotch_dir: roaming.as_ref().map(|r| r.join("codenotch")),
            bridge_file: local.as_ref().map(|l| l.join("Process Monitor").join("claude-limits.json")),
            home,
            roaming,
            local,
            claude_dir,
            codex_home,
            state_file,
            path_var: env("PATH"),
        }
    }

    pub fn claude_projects(&self) -> Option<PathBuf> {
        self.claude_dir.as_ref().map(|d| d.join("projects"))
    }

    pub fn codex_sessions(&self) -> Option<PathBuf> {
        self.codex_home.as_ref().map(|d| d.join("sessions"))
    }
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

/// Os JSON lidos aqui são pequenos; um arquivo maior que isso não é o esperado.
const MAX_JSON: u64 = 8 * 1024 * 1024;

/// Lê um JSON pequeno, aceitando o BOM que o PowerShell e o Bloco de Notas põem.
/// Ausente, grande demais ou corrompido: `None`, nunca pânico.
pub fn read_json(path: &std::path::Path) -> Option<Value> {
    if std::fs::metadata(path).ok()?.len() > MAX_JSON {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Um instante qualquer de 2027, para as datas passarem pela sanitização.
    const T: i64 = 1_800_000_000;

    #[test]
    fn tempo_em_qualquer_formato() {
        assert_eq!(parse_time(&serde_json::json!(1_800_000_000)), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!(1_800_000_000_123u64)), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!("1800000000")), Some(1_800_000_000));
        assert_eq!(parse_time(&serde_json::json!("2026-10-10T00:00:00Z")), Some(1_791_590_400));
        assert_eq!(parse_time(&serde_json::json!("2026-10-10T00:00:00.5+00:00")), Some(1_791_590_400));
        // Outro fuso: o mesmo instante.
        assert_eq!(parse_time(&serde_json::json!("2026-10-09T21:00:00-03:00")), Some(1_791_590_400));
        assert_eq!(parse_time(&serde_json::json!("2026-10-10T09:00:00+09:00")), Some(1_791_590_400));
        assert_eq!(parse_time(&serde_json::json!(null)), None);
        assert_eq!(parse_time(&serde_json::json!(-5)), None);
        assert_eq!(parse_time(&serde_json::json!("amanhã")), None);
        assert_eq!(parse_time(&serde_json::json!("10/10/2026")), None);
    }

    #[test]
    fn valores_absurdos_nao_estouram() {
        // Datas e durações fora do plausível viram "desconhecido", e nada estoura.
        let w = Window::new(FIVE_HOUR, f32::NAN, Some(i64::MAX), Some(i64::MAX));
        assert_eq!((w.used, w.resets_at, w.duration_secs), (0.0, None, None));
        let w = Window::new(FIVE_HOUR, 1e9, Some(1), Some(-5));
        assert_eq!((w.used, w.resets_at, w.duration_secs), (100.0, None, None));
        assert_eq!(forecast(&w, &[(i64::MIN, 0.0), (i64::MAX, 1.0)], T), Pace::Exhausted);
        let w = Window::new(FIVE_HOUR, 10.0, Some(T + 60), Some(FIVE_HOURS_SECS));
        let _ = forecast(&w, &[(i64::MIN, 0.0), (T, 10.0)], i64::MAX);
        let _ = budget(&w, i64::MIN + 1);
        assert_eq!(share(5, 0), 0.0);
        let t = Tokens { output: u64::MAX, input: u64::MAX, ..Tokens::default() };
        assert_eq!(t.fresh(), u64::MAX);
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
            windows: vec![Window::new(SEVEN_DAY, 20.0, Some(T + 2_000), Some(SEVEN_DAYS_SECS)), Window::new(FIVE_HOUR, 61.5, None, None)],
            source: Source::Api,
            fetched_at: T,
            plan: Some("pro".into()),
        };
        l.sort();
        assert_eq!(l.windows[0].id, FIVE_HOUR);
        assert_eq!(Limits::from_json(&l.to_json()), Some(l.clone()));
        assert_eq!(l.peak(T + 1_500), Some(61.5));
        // A semanal renovou em T + 2000: não conta mais.
        assert_eq!(l.peak(T + 2_500), Some(61.5));
        // JSON de outra versão, sem campos: não é leitura.
        assert_eq!(Limits::from_json(&serde_json::json!({"windows": "x"})), None);
    }

    #[test]
    fn janelas_extras_so_com_uso() {
        let l = |windows: Vec<Window>| Limits { windows, source: Source::Codenotch, fetched_at: T, plan: None };
        let ids = |l: &Limits, now: i64| l.shown(now).map(|w| w.id.clone()).collect::<Vec<_>>();
        let reading = l(vec![
            Window::new(FIVE_HOUR, 0.0, Some(T + 3600), Some(FIVE_HOURS_SECS)),
            Window::new(SEVEN_DAY, 0.0, Some(T + 86_400), Some(SEVEN_DAYS_SECS)),
            // weekly_scoped do Codenotch em 0%: a linha "Semanal (por modelo)" some.
            Window::new(&claude_window_id("weekly_scoped"), 0.0, Some(T + 86_400), Some(SEVEN_DAYS_SECS)),
            // Abaixo do que a tela mostra como "0,1%": também é zero.
            Window::new("seven_day_sonnet", 0.04, Some(T + 86_400), Some(SEVEN_DAYS_SECS)),
            Window::new(&claude_window_id("weekly_opus"), 12.0, Some(T + 2 * 86_400), Some(SEVEN_DAYS_SECS)),
            // Sem dado que preste (NaN vira 0).
            Window::new("seven_day_extra", f32::NAN, None, None),
        ]);
        // As principais ficam mesmo em 0%; das extras, só a que tem uso.
        assert_eq!(ids(&reading, T), [FIVE_HOUR, SEVEN_DAY, "seven_day_opus"]);
        // A do Opus renovou e ainda não veio leitura nova: some também.
        assert_eq!(ids(&reading, T + 3 * 86_400), [FIVE_HOUR, SEVEN_DAY]);
        // Uso pequeno, mas visível: aparece.
        let small = l(vec![Window::new(FIVE_HOUR, 5.0, None, None), Window::new("seven_day_scoped", 0.1, None, None)]);
        assert_eq!(ids(&small, T), [FIVE_HOUR, "seven_day_scoped"]);
        // Sem 5 h nem semanal (Codex grátis, só a mensal): a janela que existe fica.
        let monthly = l(vec![Window::new("monthly", 0.0, Some(T + 86_400), Some(30 * 86_400))]);
        assert_eq!(ids(&monthly, T), ["monthly"]);
    }

    #[test]
    fn previsao_pelo_ritmo() {
        let now = T;
        // 50% em 2 h de uma janela de 5 h: acaba em mais 2 h, antes de renovar (3 h).
        let w = Window::new(FIVE_HOUR, 50.0, Some(now + 3 * 3600), Some(FIVE_HOURS_SECS));
        assert_eq!(forecast(&w, &[], now), Pace::RunsOutAt(now + 2 * 3600));
        // Ritmo recente mais lento (5 pontos em 30 min): não acaba antes de renovar.
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

    #[test]
    fn orcamento_por_dia_e_por_hora() {
        // Semanal com 40% usados e 3 dias até renovar: 20% por dia.
        let mut w = Window::new(SEVEN_DAY, 40.0, Some(T + 3 * 86_400), Some(SEVEN_DAYS_SECS));
        w.rate = Some(30.0 / 86_400.0);
        let b = budget(&w, T).unwrap();
        assert_eq!(b.unit, BudgetUnit::Day);
        assert!((b.per_unit - 20.0).abs() < 1e-4);
        assert!(!b.last_stretch);
        assert!((b.current.unwrap() - 30.0).abs() < 1e-3);
        // Janela de 5 h com 70% usados e 2 h até renovar: 15% por hora.
        let w = Window::new(FIVE_HOUR, 70.0, Some(T + 2 * 3600), Some(FIVE_HOURS_SECS));
        let b = budget(&w, T).unwrap();
        assert_eq!((b.unit, b.current), (BudgetUnit::Hour, None));
        assert!((b.per_unit - 15.0).abs() < 1e-4);
        // Menos de uma hora: é tudo o que resta.
        let b = budget(&Window::new(FIVE_HOUR, 70.0, Some(T + 600), Some(FIVE_HOURS_SECS)), T).unwrap();
        assert!(b.last_stretch && (b.per_unit - 30.0).abs() < 1e-4);
        // Esgotada: zero. Sem hora de renovar, ou já renovou: sem orçamento.
        assert_eq!(budget(&Window::new(SEVEN_DAY, 100.0, Some(T + 86_400 * 2), Some(SEVEN_DAYS_SECS)), T).unwrap().per_unit, 0.0);
        assert!(budget(&Window::new(SEVEN_DAY, 10.0, None, None), T).is_none());
        assert!(budget(&Window::new(SEVEN_DAY, 10.0, Some(T - 1), None), T).is_none());
        // Semanal sem duração conhecida, mas com dias pela frente: por dia.
        assert_eq!(budget(&Window::new("monthly", 10.0, Some(T + 20 * 86_400), None), T).unwrap().unit, BudgetUnit::Day);
    }

    #[test]
    fn familia_do_modelo() {
        assert_eq!(model_family("claude-opus-5-5"), "Opus");
        assert_eq!(model_family("claude-sonnet-4-5-20250929"), "Sonnet");
        assert_eq!(model_family("claude-3-5-haiku-20241022"), "Haiku");
        assert_eq!(model_family("claude-fable-5-1"), "Fable");
        assert_eq!(model_family("gpt-5-codex"), "gpt-5-codex");
        assert_eq!(model_family(""), "Desconhecido");
        assert_eq!(model_family("?"), "Desconhecido");
    }

    fn rec(ts: i64, project: &str, session: &str, model: &str, kind: Kind, output: u64) -> Record {
        Record {
            ts,
            project: project.into(),
            session: session.into(),
            model: model.into(),
            kind,
            tokens: Tokens { requests: 1, output, cache_read: 1000, ..Tokens::default() },
        }
    }

    #[test]
    fn ranking_modelos_tipos_e_sessoes() {
        let records = [
            rec(100, r"C:\Projetos\Loja Online", "s1", "claude-opus-5-5", Kind::Interactive, 10),
            rec(200, r"C:\Projetos\Loja Online", "s1", "claude-sonnet-4-5", Kind::Interactive, 50),
            rec(300, r"C:\Projetos\Loja Online", "s2", "claude-opus-5-5", Kind::Subagent, 5),
            rec(400, "/home/dev/ação/", "s3", "claude-opus-5-5", Kind::Automation, 100),
            rec(50, r"C:\Projetos\antigo", "s4", "claude-opus-5-5", Kind::Interactive, 999),
        ];
        let c = aggregate(records.iter(), 100, false);
        assert_eq!(c.total.requests, 4);
        assert_eq!(c.total.output, 165);
        assert_eq!(c.projects.len(), 2);
        assert_eq!(c.projects[0].name(), "ação");
        assert_eq!(c.projects[1].name(), "Loja Online");
        let a = &c.projects[1];
        assert_eq!(a.sessions[0].id, "s1");
        assert_eq!((a.sessions[0].first, a.sessions[0].last), (100, 200));
        assert_eq!(a.sessions[0].model, "claude-sonnet-4-5");
        // Peso dos modelos, por família, somando 100%.
        assert_eq!(c.models.iter().map(|(m, t)| (m.as_str(), t.output)).collect::<Vec<_>>(), [("Opus", 115), ("Sonnet", 50)]);
        let sum: f32 = c.models.iter().map(|(_, t)| share(t.fresh(), c.total.fresh())).sum();
        assert!((sum - 100.0).abs() < 0.01);
        // Os três tipos, sempre na mesma ordem.
        assert_eq!(c.kinds.iter().map(|(k, t)| (*k, t.output)).collect::<Vec<_>>(), [
            (Kind::Interactive, 60),
            (Kind::Automation, 100),
            (Kind::Subagent, 5)
        ]);
        // As sessões mais pesadas de todos os projetos.
        assert_eq!(c.top.iter().map(|t| t.session.id.as_str()).collect::<Vec<_>>(), ["s3", "s1", "s2"]);
        assert_eq!(c.top[0].project, "/home/dev/ação/");
        // Período vazio: tudo zerado, sem dividir por zero.
        let empty = aggregate(records.iter(), 10_000, false);
        assert!(empty.projects.is_empty() && empty.top.is_empty() && empty.models.is_empty());
        assert_eq!(empty.kinds.len(), 3);
    }

    #[test]
    fn caminhos_de_qualquer_maquina() {
        use std::ffi::OsString;
        let env = |vars: &'static [(&'static str, &'static str)]| move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(*v));
        // Usuário com espaço, acento e OneDrive.
        let p = Paths::from_env(
            env(&[
                ("USERPROFILE", r"C:\Users\José da Silva"),
                ("APPDATA", r"C:\Users\José da Silva\AppData\Roaming"),
                ("LOCALAPPDATA", r"C:\Users\José da Silva\AppData\Local"),
            ]),
            |_| None,
        );
        assert_eq!(p.claude_dir, Some(PathBuf::from(r"C:\Users\José da Silva\.claude")));
        assert_eq!(p.codex_home, Some(PathBuf::from(r"C:\Users\José da Silva\.codex")));
        assert_eq!(p.codenotch_dir, Some(PathBuf::from(r"C:\Users\José da Silva\AppData\Roaming\codenotch")));
        assert_eq!(p.bridge_file, Some(PathBuf::from(r"C:\Users\José da Silva\AppData\Local\Process Monitor\claude-limits.json")));
        assert_eq!(p.claude_projects(), Some(PathBuf::from(r"C:\Users\José da Silva\.claude\projects")));
        assert!(p.state_file.starts_with(r"C:\Users\José da Silva\AppData\Local"));
        // CLAUDE_CONFIG_DIR e CODEX_HOME mandam, inclusive com `~`; vazio é ignorado.
        let p = Paths::from_env(
            env(&[
                ("USERPROFILE", r"D:\Perfis\ana"),
                ("CLAUDE_CONFIG_DIR", r"~\.claude-trabalho"),
                ("CODEX_HOME", r"E:\OneDrive - Empresa\codex"),
            ]),
            |_| None,
        );
        assert_eq!(p.claude_dir, Some(PathBuf::from(r"D:\Perfis\ana\.claude-trabalho")));
        assert_eq!(p.codex_home, Some(PathBuf::from(r"E:\OneDrive - Empresa\codex")));
        let p = Paths::from_env(env(&[("USERPROFILE", r"D:\Perfis\ana"), ("CLAUDE_CONFIG_DIR", "  ")]), |_| None);
        assert_eq!(p.claude_dir, Some(PathBuf::from(r"D:\Perfis\ana\.claude")));
        // Sem nenhuma variável: as pastas conhecidas do Windows.
        let p = Paths::from_env(
            |_| None,
            |f| match f {
                KnownFolder::Profile => Some(PathBuf::from(r"C:\Users\svc")),
                KnownFolder::RoamingAppData => Some(PathBuf::from(r"C:\Users\svc\AppData\Roaming")),
                KnownFolder::LocalAppData => None,
            },
        );
        assert_eq!(p.claude_dir, Some(PathBuf::from(r"C:\Users\svc\.claude")));
        assert_eq!(p.bridge_file, None);
        // Nada de nada: tudo ausente, sem pânico.
        let p = Paths::from_env(|_| None, |_| None);
        assert!(p.claude_dir.is_none() && p.codex_home.is_none() && p.codenotch_dir.is_none());
    }

    /// As pastas conhecidas desta máquina existem (sem administrador).
    #[test]
    fn pastas_conhecidas_do_windows() {
        for f in [KnownFolder::Profile, KnownFolder::RoamingAppData, KnownFolder::LocalAppData] {
            assert!(known_folder(f).is_some_and(|p| p.is_dir()), "{f:?}");
        }
    }

    #[test]
    fn json_grande_demais_ou_ausente() {
        let dir = std::env::temp_dir().join(format!("pm-ia json ç-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("grande.json");
        std::fs::write(&big, vec![b' '; (MAX_JSON + 1) as usize]).unwrap();
        assert!(read_json(&big).is_none());
        assert!(read_json(&dir.join("não existe.json")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
