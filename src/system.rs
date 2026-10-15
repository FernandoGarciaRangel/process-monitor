//! Configuração da máquina: o que não muda enquanto o app está aberto (modelo,
//! placa-mãe, BIOS, memória instalada, vídeo, monitores e versão do Windows).
//!
//! Tudo sai do registro, do SMBIOS e do GDI, sem WMI: a primeira consulta a
//! `Win32_ComputerSystem` custa centenas de milissegundos e exigiria COM na thread
//! da interface. A leitura acontece uma vez, na primeira visita à tela.

use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_MIRRORING_DRIVER, DISPLAY_DEVICE_PRIMARY_DEVICE,
    DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{
    GetNativeSystemInfo, GetSystemFirmwareTable, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM,
    PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_INTEL, RSMB, SYSTEM_INFO,
};
use windows::core::{HSTRING, PCWSTR};

const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
const CPU_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
const WINDOWS_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
/// Classe dos adaptadores de vídeo: onde o driver de cada GPU se registra.
const DISPLAY_CLASS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// Um módulo de memória instalado, ou um slot vazio (`size` zero).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryModule {
    /// Onde o módulo está espetado, como a placa-mãe chama: `DIMM A1`.
    pub locator: String,
    pub size: u64,
    /// `DDR4`, `LPDDR5`… vazio quando o firmware não diz.
    pub kind: String,
    /// `DIMM` ou `SODIMM`: a diferença entre desktop e notebook.
    pub form: String,
    /// Velocidade configurada, em MT/s. Costuma ser menor que a do módulo.
    pub speed: u32,
    pub manufacturer: String,
    pub part: String,
}

