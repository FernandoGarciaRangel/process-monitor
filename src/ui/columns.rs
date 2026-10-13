//! Largura das colunas das tabelas e as divisórias que a ajustam.
//!
//! O redimensionamento do egui_extras muda só a coluna à esquerda da divisória e
//! deixa a coluna `remainder` compensar. Com essa coluna (o nome) à esquerda das
//! outras, a coluna arrastada crescia para a esquerda: a divisória ficava parada e
//! o mouse seguia sozinho. Aqui a divisória move a fronteira entre as duas colunas
//! vizinhas: uma ganha o que a outra perde, e a divisória acompanha o mouse.

use std::collections::HashMap;

use egui::{CursorIcon, Id, Rect, Sense, Ui, pos2, vec2};
use egui_extras::Column;

pub trait TableCol: Copy {
    /// Nome estável da coluna: é a chave da largura salva entre execuções.
    fn key(self) -> &'static str;
    /// Largura inicial; `None` na coluna que fica com o espaço que sobra.
    fn initial_width(self) -> Option<f32>;
    fn min_width(self) -> f32;
}

/// Largura por coluna, guardada na memória persistente do egui (o eframe a salva ao fechar).
type Widths = HashMap<String, f32>;

fn state_id(salt: &str) -> Id {
    Id::new(("column_widths", salt))
}

/// Esquece as larguras que o usuário deixou na tabela `salt`.
pub fn reset_widths(ctx: &egui::Context, salt: &str) {
    ctx.data_mut(|d| d.remove::<Widths>(state_id(salt)));
}

fn load(ui: &Ui, id: Id) -> Widths {
    ui.data_mut(|d| d.get_persisted(id)).unwrap_or_default()
}

/// As colunas do `TableBuilder`, com a largura que o usuário deixou.
pub fn columns<C: TableCol>(ui: &Ui, salt: &str, cols: &[C]) -> Vec<Column> {
    let widths = load(ui, state_id(salt));
    cols.iter()
        .map(|&col| match col.initial_width() {
            None => Column::remainder().at_least(col.min_width()).clip(true),
            Some(initial) => Column::exact(widths.get(col.key()).copied().unwrap_or(initial).max(col.min_width())),
        })
        .collect()
}

/// Desenha e trata as divisórias entre as colunas. `cells` são os retângulos das
/// células do cabeçalho, na ordem de `cols`. Chame depois da tabela, para a
/// divisória ficar por cima das linhas, que também respondem ao clique.
pub fn dividers<C: TableCol>(ui: &mut Ui, salt: &str, cols: &[C], cells: &[Rect], rows: usize, row_height: f32) {
    let Some(header) = cells.first() else {
        return;
    };
    let top = header.top();
    // A linha desce até a última linha da tabela, ou até o fim da área visível.
    let spacing = ui.spacing().item_spacing.y;
    let bottom = (header.bottom() + rows as f32 * (row_height + spacing)).min(ui.min_rect().bottom());
    let id = state_id(salt);
    let mut widths = load(ui, id);
    let mut changed = false;
    let grab = ui.style().interaction.resize_grab_radius_side;
    let dragging_something_else = ui.input(|i| i.pointer.any_down() || i.pointer.any_pressed());

    for i in 0..cells.len().min(cols.len()).saturating_sub(1) {
        let (left, right) = (cols[i], cols[i + 1]);
        let x = (cells[i].right() + cells[i + 1].left()) / 2.0;
        let rect = Rect::from_x_y_ranges(x..=x, top..=bottom).expand2(vec2(grab, 0.0));
        let response = ui.interact(rect, id.with(i), Sense::drag());

        if response.dragged()
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            let (w_left, w_right) = (cells[i].width(), cells[i + 1].width());
            // Nenhuma das duas passa do mínimo; os limites incluem 0 para o clamp não
            // inverter quando a janela já espremeu uma coluna abaixo do mínimo.
            let lo = (left.min_width() - w_left).min(0.0);
            let hi = (w_right - right.min_width()).max(0.0);
            let delta = (pointer.x - x).clamp(lo, hi);
            if delta != 0.0 {
                // A coluna de sobra não guarda largura: ela recebe a diferença sozinha.
                if left.initial_width().is_some() {
                    widths.insert(left.key().to_owned(), w_left + delta);
                }
                if right.initial_width().is_some() {
                    widths.insert(right.key().to_owned(), w_right - delta);
                }
                changed = true;
                ui.ctx().request_repaint();
            }
        }

        let hovered = response.hovered() && !dragging_something_else;
        if hovered || response.dragged() {
            ui.set_cursor_icon(CursorIcon::ResizeColumn);
        }
        let visuals = &ui.style().visuals.widgets;
        let stroke = if response.dragged() {
            visuals.active.bg_stroke
        } else if hovered {
            visuals.hovered.bg_stroke
        } else {
            visuals.noninteractive.bg_stroke
        };
        ui.painter().line_segment([pos2(x, top), pos2(x, bottom)], stroke);
    }

    if changed {
        ui.data_mut(|d| d.insert_persisted(id, widths));
    }
}

