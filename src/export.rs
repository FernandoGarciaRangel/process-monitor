//! Histórico de um processo em CSV, para investigar fora do app (vazamento de
//! memória, picos de CPU). No formato do Excel em português: `;` entre colunas,
//! vírgula decimal e BOM, para os acentos do cabeçalho saírem certos.

use std::path::PathBuf;

use chrono::{DateTime, Local, TimeDelta};

use crate::sampler::Series;

/// Uma linha por coleta, da mais antiga à mais recente. O horário de cada ponto é
/// reconstruído a partir de `now` e do intervalo de coleta.
pub fn history_csv(cpu: &Series, memory: &Series, interval_ms: u64, now: DateTime<Local>) -> String {
    let mut out = String::from("\u{FEFF}Horário;CPU (%);Memória (MB)\r\n");
    let n = cpu.values().len().min(memory.values().len());
    let skip_cpu = cpu.values().len() - n;
    let skip_mem = memory.values().len() - n;
    for (i, (c, m)) in cpu.values().skip(skip_cpu).zip(memory.values().skip(skip_mem)).enumerate() {
        let back = TimeDelta::milliseconds(((n - 1 - i) as u64 * interval_ms) as i64);
        let at = now - back;
        let mb = m as f64 / (1024.0 * 1024.0);
        out.push_str(&format!(
            "{};{};{}\r\n",
            at.format("%d/%m/%Y %H:%M:%S"),
            format!("{c:.1}").replace('.', ","),
            format!("{mb:.1}").replace('.', ",")
        ));
    }
    out
}

/// Grava na pasta Downloads com um nome que não sobrescreve outro export.
pub fn save(name: &str, pid: u32, csv: &str) -> Result<PathBuf, String> {
    let dir = crate::platform::downloads_dir().ok_or("pasta Downloads não encontrada")?;
    let stem: String =
        name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let file = dir.join(format!("{stem}-{pid}-{}.csv", Local::now().format("%Y%m%d-%H%M%S")));
    std::fs::write(&file, csv).map_err(|e| e.to_string())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn csv_no_formato_do_excel() {
        let mut cpu = Series::with_capacity(10);
        let mut memory = Series::with_capacity(10);
        for (c, m) in [(1.25f32, 100.0 * 1024.0 * 1024.0), (50.0, 150.5 * 1024.0 * 1024.0)] {
            cpu.push(c);
            memory.push(m);
        }
        let now = Local.with_ymd_and_hms(2026, 10, 13, 14, 30, 5).unwrap();
        let csv = history_csv(&cpu, &memory, 1000, now);
        let lines: Vec<&str> = csv.split("\r\n").collect();
        assert_eq!(lines[0], "\u{FEFF}Horário;CPU (%);Memória (MB)");
        assert_eq!(lines[1], "13/10/2026 14:30:04;1,2;100,0");
        assert_eq!(lines[2], "13/10/2026 14:30:05;50,0;150,5");
    }
}