impl MemoryModule {
    pub fn empty(&self) -> bool {
        self.size == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Display {
    pub name: String,
    /// Adaptador que desenha nele. Como quase todo monitor se chama "Monitor
    /// Genérico PnP", é por aqui que se sabe qual é a tela do notebook.
    pub adapter: String,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub primary: bool,
}

/// Driver de um adaptador de vídeo, como está no registro.
#[derive(Clone, Debug, Default)]
pub struct VideoDriver {
    /// Nome do adaptador segundo o driver; é por ele que casamos com a GPU da coleta.
    pub desc: String,
    pub version: String,
    /// No formato do registro, `10-3-2025` (mês-dia-ano).
    pub date: String,
    pub provider: String,
}

#[derive(Clone, Debug, Default)]
pub struct SystemInfo {
    pub manufacturer: String,
    pub model: String,
    pub family: String,
    pub board: String,
    pub bios: String,
    pub bios_date: String,
    pub arch: String,
    pub user: String,
    pub os_name: String,
    pub os_version: String,
    pub os_build: String,
    pub os_installed: Option<u64>,
    pub cpu_name: String,
    pub cpu_vendor: String,
    pub cpu_identifier: String,
    pub cpu_base_mhz: u64,
    pub memory: Vec<MemoryModule>,
    pub displays: Vec<Display>,
    pub video: Vec<VideoDriver>,
}

impl SystemInfo {
    pub fn load() -> Self {
        let build: u32 = reg_string(WINDOWS_KEY, "CurrentBuild").and_then(|v| v.parse().ok()).unwrap_or(0);
        let ubr = reg_u32(WINDOWS_KEY, "UBR").unwrap_or(0);
        SystemInfo {
            manufacturer: clean(reg_string(BIOS_KEY, "SystemManufacturer")),
            model: clean(reg_string(BIOS_KEY, "SystemProductName")),
            family: clean(reg_string(BIOS_KEY, "SystemFamily")),
            board: join(
                clean(reg_string(BIOS_KEY, "BaseBoardManufacturer")),
                clean(reg_string(BIOS_KEY, "BaseBoardProduct")),
            ),
            bios: join(clean(reg_string(BIOS_KEY, "BIOSVendor")), clean(reg_string(BIOS_KEY, "BIOSVersion"))),
            bios_date: clean(reg_string(BIOS_KEY, "BIOSReleaseDate")),
            arch: architecture().to_owned(),
            user: std::env::var("USERNAME").unwrap_or_default(),
            os_name: windows_name(reg_string(WINDOWS_KEY, "ProductName").unwrap_or_default(), build),
            os_version: reg_string(WINDOWS_KEY, "DisplayVersion").unwrap_or_default(),
            os_build: if ubr > 0 { format!("{build}.{ubr}") } else { build.to_string() },
            os_installed: reg_u32(WINDOWS_KEY, "InstallDate").map(u64::from),
            cpu_name: clean(reg_string(CPU_KEY, "ProcessorNameString")),
            cpu_vendor: clean(reg_string(CPU_KEY, "VendorIdentifier")),
            cpu_identifier: clean(reg_string(CPU_KEY, "Identifier")),
            cpu_base_mhz: u64::from(reg_u32(CPU_KEY, "~MHz").unwrap_or(0)),
            memory: memory_modules(),
            displays: displays(),
            video: video_drivers(),
        }
    }

    /// Fabricante e modelo numa linha só, para o título da tela.
    pub fn machine(&self) -> String {
        let name = join(self.manufacturer.clone(), self.model.clone());
        if name.is_empty() { "Computador".to_owned() } else { name }
    }

    /// Driver do adaptador com este nome, se o registro souber dele.
    pub fn driver_for(&self, adapter: &str) -> Option<&VideoDriver> {
        let wanted = adapter.trim().to_lowercase();
        self.video.iter().find(|v| v.desc.trim().to_lowercase() == wanted)
    }

    /// Memória instalada somando os módulos. Zero quando o firmware não informa: aí
    /// vale o total do sistema, que é um pouco menor (parte fica com o hardware).
    pub fn memory_installed(&self) -> u64 {
        self.memory.iter().map(|m| m.size).sum()
    }

    pub fn slots_used(&self) -> usize {
        self.memory.iter().filter(|m| !m.empty()).count()
    }
}

/// Campos que o fabricante deixou com o texto de exemplo não valem nada na tela.
fn clean(value: Option<String>) -> String {
    let value = value.unwrap_or_default();
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    let placeholder = lower.is_empty()
        || lower.contains("to be filled")
        || lower.contains("system manufacturer")
        || lower.contains("system product")
        || lower.contains("default string")
        || lower == "none"
        || lower == "n/a"
        || lower.chars().all(|c| c == 'o' || c == '0');
    if placeholder { String::new() } else { trimmed.to_owned() }
}

fn join(a: String, b: String) -> String {
    match (a.is_empty(), b.is_empty()) {
        (false, false) => format!("{a} {b}"),
        (false, true) => a,
        (true, false) => b,
        (true, true) => String::new(),
    }
}

/// O `ProductName` do registro continua dizendo "Windows 10" no Windows 11.
fn windows_name(product: String, build: u32) -> String {
    let product = product.trim();
    if build >= 22000 && product.starts_with("Windows 10") {
        return product.replacen("Windows 10", "Windows 11", 1);
    }
    if product.is_empty() { "Windows".to_owned() } else { product.to_owned() }
}

fn architecture() -> &'static str {
    let code = unsafe {
        let mut info = SYSTEM_INFO::default();
        GetNativeSystemInfo(&mut info);
        info.Anonymous.Anonymous.wProcessorArchitecture
    };
    if code == PROCESSOR_ARCHITECTURE_AMD64 {
        "x64 (64 bits)"
    } else if code == PROCESSOR_ARCHITECTURE_ARM64 {
        "ARM64 (64 bits)"
    } else if code == PROCESSOR_ARCHITECTURE_ARM {
        "ARM (32 bits)"
    } else if code == PROCESSOR_ARCHITECTURE_INTEL {
        "x86 (32 bits)"
    } else {
        "desconhecida"
    }
}

fn reg_string(path: &str, value: &str) -> Option<String> {
    let (path, value) = (HSTRING::from(path), HSTRING::from(value));
    let mut len = 0u32;
    reg(&path, &value, RRF_RT_REG_SZ, None, Some(&mut len))?;
    let mut buf = vec![0u16; len as usize / 2 + 1];
    reg(&path, &value, RRF_RT_REG_SZ, Some(buf.as_mut_ptr().cast()), Some(&mut len))?;
    // `len` volta em bytes e inclui o terminador.
    let chars = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]).trim().to_owned())
}