/// Coluna e sentido da ordenação de uma tabela, salvos entre execuções.
pub fn sort_state(ui: &Ui, salt: &str, default: (&str, bool)) -> (String, bool) {
    ui.data_mut(|d| d.get_persisted(Id::new(("column_sort", salt)))).unwrap_or((default.0.to_owned(), default.1))
}

/// Cabeçalho que ordena: a seta fica do lado de dentro da coluna ativa. Clicar na ativa
/// inverte o sentido; em outra, ordena por ela no sentido `desc_first`.
pub fn sort_header<C: TableCol>(ui: &mut Ui, salt: &str, col: C, label: &str, right: bool, desc_first: bool, sort: &(String, bool)) {
    use crate::theme::{self, HAIRLINE, INK_2, INK_3};
    let rect = ui.max_rect();
    let response = ui.interact(rect, ui.id().with(("sort", col.key())), Sense::click());
    let active = sort.0 == col.key();
    let caret = if sort.1 { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_UP };
    let text = match (active, right) {
        (true, true) => format!("{caret} {label}"),
        (true, false) => format!("{label} {caret}"),
        (false, _) => label.to_owned(),
    };
    let color = if active || response.hovered() { INK_2 } else { INK_3 };
    let (x, align) =
        if right { (rect.right() - 10.0, egui::Align2::RIGHT_BOTTOM) } else { (rect.left() + 8.0, egui::Align2::LEFT_BOTTOM) };
    let painter = ui.painter();
    painter.text(pos2(x, rect.bottom() - 7.0), align, text, theme::regular(11.5), color);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(1.0, HAIRLINE));
    if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        let next = if active { (sort.0.clone(), !sort.1) } else { (col.key().to_owned(), desc_first) };
        ui.data_mut(|d| d.insert_persisted(Id::new(("column_sort", salt)), next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};
    use egui_extras::TableBuilder;

    /// Nome à esquerda, como nas tabelas do app: foi aí que a coluna crescia ao contrário.
    #[derive(Clone, Copy, Debug)]
    enum C {
        Name,
        A,
        B,
    }

    impl TableCol for C {
        fn key(self) -> &'static str {
            match self {
                C::Name => "name",
                C::A => "a",
                C::B => "b",
            }
        }

        fn initial_width(self) -> Option<f32> {
            match self {
                C::Name => None,
                C::A | C::B => Some(80.0),
            }
        }

        fn min_width(self) -> f32 {
            match self {
                C::Name => 100.0,
                C::A | C::B => 40.0,
            }
        }
    }

    const COLS: [C; 3] = [C::Name, C::A, C::B];

    /// Desenha um quadro da tabela e devolve as células do cabeçalho.
    fn frame(ctx: &Context, events: Vec<Event>) -> Vec<Rect> {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))),
            events,
            ..Default::default()
        };
        let mut cells = Vec::new();
        let output = ctx.run_ui(input, |ui| {
            let mut header_cells = Vec::new();
            let table_columns = columns(ui, "t", &COLS);
            let mut table = TableBuilder::new(ui).id_salt("t").auto_shrink([false, false]);
            for column in table_columns {
                table = table.column(column);
            }
            table
                .header(20.0, |mut header| {
                    for _ in COLS {
                        header.col(|ui| header_cells.push(ui.max_rect()));
                    }
                })
                .body(|body| {
                    body.rows(20.0, 5, |mut row| {
                        for _ in COLS {
                            row.col(|ui| {
                                ui.label("x");
                            });
                        }
                    });
                });
            dividers(ui, "t", &COLS, &header_cells, 5, 20.0);
            cells = header_cells;
        });
        output.drop_without_applying_deltas();
        cells
    }

    fn boundary(cells: &[Rect], i: usize) -> f32 {
        (cells[i].right() + cells[i + 1].left()) / 2.0
    }

    fn moved(pos: Pos2) -> Vec<Event> {
        vec![Event::PointerMoved(pos)]
    }

    fn button(pos: Pos2, pressed: bool) -> Vec<Event> {
        vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]
    }

    /// Alguns quadros sem entrada, para a largura nova assentar.
    fn settle(ctx: &Context) -> Vec<Rect> {
        frame(ctx, vec![]);
        frame(ctx, vec![]);
        frame(ctx, vec![])
    }

    /// Arrasta a divisória `i` em `dx` pontos, aos poucos, como a mão faz.
    fn drag(ctx: &Context, i: usize, dx: f32) -> (Vec<Rect>, Vec<Rect>) {
        let before = settle(ctx);
        let start = pos2(boundary(&before, i), 50.0);
        frame(ctx, moved(start));
        frame(ctx, button(start, true));
        let steps = 10;
        for s in 1..=steps {
            frame(ctx, moved(start + vec2(dx * s as f32 / steps as f32, 0.0)));
        }
        let end = start + vec2(dx, 0.0);
        frame(ctx, button(end, false));
        let after = settle(ctx);
        (before, after)
    }

    #[test]
    fn divisoria_acompanha_o_mouse_para_a_direita() {
        let ctx = Context::default();
        let (before, after) = drag(&ctx, 1, 30.0);
        // A divisória entre A e B foi para onde o mouse foi.
        assert!((boundary(&after, 1) - (boundary(&before, 1) + 30.0)).abs() < 1.0, "{before:?} → {after:?}");
        // A, à esquerda, cresceu; B encolheu; Nome não mudou.
        assert!((after[1].width() - (before[1].width() + 30.0)).abs() < 1.0);
        assert!((after[2].width() - (before[2].width() - 30.0)).abs() < 1.0);
        assert!((after[0].width() - before[0].width()).abs() < 1.0);
    }

    #[test]
    fn divisoria_acompanha_o_mouse_para_a_esquerda() {
        let ctx = Context::default();
        let (before, after) = drag(&ctx, 1, -25.0);
        assert!((boundary(&after, 1) - (boundary(&before, 1) - 25.0)).abs() < 1.0, "{before:?} → {after:?}");
        assert!((after[1].width() - (before[1].width() - 25.0)).abs() < 1.0);
        assert!((after[2].width() - (before[2].width() + 25.0)).abs() < 1.0);
    }

    #[test]
    fn divisoria_do_nome_move_a_fronteira_com_a_vizinha() {
        let ctx = Context::default();
        let (before, after) = drag(&ctx, 0, 40.0);
        assert!((boundary(&after, 0) - (boundary(&before, 0) + 40.0)).abs() < 1.0, "{before:?} → {after:?}");
        assert!((after[0].width() - (before[0].width() + 40.0)).abs() < 1.0);
        assert!((after[1].width() - (before[1].width() - 40.0)).abs() < 1.0);
        // A divisória seguinte não saiu do lugar.
        assert!((boundary(&after, 1) - boundary(&before, 1)).abs() < 1.0);
    }

    #[test]
    fn arrasto_para_no_minimo_da_vizinha() {
        let ctx = Context::default();
        let (before, after) = drag(&ctx, 1, 300.0);
        assert!((after[2].width() - C::B.min_width()).abs() < 1.0, "{after:?}");
        assert!((after[1].width() - (before[1].width() + before[2].width() - C::B.min_width())).abs() < 1.0);
    }
}
