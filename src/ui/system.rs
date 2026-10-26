//! Tela "Sistema": a ficha da máquina — modelo, processador, memória instalada,
//! vídeo, monitores, unidades e versão do Windows. Tudo o que se pede num chamado
//! de suporte, com um botão que copia a ficha inteira em texto.

use egui::{Align, Color32, Layout, RichText, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::app::App;
use crate::format;
use crate::sampler::{Snapshot, percent_of};
use crate::system::{self, SystemInfo};
use crate::theme::{self, ACCENT, CPU, DISK, GPU, INK, INK_2, INK_3, MEMORY};
use crate::ui::widgets;

pub fn show(app: &mut App, ui: &mut Ui, s: &Snapshot) {
    // Uma leitura por execução: registro, SMBIOS e GDI não mudam com o app aberto.
    let info = app.system_info.get_or_insert_with(|| std::sync::Arc::new(SystemInfo::load())).clone();
    let mut copy = false;

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(info.machine()).font(theme::semibold(22.0)).color(INK));
            let family = if info.family.is_empty() { String::new() } else { format!("{} · ", info.family) };
            ui.label(RichText::new(format!("{family}{}", s.host)).font(theme::regular(12.5)).color(INK_3));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            copy = widgets::ghost_button(ui, &format!("{}  Copiar ficha", icon::COPY)).clicked();
        });
    });
    ui.add_space(14.0);
    if copy {
        ui.ctx().copy_text(report(&info, s));
        app.toast("Ficha do computador copiada".to_owned(), crate::app::ToastKind::Ok);
    }

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.columns(2, |cols| {
            equipment(&mut cols[0], &info);
            windows(&mut cols[1], &info, s);
        });
        ui.add_space(10.0);
        ui.columns(2, |cols| {
            processor(&mut cols[0], &info, s);
            memory(&mut cols[1], &info, s);
        });
        ui.add_space(10.0);
        video(ui, &info, s);
        ui.add_space(10.0);
        ui.columns(2, |cols| {
            monitors(&mut cols[0], &info);
            storage(&mut cols[1], s);
        });
    });
}

fn equipment(ui: &mut Ui, info: &SystemInfo) {
    card(ui, icon::COMPUTER_TOWER, "Equipamento", ACCENT, |ui| {
        grid(ui, "equipment", |ui| {
            field(ui, "Fabricante", &info.manufacturer);
            field(ui, "Modelo", &info.model);
            field(ui, "Família", &info.family);
            field(ui, "Placa-mãe", &info.board);
            field(ui, "BIOS", &info.bios);
            field(ui, "Data do BIOS", &system::us_date(&info.bios_date));
            field(ui, "Tipo de sistema", &info.arch);
            field(ui, "Usuário", &info.user);
        });
    });
}

fn windows(ui: &mut Ui, info: &SystemInfo, s: &Snapshot) {
    card(ui, icon::WINDOWS_LOGO, "Windows", INK_2, |ui| {
        grid(ui, "windows", |ui| {
            field(ui, "Edição", &info.os_name);
            field(ui, "Versão", &info.os_version);
            field(ui, "Compilação", &info.os_build);
            if let Some(at) = info.os_installed {
                field(ui, "Instalado em", &format::datetime(at));
            }
            field(ui, "Ligado há", &format::duration(s.uptime));
            field(ui, "Processos", &s.processes.len().to_string());
        });
    });
}

fn processor(ui: &mut Ui, info: &SystemInfo, s: &Snapshot) {
    card(ui, icon::CPU, "Processador", CPU, |ui| {
        let name = if info.cpu_name.is_empty() { s.cpu_brand.clone() } else { info.cpu_name.clone() };
        let cores = match s.physical_cores {
            Some(p) => format!("{p} físicos · {} lógicos", s.cores.len()),
            None => format!("{} lógicos", s.cores.len()),
        };
        grid(ui, "processor", |ui| {
            field(ui, "Modelo", &name);
            field(ui, "Fabricante", &info.cpu_vendor);
            if info.cpu_base_mhz > 0 {
                field(ui, "Velocidade base", &format::ghz(info.cpu_base_mhz));
            }
            field(ui, "Núcleos", &cores);
            field(ui, "Identificação", &info.cpu_identifier);
        });
    });
}

fn memory(ui: &mut Ui, info: &SystemInfo, s: &Snapshot) {
    card(ui, icon::MEMORY, "Memória", MEMORY, |ui| {
        // O firmware conta a memória instalada; o Windows informa quanto sobra para
        // o sistema, que é menos (parte fica reservada para o hardware).
        let installed = info.memory_installed();
        grid(ui, "memory", |ui| {
            field(ui, "Instalada", &format::bytes(if installed > 0 { installed } else { s.mem_total }));
            field(ui, "Disponível ao Windows", &format::bytes(s.mem_total));
            if !info.memory.is_empty() {
                field(ui, "Slots", &format!("{} de {} ocupados", info.slots_used(), info.memory.len()));
            }
        });
        for module in info.memory.iter().filter(|m| !m.empty()) {
            ui.add_space(6.0);
            // Um embaixo do outro: o nome do slot ("Controller0-ChannelA-DIMM0") é
            // longo, e lado a lado o detalhe alinhado à direita passava por cima dele.
            // O detalhe quebra entre palavras: o código da peça não cabe partido.
            ui.label(RichText::new(&module.locator).font(theme::semibold(12.0)).color(INK_2));
            ui.add(egui::Label::new(RichText::new(module_line(module)).font(theme::regular(12.0)).color(INK_3)).wrap());
        }
    });
}

