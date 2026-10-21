//! État de l'application et boucle egui.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{
    self, CentralPanel, Color32, Context, Key, Modifiers, Panel, Rect, ScrollArea, Sense,
    TextureHandle, TextureOptions, Vec2, pos2, vec2,
};

use crate::formats::{self, Decoded};
use crate::tree::{Entry, NodeId, Tree, sort_entries};

const ROW_HEIGHT: f32 = 22.0;
const INDENT: f32 = 14.0;
const BG_IMAGE: Color32 = Color32::from_rgb(24, 24, 27);
const BG_PANEL: Color32 = Color32::from_rgb(32, 32, 36);
const SELECTION: Color32 = Color32::from_rgb(56, 98, 168);

/// Résultat de la lecture d'un dossier, renvoyé par un thread.
struct DirLoaded {
    /// Génération de l'arbre au moment de la requête (ignoré si re-raciné).
    epoch: u64,
    id: NodeId,
    entries: Vec<Entry>,
}

/// Image affichée à droite.
struct Shown {
    node: NodeId,
    texture: Option<TextureHandle>,
    original_size: [usize; 2],
    file_size: u64,
    error: Option<String>,
}

pub struct App {
    tree: Tree,
    epoch: u64,
    tx: Sender<DirLoaded>,
    rx: Receiver<DirLoaded>,
    ctx: Context,
    selected: Option<NodeId>,
    /// Chemin à sélectionner une fois son dossier lu.
    pending_select: Option<PathBuf>,
    show_hidden: bool,
    shown: Option<Shown>,
    scroll_to_selected: bool,
    scroll_offset: f32,
    viewport_height: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, root: PathBuf, select: Option<PathBuf>) -> Self {
        let (tx, rx) = channel();
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            tree: Tree::new(root),
            epoch: 0,
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            selected: None,
            pending_select: select,
            show_hidden: false,
            shown: None,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            viewport_height: 0.0,
        };
        app.request_dir(Tree::ROOT);
        app
    }

    /// Lance la lecture d'un dossier dans un thread dédié.
    fn request_dir(&mut self, id: NodeId) {
        let node = &mut self.tree.nodes[id];
        if node.children.is_some() || node.loading {
            return;
        }
        node.loading = true;
        let path = node.path.clone();
        let (epoch, tx, ctx) = (self.epoch, self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let entries = read_dir(&path);
            let _ = tx.send(DirLoaded { epoch, id, entries });
            ctx.request_repaint();
        });
    }

    fn drain_loaded(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            if msg.epoch != self.epoch {
                continue;
            }
            self.tree.set_children(msg.id, msg.entries);
            if let Some(target) = &self.pending_select {
                let found = self.tree.nodes[msg.id]
                    .children
                    .iter()
                    .flatten()
                    .copied()
                    .find(|&c| self.tree.nodes[c].path == *target);
                if let Some(c) = found {
                    self.pending_select = None;
                    self.select(Some(c));
                }
            }
        }
    }

    fn select(&mut self, id: Option<NodeId>) {
        if self.selected != id {
            self.selected = id;
            self.scroll_to_selected = true;
        }
    }

    /// Re-racine l'arbre sur le dossier parent.
    fn go_up(&mut self) {
        let old = self.tree.nodes[Tree::ROOT].path.clone();
        let Some(parent) = old.parent() else { return };
        self.epoch += 1;
        self.tree = Tree::new(parent.to_path_buf());
        self.selected = None;
        self.shown = None;
        self.pending_select = Some(old);
        self.request_dir(Tree::ROOT);
    }

    fn toggle(&mut self, id: NodeId) {
        let node = &mut self.tree.nodes[id];
        if !node.is_dir {
            return;
        }
        node.expanded = !node.expanded;
        if node.expanded {
            self.request_dir(id);
        }
    }

    fn is_image(&self, id: NodeId) -> bool {
        let n = &self.tree.nodes[id];
        !n.is_dir && formats::kind_of(&n.path).is_some()
    }

    /// Images du dossier de la sélection, dans l'ordre d'affichage.
    fn folder_images(&self) -> Vec<NodeId> {
        let Some(sel) = self.selected else { return vec![] };
        let folder = if self.tree.nodes[sel].is_dir {
            Some(sel)
        } else {
            self.tree.nodes[sel].parent
        };
        folder
            .map(|f| {
                self.tree
                    .visible_children(f, self.show_hidden)
                    .into_iter()
                    .filter(|&c| self.is_image(c))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn step_image(&mut self, delta: isize) {
        let imgs = self.folder_images();
        if imgs.is_empty() {
            return;
        }
        let cur = self.selected.and_then(|s| imgs.iter().position(|&i| i == s));
        let next = match cur {
            Some(i) => (i as isize + delta).clamp(0, imgs.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => imgs.len() - 1,
        };
        self.select(Some(imgs[next]));
    }

    fn handle_keys(&mut self, ctx: &Context) {
        let rows = self.tree.visible_rows(self.show_hidden);
        let pos = self.selected.and_then(|s| rows.iter().position(|&r| r == s));
        let mut action = Keys::default();
        ctx.input_mut(|i| {
            action.prev_image = i.consume_key(Modifiers::SHIFT, Key::Space);
            action.next_image = i.consume_key(Modifiers::NONE, Key::Space);
            action.up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
            action.down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
            action.left = i.consume_key(Modifiers::NONE, Key::ArrowLeft);
            action.right = i.consume_key(Modifiers::NONE, Key::ArrowRight);
            action.enter = i.consume_key(Modifiers::NONE, Key::Enter);
            action.back = i.consume_key(Modifiers::NONE, Key::Backspace);
            action.dot = i.consume_key(Modifiers::NONE, Key::Period);
            action.refresh = i.consume_key(Modifiers::NONE, Key::F5);
            action.quit = i.consume_key(Modifiers::NONE, Key::Q)
                || i.consume_key(Modifiers::NONE, Key::Escape);
        });

        if action.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if action.dot {
            self.show_hidden = !self.show_hidden;
        }
        if action.back {
            self.go_up();
            return;
        }
        if action.refresh {
            self.refresh_current_folder();
        }
        if action.next_image {
            self.step_image(1);
        }
        if action.prev_image {
            self.step_image(-1);
        }
        if action.down {
            let next = match pos {
                Some(p) => rows.get(p + 1).copied(),
                None => rows.get(1).copied(),
            };
            if next.is_some() {
                self.select(next);
            }
        }
        if action.up
            && let Some(p) = pos.filter(|&p| p > 1)
        {
            self.select(Some(rows[p - 1]));
        }
        if let Some(sel) = self.selected {
            let (is_dir, expanded, parent) = {
                let n = &self.tree.nodes[sel];
                (n.is_dir, n.expanded, n.parent)
            };
            if action.right && is_dir && !expanded {
                self.toggle(sel);
            } else if action.right
                && is_dir
                && expanded
                && let Some(&first) = self.tree.visible_children(sel, self.show_hidden).first()
            {
                self.select(Some(first));
            }
            if action.left {
                if is_dir && expanded && sel != Tree::ROOT {
                    self.toggle(sel);
                } else if let Some(p) = parent.filter(|&p| p != Tree::ROOT) {
                    self.select(Some(p));
                }
            }
            if action.enter {
                self.toggle(sel);
            }
        }
    }

    /// F5 : relit le dossier de la sélection (ou la racine).
    fn refresh_current_folder(&mut self) {
        let folder = match self.selected {
            Some(s) if self.tree.nodes[s].is_dir => s,
            Some(s) => self.tree.nodes[s].parent.unwrap_or(Tree::ROOT),
            None => Tree::ROOT,
        };
        // Les enfants existants deviennent orphelins dans l'arène : acceptable
        // à cette étape, l'arbre est reconstruit à chaque re-racinage.
        self.tree.nodes[folder].children = None;
        self.selected = Some(folder);
        self.shown = None;
        self.request_dir(folder);
    }

    /// Décode l'image sélectionnée (synchrone, toléré à l'étape 1 uniquement).
    fn ensure_shown(&mut self, ctx: &Context) {
        let want = self.selected.filter(|&s| self.is_image(s));
        match (want, &self.shown) {
            (Some(w), Some(s)) if s.node == w => return,
            (None, _) => {
                self.shown = None;
                return;
            }
            _ => {}
        }
        let w = want.unwrap();
        let path = self.tree.nodes[w].path.clone();
        let max_side = ctx.input(|i| i.max_texture_side);
        let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.shown = Some(match formats::decode(&path, max_side) {
            Ok(Decoded { pixels, original_size }) => Shown {
                node: w,
                texture: Some(ctx.load_texture("image", pixels, TextureOptions::LINEAR)),
                original_size,
                file_size,
                error: None,
            },
            Err(e) => Shown {
                node: w,
                texture: None,
                original_size: [0, 0],
                file_size,
                error: Some(e),
            },
        });
    }

    fn tree_panel(&mut self, ui: &mut egui::Ui) {
        let rows = self.tree.visible_rows(self.show_hidden);
        let has_parent = self.tree.nodes[Tree::ROOT].path.parent().is_some();
        let up_rows = usize::from(has_parent);
        let total = rows.len() + up_rows;

        let mut area = ScrollArea::vertical().auto_shrink([false, false]);
        if self.scroll_to_selected {
            if let Some(idx) = self
                .selected
                .and_then(|s| rows.iter().position(|&r| r == s))
                .map(|p| p + up_rows)
            {
                let top = idx as f32 * ROW_HEIGHT;
                let bottom = top + ROW_HEIGHT;
                if top < self.scroll_offset {
                    area = area.vertical_scroll_offset(top);
                } else if bottom > self.scroll_offset + self.viewport_height {
                    area = area.vertical_scroll_offset(bottom - self.viewport_height);
                }
            }
            self.scroll_to_selected = false;
        }

        let mut clicked: Option<Click> = None;
        let out = area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
            for i in range {
                let width = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
                if i < up_rows {
                    paint_label(ui, rect, 0, "..", Color32::GRAY);
                    if resp.clicked() {
                        clicked = Some(Click::Up);
                    }
                    continue;
                }
                let id = rows[i - up_rows];
                let node = &self.tree.nodes[id];
                if self.selected == Some(id) {
                    ui.painter().rect_filled(rect, 3.0, SELECTION);
                }
                if node.is_dir {
                    paint_chevron(ui, rect, node.depth, node.expanded);
                }
                let color = if node.is_dir {
                    Color32::from_rgb(230, 200, 130)
                } else if node.hidden {
                    Color32::GRAY
                } else {
                    Color32::LIGHT_GRAY
                };
                paint_label(ui, rect, node.depth, &node.name, color);
                if resp.double_clicked() {
                    clicked = Some(Click::Toggle(id));
                } else if resp.clicked() {
                    clicked = Some(Click::Select(id));
                }
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.viewport_height = out.inner_rect.height();

        match clicked {
            Some(Click::Up) => self.go_up(),
            Some(Click::Select(id)) => self.select(Some(id)),
            Some(Click::Toggle(id)) => self.toggle(id),
            None => {}
        }
    }

    fn status_text(&self) -> String {
        let Some(shown) = &self.shown else { return String::new() };
        let node = &self.tree.nodes[shown.node];
        let imgs = self.folder_images();
        let pos = imgs
            .iter()
            .position(|&i| i == shown.node)
            .map(|p| format!("{}/{}", p + 1, imgs.len()))
            .unwrap_or_default();
        let [w, h] = shown.original_size;
        format!(
            "{}   {}×{}   {}   {}",
            node.name,
            w,
            h,
            human_size(shown.file_size),
            pos
        )
    }

    fn image_panel(&self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, BG_IMAGE);
        let Some(shown) = &self.shown else { return };
        if let Some(err) = &shown.error {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                err,
                egui::FontId::proportional(14.0),
                Color32::LIGHT_RED,
            );
            return;
        }
        let Some(tex) = &shown.texture else { return };
        let [ow, oh] = shown.original_size;
        let avail = rect.size();
        let scale = (avail.x / ow as f32).min(avail.y / oh as f32).min(1.0);
        let size = Vec2::new(ow as f32 * scale, oh as f32 * scale);
        let img_rect = Rect::from_center_size(rect.center(), size);
        ui.painter().image(
            tex.id(),
            img_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.drain_loaded();
        self.handle_keys(ctx);
        self.ensure_shown(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let panel_frame = egui::Frame::new()
            .fill(BG_PANEL)
            .inner_margin(egui::Margin::symmetric(8, 6));

        Panel::bottom("status")
            .frame(panel_frame)
            .exact_size(26.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(self.status_text()).color(Color32::GRAY));
            });
        Panel::left("tree")
            .frame(egui::Frame::new().fill(BG_PANEL).inner_margin(4))
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| self.tree_panel(ui));
        CentralPanel::default()
            .frame(egui::Frame::new().fill(BG_IMAGE))
            .show(ui, |ui| self.image_panel(ui));
    }
}

#[derive(Default)]
struct Keys {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    enter: bool,
    back: bool,
    dot: bool,
    refresh: bool,
    quit: bool,
    next_image: bool,
    prev_image: bool,
}

enum Click {
    Up,
    Select(NodeId),
    Toggle(NodeId),
}

fn paint_label(ui: &egui::Ui, rect: Rect, depth: usize, text: &str, color: Color32) {
    let x = rect.left() + 18.0 + depth as f32 * INDENT;
    ui.painter().text(
        pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.5),
        color,
    );
}

fn paint_chevron(ui: &egui::Ui, rect: Rect, depth: usize, open: bool) {
    let c = pos2(rect.left() + 8.0 + depth as f32 * INDENT, rect.center().y);
    let pts = if open {
        vec![c + vec2(-3.5, -2.0), c + vec2(3.5, -2.0), c + vec2(0.0, 3.0)]
    } else {
        vec![c + vec2(-2.0, -3.5), c + vec2(3.0, 0.0), c + vec2(-2.0, 3.5)]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        pts,
        Color32::GRAY,
        egui::Stroke::NONE,
    ));
}

/// Lit un dossier : ne garde que les dossiers et les images supportées.
fn read_dir(path: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(path) else { return vec![] };
    let mut entries: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            // `fs::metadata` suit les liens symboliques.
            let is_dir = fs::metadata(&path).ok()?.is_dir();
            if !is_dir && formats::kind_of(&path).is_none() {
                return None;
            }
            Some(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                is_dir,
            })
        })
        .collect();
    sort_entries(&mut entries);
    entries
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["o", "Ko", "Mo", "Go"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} o")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512 o");
        assert_eq!(human_size(2048), "2.0 Ko");
        assert_eq!(human_size(20 * 1024 * 1024), "20.0 Mo");
    }
}