fn reg_u32(path: &str, value: &str) -> Option<u32> {
    let (path, value) = (HSTRING::from(path), HSTRING::from(value));
    let mut data = 0u32;
    let mut len = 4u32;
    reg(&path, &value, RRF_RT_REG_DWORD, Some((&raw mut data).cast()), Some(&mut len))?;
    Some(data)
}

fn reg(
    path: &HSTRING,
    value: &HSTRING,
    flags: REG_ROUTINE_FLAGS,
    data: Option<*mut core::ffi::c_void>,
    len: Option<*mut u32>,
) -> Option<()> {
    unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, path, value, flags, None, data, len).ok().ok() }
}

/// Monitores ligados agora, com a resolução e a taxa em uso.
fn displays() -> Vec<Display> {
    let mut out = Vec::new();
    unsafe {
        for index in 0..16 {
            let mut adapter = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut adapter, 0).as_bool() {
                break;
            }
            let flags = adapter.StateFlags.0;
            if flags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 || flags & DISPLAY_DEVICE_MIRRORING_DRIVER.0 != 0 {
                continue;
            }
            let device = HSTRING::from(wide_to_string(&adapter.DeviceName).as_str());
            let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
            if !EnumDisplaySettingsW(&device, ENUM_CURRENT_SETTINGS, &mut mode).as_bool() {
                continue;
            }
            // O nome bonito ("Monitor Genérico PnP") está no filho do adaptador.
            let mut monitor = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            let name = if EnumDisplayDevicesW(&device, 0, &mut monitor, 0).as_bool() {
                wide_to_string(&monitor.DeviceString)
            } else {
                wide_to_string(&adapter.DeviceString)
            };
            out.push(Display {
                name,
                adapter: wide_to_string(&adapter.DeviceString),
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
                hz: mode.dmDisplayFrequency,
                primary: flags & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0,
            });
        }
    }
    out
}

fn video_drivers() -> Vec<VideoDriver> {
    let mut out = Vec::new();
    // As subchaves são numeradas em sequência, mas uma pode faltar: percorre todas.
    for index in 0..16 {
        let path = format!("{DISPLAY_CLASS_KEY}\\{index:04}");
        let Some(desc) = reg_string(&path, "DriverDesc").filter(|d| !d.is_empty()) else {
            continue;
        };
        out.push(VideoDriver {
            desc,
            version: reg_string(&path, "DriverVersion").unwrap_or_default(),
            date: reg_string(&path, "DriverDate").unwrap_or_default(),
            provider: reg_string(&path, "ProviderName").unwrap_or_default(),
        });
    }
    out
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_owned()
}

/// Memória instalada pelo SMBIOS, a mesma tabela que o Gerenciador de Tarefas lê
/// para mostrar tipo e velocidade. O firmware entrega tudo numa chamada.
fn memory_modules() -> Vec<MemoryModule> {
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, None);
        if size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        let written = GetSystemFirmwareTable(RSMB, 0, Some(&mut buf));
        if written == 0 || written as usize > buf.len() {
            return Vec::new();
        }
        buf.truncate(written as usize);
        // Cabeçalho RawSMBIOSData: 4 bytes de versão + 4 do tamanho da tabela.
        if buf.len() <= 8 {
            return Vec::new();
        }
        parse_memory(&buf[8..])
    }
}

