//! Formatação de números para a interface, no padrão pt-BR (vírgula decimal).

const KIB: f64 = 1024.0;

/// Bytes em unidade binária legível: `512 B`, `12,4 MB`, `1,95 GB`.
pub fn bytes(value: u64) -> String {
    let v = value as f64;
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut idx = 0;
    let mut scaled = v;
    while scaled >= KIB && idx < units.len() - 1 {
        scaled /= KIB;
        idx += 1;
    }
    if idx == 0 {
        format!("{value} B")
    } else {
        format!("{} {}", decimal(scaled, precision_for(scaled)), units[idx])
    }
}

/// Taxa por segundo: `0 B/s`, `3,2 MB/s`.
pub fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0).round() as u64))
}

/// Percentual com uma casa: `7,3%`. Abaixo de 0,05 vira `0%` para não poluir a tabela.
pub fn percent(value: f32) -> String {
    if value < 0.05 {
        "0%".to_owned()
    } else {
        format!("{}%", decimal(value as f64, 1))
    }
}

/// Duração compacta: `42 s`, `5 min 03 s`, `3 h 07 min`, `2 d 4 h`.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// Data e hora local a partir de segundos desde a época Unix: `08/10/2026 14:32:05`.
pub fn datetime(unix_secs: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs as i64, 0).single() {
        Some(dt) => dt.format("%d/%m/%Y %H:%M:%S").to_string(),
        None => "—".to_owned(),
    }
}

/// Frequência em GHz a partir de MHz: `3,40 GHz`.
pub fn ghz(mhz: u64) -> String {
    format!("{} GHz", decimal(mhz as f64 / 1000.0, 2))
}

fn precision_for(v: f64) -> usize {
    if v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    }
}

fn decimal(v: f64, places: usize) -> String {
    format!("{v:.places$}").replace('.', ",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_escala_e_virgula() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1,50 KB");
        assert_eq!(bytes(12 * 1024 * 1024 + 400 * 1024), "12,4 MB");
        assert_eq!(bytes(300 * 1024 * 1024), "300 MB");
        assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2,00 GB");
    }

    #[test]
    fn rate_nunca_negativa() {
        assert_eq!(rate(-5.0), "0 B/s");
        assert_eq!(rate(2048.0), "2,00 KB/s");
    }

    #[test]
    fn percent_corta_ruido() {
        assert_eq!(percent(0.01), "0%");
        assert_eq!(percent(7.34), "7,3%");
        assert_eq!(percent(100.0), "100,0%");
    }

    #[test]
    fn duration_escolhe_duas_unidades() {
        assert_eq!(duration(42), "42 s");
        assert_eq!(duration(5 * 60 + 3), "5 min 03 s");
        assert_eq!(duration(3 * 3600 + 7 * 60 + 9), "3 h 07 min");
        assert_eq!(duration(2 * 86_400 + 4 * 3600), "2 d 4 h");
    }

    #[test]
    fn ghz_duas_casas() {
        assert_eq!(ghz(3400), "3,40 GHz");
    }
}
