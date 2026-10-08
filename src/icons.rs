//! Cache de ícones de executáveis. A extração (Shell + GDI) roda numa thread própria
//! para a tabela não engasgar quando aparecem dezenas de processos novos de uma vez.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{ColorImage, TextureHandle, TextureOptions};

use crate::platform;

type Pixels = Option<([usize; 2], Vec<u8>)>;

pub struct IconCache {
    textures: HashMap<PathBuf, Option<TextureHandle>>,
    pending: HashSet<PathBuf>,
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Pixels)>,
}

impl IconCache {
    pub fn new(ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("icons".into())
            .spawn(move || {
                platform::init_com_for_icons();
                for path in req_rx {
                    let pixels = platform::extract_icon(&path);
                    if res_tx.send((path, pixels)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("thread de ícones");
        Self { textures: HashMap::new(), pending: HashSet::new(), requests: req_tx, results: res_rx }
    }

    /// Carrega as texturas que ficaram prontas. Chamar uma vez por quadro.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((path, pixels)) = self.results.try_recv() {
            self.pending.remove(&path);
            let texture = pixels.map(|(size, rgba)| {
                let image = ColorImage::from_rgba_unmultiplied(size, &rgba);
                ctx.load_texture(path.display().to_string(), image, TextureOptions::LINEAR)
            });
            self.textures.insert(path, texture);
        }
    }

    /// Ícone do executável, ou `None` enquanto carrega ou se não houver ícone.
    pub fn get(&mut self, path: Option<&Path>) -> Option<TextureHandle> {
        let path = path?;
        if let Some(entry) = self.textures.get(path) {
            return entry.clone();
        }
        if self.pending.insert(path.to_path_buf()) {
            let _ = self.requests.send(path.to_path_buf());
        }
        None
    }
}
