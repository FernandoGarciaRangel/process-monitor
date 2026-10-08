//! Uso de GPU pelos contadores de desempenho do Windows (PDH).
//!
//! O sysinfo publicado ainda não expõe GPU. O Gerenciador de Tarefas lê os mesmos
//! contadores: `GPU Engine` traz a utilização por motor e por processo, e
//! `GPU Adapter Memory` traz a VRAM dedicada em uso. O nome e a VRAM total vêm do DXGI.
//!
//! As instâncias de `GPU Engine` têm o formato
//! `pid_1234_luid_0x00000000_0x0000D1A5_phys_0_eng_3_engtype_3D`. A utilização de uma
//! GPU é a do motor mais ocupado (soma de todos os processos naquele motor), e a de um
//! processo é a do motor em que ele mais pesa. É a mesma regra do Gerenciador de Tarefas.

use std::collections::HashMap;

use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{PCWSTR, w};

/// Um adaptador físico, identificado pelo LUID no formato dos nomes de instância do PDH.
#[derive(Clone, Debug)]
pub struct Adapter {
    pub name: String,
    pub luid: String,
    pub vram_total: u64,
}

#[derive(Clone, Debug, Default)]
pub struct GpuSample {
    /// Utilização 0–100 por adaptador, na mesma ordem de [`GpuMonitor::adapters`].
    pub utilization: Vec<f32>,
    /// VRAM dedicada em uso por adaptador, em bytes.
    pub vram_used: Vec<u64>,
    /// Utilização 0–100 por PID (maior motor do processo, somando todas as GPUs).
    pub per_process: HashMap<u32, f32>,
}

pub struct GpuMonitor {
    query: PDH_HQUERY,
    engine: PDH_HCOUNTER,
    memory: PDH_HCOUNTER,
    adapters: Vec<Adapter>,
    buffer: Vec<u64>,
}

// O handle de consulta do PDH é usado só pela thread de coleta, que é dona do monitor.
unsafe impl Send for GpuMonitor {}

impl GpuMonitor {
    /// `None` quando a máquina não tem GPU de hardware ou o PDH não tem os contadores.
    pub fn new() -> Option<Self> {
        let adapters = enumerate_adapters();
        if adapters.is_empty() {
            return None;
        }
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut engine = PDH_HCOUNTER::default();
            let mut memory = PDH_HCOUNTER::default();
            let ok_engine = PdhAddEnglishCounterW(
                query,
                w!(r"\GPU Engine(*)\Utilization Percentage"),
                0,
                &mut engine,
            ) == 0;
            let ok_memory = PdhAddEnglishCounterW(
                query,
                w!(r"\GPU Adapter Memory(*)\Dedicated Usage"),
                0,
                &mut memory,
            ) == 0;
            if !ok_engine || !ok_memory {
                PdhCloseQuery(query);
                return None;
            }
            // Contador de taxa: a primeira coleta só estabelece a base.
            PdhCollectQueryData(query);
            Some(Self { query, engine, memory, adapters, buffer: Vec::new() })
        }
    }

    pub fn adapters(&self) -> &[Adapter] {
        &self.adapters
    }

    pub fn sample(&mut self) -> GpuSample {
        let n = self.adapters.len();
        let mut out = GpuSample {
            utilization: vec![0.0; n],
            vram_used: vec![0; n],
            per_process: HashMap::new(),
        };
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return out;
        }

        // Soma por motor (todas as instâncias de processo) e por (pid, motor).
        let mut per_engine: HashMap<String, f64> = HashMap::new();
        let mut per_pid_engine: HashMap<(u32, String), f64> = HashMap::new();
        let engine = self.engine;
        read_array(engine, &mut self.buffer, |name, value| {
            let Some((pid, engine_key)) = parse_engine_instance(name) else {
                return;
            };
            *per_engine.entry(engine_key.clone()).or_default() += value;
            *per_pid_engine.entry((pid, engine_key)).or_default() += value;
        });

        for (key, value) in &per_engine {
            if let Some(i) = self.adapter_index(key) {
                out.utilization[i] = out.utilization[i].max(value.min(100.0) as f32);
            }
        }
        for ((pid, _), value) in per_pid_engine {
            let slot = out.per_process.entry(pid).or_default();
            *slot = slot.max(value.min(100.0) as f32);
        }

        let memory = self.memory;
        let mut vram = vec![0u64; n];
        read_array(memory, &mut self.buffer, |name, value| {
            if let Some(i) = self.adapters.iter().position(|a| contains_ci(name, &a.luid)) {
                vram[i] = vram[i].max(value.max(0.0) as u64);
            }
        });
        out.vram_used = vram;
        out
    }

    fn adapter_index(&self, engine_key: &str) -> Option<usize> {
        self.adapters.iter().position(|a| contains_ci(engine_key, &a.luid))
    }
}

impl Drop for GpuMonitor {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// Lê todas as instâncias de um contador curinga e entrega `(nome, valor)` a `f`.
fn read_array(counter: PDH_HCOUNTER, buffer: &mut Vec<u64>, mut f: impl FnMut(&str, f64)) {
    unsafe {
        let mut size_bytes = 0u32;
        let mut count = 0u32;
        let status = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size_bytes, &mut count, None);
        if status != PDH_MORE_DATA || size_bytes == 0 {
            return;
        }
        // Buffer em u64 para respeitar o alinhamento da struct (contém f64 e ponteiros).
        let words = (size_bytes as usize).div_ceil(8);
        buffer.clear();
        buffer.resize(words, 0);
        let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let status = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size_bytes, &mut count, Some(items));
        if status != 0 {
            return;
        }
        for item in std::slice::from_raw_parts(items, count as usize) {
            let cstatus = item.FmtValue.CStatus;
            if cstatus != PDH_CSTATUS_VALID_DATA && cstatus != PDH_CSTATUS_NEW_DATA {
                continue;
            }
            let Ok(name) = item.szName.to_string() else { continue };
            f(&name, item.FmtValue.Anonymous.doubleValue);
        }
    }
}

/// `pid_1234_luid_0x0_0xD1A5_phys_0_eng_3_engtype_3D` → `(1234, "luid_0x0_0xD1A5_phys_0_eng_3")`.
fn parse_engine_instance(name: &str) -> Option<(u32, String)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let pid = pid.parse().ok()?;
    let engine = match rest.find("_engtype_") {
        Some(i) => &rest[..i],
        None => rest,
    };
    Some((pid, engine.to_owned()))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

fn enumerate_adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]).trim().to_owned();
            let luid = format!(
                "luid_0x{:08x}_0x{:08x}",
                desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
            );
            // O mesmo adaptador pode aparecer duas vezes (uma por saída de vídeo).
            if out.iter().any(|a: &Adapter| a.luid == luid) {
                continue;
            }
            out.push(Adapter { name, luid, vram_total: desc.DedicatedVideoMemory as u64 });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instancia_de_motor() {
        let (pid, key) =
            parse_engine_instance("pid_1234_luid_0x00000000_0x0000D1A5_phys_0_eng_3_engtype_VideoDecode").unwrap();
        assert_eq!(pid, 1234);
        assert_eq!(key, "luid_0x00000000_0x0000D1A5_phys_0_eng_3");
        assert!(contains_ci(&key, "luid_0x00000000_0x0000d1a5"));
        assert!(parse_engine_instance("_Total").is_none());
    }
}