/// Percorre as estruturas SMBIOS e devolve as do tipo 17 (um slot de memória cada).
fn parse_memory(table: &[u8]) -> Vec<MemoryModule> {
    const MEMORY_DEVICE: u8 = 17;
    const END_OF_TABLE: u8 = 127;
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= table.len() {
        let kind = table[at];
        let len = table[at + 1] as usize;
        if len < 4 || at + len > table.len() {
            break;
        }
        let fixed = &table[at..at + len];
        // Depois da área fixa vêm as strings, terminadas por dois zeros seguidos.
        let strings_at = at + len;
        let mut end = strings_at;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        if kind == MEMORY_DEVICE {
            out.push(memory_device(fixed, &table[strings_at..end.min(table.len())]));
        }
        if kind == END_OF_TABLE {
            break;
        }
        at = end + 2;
    }
    out
}

fn memory_device(fixed: &[u8], strings: &[u8]) -> MemoryModule {
    let byte = |at: usize| fixed.get(at).copied().unwrap_or(0);
    let word = |at: usize| match fixed.get(at..at + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    };
    let dword = |at: usize| match fixed.get(at..at + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    };
    // 0x7FFF quer dizer "grande demais para caber aqui": o tamanho real está em 0x1C.
    let size = match word(0x0C) {
        0 | 0xFFFF => 0,
        0x7FFF => u64::from(dword(0x1C) & 0x7FFF_FFFF) * 1024 * 1024,
        // Bit 15 ligado conta em KB; desligado, em MB.
        v if v & 0x8000 != 0 => u64::from(v & 0x7FFF) * 1024,
        v => u64::from(v) * 1024 * 1024,
    };
    // A velocidade configurada (0x20) é a que vale; sem ela, a nominal do módulo.
    let speed = match word(0x20) {
        0 => u32::from(word(0x15)),
        configured => u32::from(configured),
    };
    MemoryModule {
        locator: smbios_string(strings, byte(0x10)),
        size,
        kind: memory_kind(byte(0x12)).to_owned(),
        form: form_factor(byte(0x0E)).to_owned(),
        speed,
        manufacturer: clean(Some(smbios_string(strings, byte(0x17)))),
        part: clean(Some(smbios_string(strings, byte(0x1A)))),
    }
}

/// As strings vêm numeradas a partir de 1, separadas por zero; 0 quer dizer "nenhuma".
fn smbios_string(strings: &[u8], index: u8) -> String {
    if index == 0 {
        return String::new();
    }
    strings
        .split(|&b| b == 0)
        .nth(index as usize - 1)
        .map(|s| String::from_utf8_lossy(s).trim().to_owned())
        .unwrap_or_default()
}

/// Data americana do firmware e do registro (`10/3/2025`, `10-3-2025`) no formato
/// daqui. O que não tiver essa cara volta como veio.
pub fn us_date(text: &str) -> String {
    let parts: Vec<&str> = text.trim().split(['/', '-']).collect();
    let [month, day, year] = parts[..] else {
        return text.trim().to_owned();
    };
    let valid = year.len() == 4
        && [month, day, year].iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if valid {
        format!("{day:0>2}/{month:0>2}/{year}")
    } else {
        text.trim().to_owned()
    }
}

fn memory_kind(code: u8) -> &'static str {
    match code {
        0x12 => "DDR",
        0x13 | 0x14 => "DDR2",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "",
    }
}

