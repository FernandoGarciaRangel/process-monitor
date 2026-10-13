//! Regras por programa: prioridade, núcleos e modo eficiência aplicados sozinhos toda
//! vez que o programa abre (o Teams sempre em modo eficiência, o cargo sempre em
//! prioridade baixa). Quem aplica é a thread da coleta, que continua viva com a janela
//! na bandeja; aqui fica a regra, a escolha de quem recebe e o formato salvo.

use std::collections::HashSet;

use crate::control::{self, Priority};
use crate::sampler::{ProcInfo, ProcKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Nome do executável, em minúsculas (`teams.exe`): a mesma identidade dos grupos.
    pub exe: String,
    pub priority: Option<Priority>,
    /// Máscara de núcleos lógicos.
    pub affinity: Option<u64>,
    pub efficiency: bool,
}

impl Rule {
    pub fn matches(&self, p: &ProcInfo) -> bool {
        self.exe.eq_ignore_ascii_case(&p.name)
    }

    /// "Modo eficiência · núcleos 0–3", para a lista nas configurações.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.efficiency {
            parts.push("modo eficiência".to_owned());
        }
        if let Some(p) = self.priority {
            parts.push(format!("prioridade {}", p.label().to_lowercase()));
        }
        if let Some(mask) = self.affinity {
            parts.push(format!("{} núcleos", control::core_count(mask)));
        }
        let text = parts.join(" · ");
        let mut chars = text.chars();
        chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
    }

    /// Nada a aplicar: a regra não teria efeito.
    pub fn is_empty(&self) -> bool {
        self.priority.is_none() && self.affinity.is_none() && !self.efficiency
    }
}

/// Estado da aplicação: quem já recebeu a regra (uma vez por processo, mesmo se
/// falhou, para não insistir a cada coleta num processo sem acesso).
#[derive(Default)]
pub struct Applier {
    rules: Vec<Rule>,
    done: HashSet<ProcKey>,
}

impl Applier {
    /// Regras novas valem também para o que já está rodando.
    pub fn set_rules(&mut self, rules: Vec<Rule>) {
        self.rules = rules;
        self.done.clear();
    }

    /// Processos que ainda não receberam a regra que casa com eles. `own_pid` fica de fora.
    pub fn pending<'a>(&mut self, processes: &'a [ProcInfo], own_pid: u32) -> Vec<(&'a ProcInfo, Rule)> {
        let alive: HashSet<ProcKey> = processes.iter().map(|p| p.key()).collect();
        self.done.retain(|k| alive.contains(k));
        if self.rules.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for p in processes.iter().filter(|p| p.pid != own_pid) {
            if let Some(rule) = self.rules.iter().find(|r| r.matches(p))
                && self.done.insert(p.key())
            {
                out.push((p, rule.clone()));
            }
        }
        out
    }

    /// Aplica de verdade. O modo eficiência vem antes da prioridade: ele baixa a
    /// prioridade, e uma prioridade escolhida na regra tem que prevalecer.
    pub fn apply(&mut self, processes: &[ProcInfo], own_pid: u32) {
        for (p, rule) in self.pending(processes, own_pid) {
            let key = p.key();
            if rule.efficiency {
                let _ = control::set_efficiency(key, &p.name, true);
            }
            if let Some(priority) = rule.priority {
                let _ = control::set_priority(key, &p.name, priority);
            }
            if let Some(mask) = rule.affinity {
                let _ = control::set_affinity(key, &p.name, mask);
            }
        }
    }
}

/// Formato salvo: `teams.exe|-|-|1;cargo.exe|0|f|0` (exe, prioridade, máscara em hexa,
/// eficiência). Regra ilegível é descartada.
pub fn to_text(rules: &[Rule]) -> String {
    rules
        .iter()
        .map(|r| {
            let priority = r.priority.and_then(|p| Priority::ALL.iter().position(|x| *x == p)).map_or("-".to_owned(), |i| i.to_string());
            let affinity = r.affinity.map_or("-".to_owned(), |m| format!("{m:x}"));
            format!("{}|{priority}|{affinity}|{}", r.exe, u8::from(r.efficiency))
        })
        .collect::<Vec<_>>()
        .join(";")
}

pub fn parse(text: &str) -> Vec<Rule> {
    text.split(';')
        .filter_map(|item| {
            let mut f = item.split('|');
            let exe = f.next()?.trim().to_lowercase();
            let priority = match f.next()? {
                "-" => None,
                i => Some(*Priority::ALL.get(i.parse::<usize>().ok()?)?),
            };
            let affinity = match f.next()? {
                "-" => None,
                m => Some(u64::from_str_radix(m, 16).ok()?).filter(|m| *m != 0),
            };
            let efficiency = f.next()? == "1";
            let rule = Rule { exe, priority, affinity, efficiency };
            (!rule.exe.is_empty() && !rule.is_empty()).then_some(rule)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::tests::proc;

    fn rule(exe: &str) -> Rule {
        Rule { exe: exe.to_owned(), priority: Some(Priority::BelowNormal), affinity: Some(0xF), efficiency: true }
    }

    #[test]
    fn salva_e_le_igual() {
        let rules = vec![rule("teams.exe"), Rule { exe: "cargo.exe".into(), priority: None, affinity: None, efficiency: true }];
        assert_eq!(parse(&to_text(&rules)), rules);
        assert!(parse("").is_empty());
        assert!(parse("x.exe|9|-|0;y.exe|-|zz|1;vazio.exe|-|-|0").is_empty(), "ilegível ou sem efeito");
    }

    #[test]
    fn aplica_uma_vez_por_processo() {
        let mut applier = Applier::default();
        applier.set_rules(vec![rule("teams.exe")]);
        let procs = vec![proc(10, None, 1, "Teams.exe"), proc(11, None, 1, "code.exe"), proc(12, None, 1, "teams.exe")];
        let first: Vec<u32> = applier.pending(&procs, 99).iter().map(|(p, _)| p.pid).collect();
        assert_eq!(first, vec![10, 12]);
        assert!(applier.pending(&procs, 99).is_empty(), "segunda coleta não repete");
        // Processo novo do mesmo programa recebe; o próprio app nunca.
        let procs = vec![proc(10, None, 1, "Teams.exe"), proc(13, None, 2, "teams.exe")];
        assert_eq!(applier.pending(&procs, 13).len(), 0);
        let procs = vec![proc(14, None, 3, "teams.exe")];
        assert_eq!(applier.pending(&procs, 99).len(), 1);
        // Trocar as regras reaplica no que já roda.
        applier.set_rules(vec![rule("teams.exe")]);
        assert_eq!(applier.pending(&procs, 99).len(), 1);
    }

    #[test]
    fn resumo() {
        assert_eq!(rule("a").summary(), "Modo eficiência · prioridade abaixo do normal · 4 núcleos");
    }
}