/// `16,0 GB · DDR4 SODIMM · 2667 MT/s · Kingston KF548S38`, sem os campos vazios.
fn module_line(m: &system::MemoryModule) -> String {
    let mut parts = vec![format::bytes(m.size)];
    let kind = match (m.kind.as_str(), m.form.as_str()) {
        ("", "") => String::new(),
        ("", form) => form.to_owned(),
        (kind, "") => kind.to_owned(),
        (kind, form) => format!("{kind} {form}"),
    };
    if !kind.is_empty() {
        parts.push(kind);
    }
    if m.speed > 0 {
        parts.push(format!("{} MT/s", m.speed));
    }
    let maker = match (m.manufacturer.is_empty(), m.part.is_empty()) {
        (false, false) => format!("{} {}", m.manufacturer, m.part),
        (false, true) => m.manufacturer.clone(),
        (true, false) => m.part.clone(),
        (true, true) => String::new(),
    };
    if !maker.is_empty() {
        parts.push(maker);
    }
    parts.join(" · ")
}

fn video(ui: &mut Ui, info: &SystemInfo, s: &Snapshot) {
    card(ui, icon::GRAPHICS_CARD, "Vídeo", GPU, |ui| {
        if s.gpus.is_empty() && info.video.is_empty() {
            ui.label(RichText::new("Nenhum adaptador de vídeo encontrado.").font(theme::regular(12.5)).color(INK_3));
            return;
        }
        // A coleta conhece a memória dedicada e o uso; o registro, o driver.
        for (i, gpu) in s.gpus.iter().enumerate() {
            if i > 0 {
                ui.add_space(10.0);
                crate::app::hairline(ui);
                ui.add_space(10.0);
            }
            ui.label(RichText::new(&gpu.name).font(theme::semibold(13.0)).color(INK));
            ui.add_space(4.0);
            grid(ui, &format!("gpu{i}"), |ui| {
                field(ui, "Memória dedicada", &format::bytes(gpu.vram_total));
                if let Some(driver) = info.driver_for(&gpu.name) {
                    field(ui, "Driver", &driver.version);
                    field(ui, "Data do driver", &system::us_date(&driver.date));
                    field(ui, "Fornecedor", &driver.provider);
                }
            });
        }
        // Sem contadores de GPU no PDH, o registro é o que sobra. Fora isso ele não
        // serve para listar: além das placas, traz as telas virtuais do DisplayLink
        // e do Miracast, que não são placa de vídeo nenhuma.
        if s.gpus.is_empty() {
            for (i, driver) in info.video.iter().enumerate() {
                if i > 0 {
                    ui.add_space(10.0);
                    crate::app::hairline(ui);
                    ui.add_space(10.0);
                }
                ui.label(RichText::new(&driver.desc).font(theme::semibold(13.0)).color(INK));
                ui.add_space(4.0);
                grid(ui, &format!("driver{i}"), |ui| {
                    field(ui, "Driver", &driver.version);
                    field(ui, "Data do driver", &system::us_date(&driver.date));
                    field(ui, "Fornecedor", &driver.provider);
                });
            }
        }
    });
}

fn monitors(ui: &mut Ui, info: &SystemInfo) {
    card(ui, icon::MONITOR, "Monitores", INK_2, |ui| {
        if info.displays.is_empty() {
            ui.label(RichText::new("Nenhum monitor ativo.").font(theme::regular(12.5)).color(INK_3));
            return;
        }
        // Com mais de uma tela, o adaptador é o que diferencia: o nome do monitor
        // costuma ser o mesmo genérico em todas.
        let many = info.displays.len() > 1;
        for display in &info.displays {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&display.name).font(theme::regular(12.5)).color(INK_2));
                if display.primary {
                    ui.label(RichText::new("principal").font(theme::regular(11.0)).color(ACCENT));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(resolution(display)).font(theme::regular(12.0)).color(INK_3));
                });
            });
            if many && !display.adapter.is_empty() {
                ui.label(RichText::new(&display.adapter).font(theme::regular(11.0)).color(theme::INK_4));
            }
            ui.add_space(6.0);
        }
    });
}

fn resolution(d: &system::Display) -> String {
    if d.hz > 0 {
        format!("{} × {} · {} Hz", d.width, d.height, d.hz)
    } else {
        format!("{} × {}", d.width, d.height)
    }
}