fn form_factor(code: u8) -> &'static str {
    match code {
        0x08 => "DIMM",
        0x0D => "SODIMM",
        0x0F => "FB-DIMM",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta uma estrutura SMBIOS do tipo 17 com as strings logo depois.
    fn memory_struct(size_mb: u16, kind: u8, configured: u16, strings: &[&str]) -> Vec<u8> {
        let mut s = vec![0u8; 0x22];
        s[0] = 17;
        s[1] = 0x22;
        s[0x0C..0x0E].copy_from_slice(&size_mb.to_le_bytes());
        s[0x0E] = 0x0D; // SODIMM
        s[0x10] = 1; // locator: primeira string
        s[0x12] = kind;
        s[0x15..0x17].copy_from_slice(&3200u16.to_le_bytes());
        s[0x17] = 2; // fabricante
        s[0x1A] = 3; // part number
        s[0x20..0x22].copy_from_slice(&configured.to_le_bytes());
        for text in strings {
            s.extend_from_slice(text.as_bytes());
            s.push(0);
        }
        s.push(0); // fim da área de strings
        s
    }

    #[test]
    fn le_modulos_e_slots_vazios() {
        let mut table = memory_struct(16 * 1024, 0x1A, 2667, &["DIMM A1", "Kingston", "KF548S38"]);
        table.extend(memory_struct(0, 0x02, 0, &["DIMM B1"]));
        table.extend([127, 4, 0, 0, 0, 0]); // fim da tabela

        let modules = parse_memory(&table);
        assert_eq!(modules.len(), 2);
        let first = &modules[0];
        assert_eq!(first.locator, "DIMM A1");
        assert_eq!(first.size, 16 * 1024 * 1024 * 1024);
        assert_eq!(first.kind, "DDR4");
        assert_eq!(first.form, "SODIMM");
        assert_eq!(first.speed, 2667);
        assert_eq!(first.manufacturer, "Kingston");
        assert_eq!(first.part, "KF548S38");
        assert!(modules[1].empty());
        assert_eq!(modules[1].locator, "DIMM B1");
    }

    #[test]
    fn tamanho_em_kb_e_velocidade_nominal() {
        // Bit 15 ligado: o valor conta em KB. Sem velocidade configurada, vale a do módulo.
        let table = memory_struct(0x8000 | 512, 0x22, 0, &["DIMM 0"]);
        let m = &parse_memory(&table)[0];
        assert_eq!(m.size, 512 * 1024);
        assert_eq!(m.kind, "DDR5");
        assert_eq!(m.speed, 3200);
    }

    #[test]
    fn tabela_truncada_nao_entra_em_laco() {
        assert!(parse_memory(&[]).is_empty());
        assert!(parse_memory(&[17, 0x22, 0, 0]).is_empty());
        assert!(parse_memory(&[17, 2, 0, 0]).is_empty());
    }

    #[test]
    fn windows_11_corrige_o_nome_do_registro() {
        assert_eq!(windows_name("Windows 10 Pro".to_owned(), 26200), "Windows 11 Pro");
        assert_eq!(windows_name("Windows 10 Pro".to_owned(), 19045), "Windows 10 Pro");
        assert_eq!(windows_name("Windows 11 Home".to_owned(), 26200), "Windows 11 Home");
        assert_eq!(windows_name(String::new(), 0), "Windows");
    }

    /// O que esta máquina devolve de verdade, para conferir firmware esquisito:
    /// `cargo test ficha -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ficha_desta_maquina() {
        println!("{:#?}", SystemInfo::load());
    }

    #[test]
    fn data_americana_vira_brasileira() {
        assert_eq!(us_date("10/3/2025"), "03/10/2025");
        assert_eq!(us_date("10-3-2025"), "03/10/2025");
        assert_eq!(us_date("12/31/2024"), "31/12/2024");
        assert_eq!(us_date("2025-03-10"), "2025-03-10");
        assert_eq!(us_date("A17"), "A17");
    }

    #[test]
    fn campo_de_exemplo_do_fabricante_vira_vazio() {
        assert_eq!(clean(Some("To Be Filled By O.E.M.".to_owned())), "");
        assert_eq!(clean(Some("Default string".to_owned())), "");
        assert_eq!(clean(Some("OOOO".to_owned())), "");
        assert_eq!(clean(Some(" Dell Inc. ".to_owned())), "Dell Inc.");
    }
}
