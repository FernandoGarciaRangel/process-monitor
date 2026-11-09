//! Quando a consulta própria pode ir à rede. Regra pura, sem relógio nem chamada de
//! sistema; o estado é gravado em disco para valer depois de reabrir o app.
//!
//! - Piso de 5 min entre tentativas, contando as que falharam e o botão "Atualizar".
//!   Exceção: uma janela cujo `resets_at` já passou libera uma leitura (a % zerou).
//! - 429: espera `60 s × 2ⁿ`, até 15 min; o `Retry-After` só aumenta a espera, mesmo
//!   além do teto, porque tentar antes só rende outro 429.
//!
//! Os números são os do Codenotch (github.com/vinzdg/codenotch, MIT), que mediu o que o
//! servidor tolera.

use serde_json::Value;

pub const MIN_INTERVAL_SECS: i64 = 5 * 60;
const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 15 * 60;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    pub last_attempt: i64,
    pub backoff_until: i64,
    pub consecutive_429: u32,
}

impl Gate {
    /// `resets`: os `resets_at` da última leitura boa.
    pub fn may_fetch(&self, now: i64, resets: &[i64]) -> bool {
        if self.backoff_until > now {
            return false;
        }
        self.last_attempt == 0
            || now - self.last_attempt >= MIN_INTERVAL_SECS
            || resets.iter().any(|r| *r > self.last_attempt && *r <= now)
    }

    pub fn attempt(&mut self, now: i64) {
        self.last_attempt = now;
    }

    pub fn succeeded(&mut self) {
        self.consecutive_429 = 0;
        self.backoff_until = 0;
    }

    /// Devolve a espera aplicada, em segundos.
    pub fn rate_limited(&mut self, now: i64, retry_after: Option<i64>) -> i64 {
        let exp = BACKOFF_BASE_SECS.saturating_mul(1 << self.consecutive_429.min(4)).clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS);
        let wait = exp.max(retry_after.unwrap_or(0));
        self.consecutive_429 = self.consecutive_429.saturating_add(1);
        self.backoff_until = now + wait;
        wait
    }

    pub fn to_json(self) -> Value {
        serde_json::json!({
            "last_attempt": self.last_attempt, "backoff_until": self.backoff_until, "consecutive_429": self.consecutive_429,
        })
    }

    pub fn from_json(v: &Value) -> Self {
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        Self { last_attempt: n("last_attempt"), backoff_until: n("backoff_until"), consecutive_429: n("consecutive_429") as u32 }
    }
}

/// `Retry-After` em segundos ou como data HTTP.
pub fn parse_retry_after(header: &str, now: i64) -> Option<i64> {
    let h = header.trim();
    h.parse::<i64>().ok().filter(|s| *s >= 0).or_else(|| {
        chrono::DateTime::parse_from_rfc2822(h).ok().map(|d| (d.timestamp() - now).max(0))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_000_000;

    #[test]
    fn piso_de_cinco_minutos() {
        let mut g = Gate::default();
        assert!(g.may_fetch(T, &[]));
        g.attempt(T);
        assert!(!g.may_fetch(T + 60, &[]));
        assert!(!g.may_fetch(T + MIN_INTERVAL_SECS - 1, &[]));
        assert!(g.may_fetch(T + MIN_INTERVAL_SECS, &[]));
        // A janela renovou: uma leitura fora do piso, uma vez só.
        assert!(g.may_fetch(T + 60, &[T + 30]));
        g.attempt(T + 60);
        assert!(!g.may_fetch(T + 90, &[T + 30]));
    }

    #[test]
    fn recuo_exponencial_com_retry_after() {
        let mut g = Gate::default();
        assert_eq!(g.rate_limited(T, None), 60);
        assert_eq!(g.rate_limited(T, None), 120);
        assert_eq!(g.rate_limited(T, None), 240);
        assert_eq!(g.rate_limited(T, None), 480);
        assert_eq!(g.rate_limited(T, None), 900);
        assert_eq!(g.rate_limited(T, None), 900);
        // O servidor pede mais: vale, mesmo além do teto.
        assert_eq!(g.rate_limited(T, Some(3600)), 3600);
        assert!(!g.may_fetch(T + 3599, &[T + 10]));
        assert!(g.may_fetch(T + 3600, &[]));
        g.succeeded();
        assert_eq!(g.rate_limited(T, Some(5)), 60);
    }

    #[test]
    fn estado_gravado_volta_igual() {
        let g = Gate { last_attempt: 5, backoff_until: 10, consecutive_429: 2 };
        assert_eq!(Gate::from_json(&g.to_json()), g);
        assert_eq!(Gate::from_json(&serde_json::json!({})), Gate::default());
    }

    #[test]
    fn retry_after_em_segundos_ou_data() {
        assert_eq!(parse_retry_after(" 3600 ", T), Some(3600));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", 1_445_412_400), Some(80));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", 2_000_000_000), Some(0));
        assert_eq!(parse_retry_after("logo", T), None);
    }
}