fn storage(ui: &mut Ui, s: &Snapshot) {
    card(ui, icon::HARD_DRIVES, "Armazenamento", DISK, |ui| {
        for d in &s.disks {
            let used = d.total.saturating_sub(d.available);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.mount).font(theme::semibold(12.5)).color(INK));
                let label = if d.label.is_empty() { String::new() } else { format!("{} · ", d.label) };
                ui.label(RichText::new(format!("{label}{} · {}", d.file_system, d.kind)).font(theme::regular(11.5)).color(INK_3));
            });
            ui.add_space(4.0);
            widgets::meter(ui, percent_of(used, d.total) / 100.0, DISK, 6.0);
            ui.add_space(3.0);
            ui.label(
                RichText::new(format!("{} livres de {}", format::bytes(d.available), format::bytes(d.total)))
                    .font(theme::regular(11.5))
                    .color(INK_3),
            );
            ui.add_space(8.0);
        }
    });
}

/// Cartão com cabeçalho de ícone e título; o corpo recebe a largura toda.
fn card(ui: &mut Ui, glyph: &str, title: &str, color: Color32, body: impl FnOnce(&mut Ui)) {
    widgets::card().show(ui, |ui| {
        // `vertical`: dentro de `columns` o Frame herdaria o layout de quem chamou.
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(glyph).font(theme::regular(15.0)).color(color));
                widgets::section_label(ui, title);
            });
            ui.add_space(8.0);
            body(ui);
        });
    });
}

fn grid(ui: &mut Ui, id: &str, body: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id).num_columns(2).spacing(vec2(14.0, 7.0)).min_col_width(118.0).show(ui, body);
}

/// Linha chave/valor. Campo que o firmware não traz não vira linha vazia.
///
/// Quebra entre palavras, ao contrário do `widgets::field`: aqui não há caminho
/// nem linha de comando, e no meio da palavra saía "Single Langu / age".
fn field(ui: &mut Ui, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    ui.label(RichText::new(label).font(theme::regular(12.0)).color(INK_3));
    ui.add(egui::Label::new(RichText::new(value).font(theme::regular(12.5)).color(INK)).wrap().selectable(true));
    ui.end_row();
}

/// A ficha inteira em texto, para colar num chamado.
fn report(info: &SystemInfo, s: &Snapshot) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: String| {
        if !value.trim().is_empty() {
            out.push_str(&format!("{label}: {value}\r\n"));
        }
    };
    line("Computador", info.machine());
    line("Família", info.family.clone());
    line("Nome", s.host.clone());
    line("Usuário", info.user.clone());
    line("Placa-mãe", info.board.clone());
    line("BIOS", format!("{} ({})", info.bios, system::us_date(&info.bios_date)));
    line("Tipo de sistema", info.arch.clone());
    line(
        "Windows",
        format!("{} {} (compilação {})", info.os_name, info.os_version, info.os_build).replace("  ", " "),
    );
    if let Some(at) = info.os_installed {
        line("Instalado em", format::datetime(at));
    }
    line("Ligado há", format::duration(s.uptime));

    let cpu = if info.cpu_name.is_empty() { s.cpu_brand.clone() } else { info.cpu_name.clone() };
    let cores = match s.physical_cores {
        Some(p) => format!("{p} núcleos físicos, {} lógicos", s.cores.len()),
        None => format!("{} núcleos lógicos", s.cores.len()),
    };
    line("Processador", format!("{cpu} — {cores}"));

    let installed = info.memory_installed();
    line("Memória", format::bytes(if installed > 0 { installed } else { s.mem_total }));
    for module in info.memory.iter().filter(|m| !m.empty()) {
        line(&format!("  {}", module.locator), module_line(module));
    }
    for gpu in &s.gpus {
        let driver = info.driver_for(&gpu.name).map(|d| format!(" — driver {}", d.version)).unwrap_or_default();
        line("Vídeo", format!("{} ({}){driver}", gpu.name, format::bytes(gpu.vram_total)));
    }
    for display in &info.displays {
        let main = if display.primary { " (principal)" } else { "" };
        line("Monitor", format!("{} — {}{main}", display.name, resolution(display)));
    }
    for d in &s.disks {
        line(
            &format!("Disco {}", d.mount),
            format!("{} de {} livres · {}", format::bytes(d.available), format::bytes(d.total), d.kind),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{Display, MemoryModule};

    #[test]
    fn linha_do_modulo_pula_campos_vazios() {
        let full = MemoryModule {
            locator: "DIMM A1".to_owned(),
            size: 16 * 1024 * 1024 * 1024,
            kind: "DDR4".to_owned(),
            form: "SODIMM".to_owned(),
            speed: 2667,
            manufacturer: "Kingston".to_owned(),
            part: "KF548S38".to_owned(),
        };
        assert_eq!(module_line(&full), "16,0 GB · DDR4 SODIMM · 2667 MT/s · Kingston KF548S38");

        let bare = MemoryModule { size: 8 * 1024 * 1024 * 1024, ..Default::default() };
        assert_eq!(module_line(&bare), "8,00 GB");
    }

    #[test]
    fn resolucao_sem_taxa_nao_mostra_hz() {
        let d = Display { width: 1366, height: 768, hz: 60, ..Default::default() };
        assert_eq!(resolution(&d), "1366 × 768 · 60 Hz");
        assert_eq!(resolution(&Display { hz: 0, ..d }), "1366 × 768");
    }
}
