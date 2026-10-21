//! Application state and egui loop.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, CentralPanel, Color32, Context, Key, Modifiers, Panel, Rect, ScrollArea, Sense,
    TextureFilter, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};

use crate::formats::{self, Kind, MAX_SVG_SIDE};
use crate::loader::{Loader, Lru, Priority, Response};
use crate::thumbs::grid_columns;
use crate::tree::{NodeId, Tree};
use crate::viewer::{KEY_STEP, View};

const ROW_HEIGHT: f32 = 22.0;
const INDENT: f32 = 14.0;
/// Mouse wheel sensitivity: exponent applied to the scroll amount in points.
const WHEEL_SENSITIVITY: f32 = 0.0015;
const BG_IMAGE: Color32 = Color32::from_rgb(24, 24, 27);
const BG_PANEL: Color32 = Color32::from_rgb(32, 32, 36);
const SELECTION: Color32 = Color32::from_rgb(56, 98, 168);
/// Subtle dividers between areas.
const DIVIDER: Color32 = Color32::from_rgb(44, 44, 51);
const TEXT_DIM: Color32 = Color32::from_rgb(128, 128, 138);
const TEXT_NORMAL: Color32 = Color32::from_rgb(205, 205, 212);
const FOLDER_TEXT: Color32 = Color32::from_rgb(226, 196, 130);

/// Target width of a grid cell, in points.
const TARGET_CELL: f32 = 110.0;
/// Thumbnail texture cache budget, in bytes.
const THUMB_CACHE_BYTES: usize = 192 * 1024 * 1024;
/// Rows of thumbnails requested beyond the visible area.
const THUMB_MARGIN_ROWS: usize = 2;
/// Rounding step for the SVG rasterisation size (avoids re-rasterising on every wheel notch).
const SVG_BUCKET: usize = 512;
/// Delay without zoom change before re-rasterising an SVG.
const SVG_DEBOUNCE: Duration = Duration::from_millis(150);

/// Mipmaps: much better downscaling quality when the image is displayed
/// far smaller than its texture.
const TEXTURE_OPTIONS: TextureOptions = TextureOptions {
    mipmap_mode: Some(TextureFilter::Linear),
    ..TextureOptions::LINEAR
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tree,
    Thumbs,
}

/// Size of the full-size texture window: current + neighbours n±2.
const FULL_CACHE_SLOTS: usize = 5;

#[derive(Clone)]
struct Cached {
    texture: TextureHandle,
    original_size: [usize; 2],
    file_size: u64,
    /// Longest side of the texture (for SVGs: the rasterisation resolution).
    raster_side: usize,
}

/// Image currently displayed (or its error).
struct Display {
    path: PathBuf,
    item: Result<Cached, String>,
}

pub struct App {
    tree: Tree,
    epoch: u64,
    loader: Loader,
    ctx: Context,
    cache: Lru<PathBuf, Cached>,
    failed: HashMap<PathBuf, String>,
    display: Option<Display>,
    mode: Mode,
    /// SVG re-rasterisation waiting for the zoom to settle.
    svg_pending: Option<(PathBuf, usize, Instant)>,
    svg_inflight: Option<(PathBuf, usize)>,
    thumbs: Lru<PathBuf, TextureHandle>,
    thumbs_failed: HashSet<PathBuf>,
    /// Last thumbnail request issued: (folder, range of visible indices).
    thumb_key: Option<(NodeId, (usize, usize))>,
    thumbs_dirty: bool,
    /// Range of image indices visible in the grid in the last frame.
    thumb_range: (usize, usize),
    grid_cols: usize,
    view: View,
    view_cmd: Option<ViewCmd>,
    fullscreen: bool,
    show_help: bool,
    /// Size (in points) of the image area in the last frame.
    last_avail: Vec2,
    retarget_needed: bool,
    max_side: usize,
    selected: Option<NodeId>,
    /// Path to select once its folder has been read.
    pending_select: Option<PathBuf>,
    show_hidden: bool,
    scroll_to_selected: bool,
    scroll_offset: f32,
    viewport_height: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, root: PathBuf, select: Option<PathBuf>) -> Self {
        apply_theme(&cc.egui_ctx);
        let mut app = Self {
            tree: Tree::new(root),
            epoch: 0,
            loader: Loader::new(&cc.egui_ctx),
            ctx: cc.egui_ctx.clone(),
            cache: Lru::new(FULL_CACHE_SLOTS),
            failed: HashMap::new(),
            display: None,
            mode: Mode::Tree,
            svg_pending: None,
            svg_inflight: None,
            thumbs: Lru::new(THUMB_CACHE_BYTES),
            thumbs_failed: HashSet::new(),
            thumb_key: None,
            thumbs_dirty: false,
            thumb_range: (0, 0),
            grid_cols: 1,
            view: View::default(),
            view_cmd: None,
            fullscreen: false,
            show_help: false,
            last_avail: Vec2::ZERO,
            retarget_needed: false,
            max_side: 4096,
            selected: None,
            pending_select: select,
            show_hidden: false,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            viewport_height: 0.0,
        };
        app.request_dir(Tree::ROOT);
        app
    }

    /// Asks the loader to read a folder (off the UI thread).
    fn request_dir(&mut self, id: NodeId) {
        let node = &mut self.tree.nodes[id];
        if node.children.is_some() || node.loading {
            return;
        }
        node.loading = true;
        self.loader.request_dir(self.epoch, id, node.path.clone());
    }

    fn drain_loaded(&mut self) {
        while let Some(resp) = self.loader.try_recv() {
            match resp {
                Response::Dir { epoch, id, entries } => {
                    if epoch == self.epoch {
                        self.on_dir_loaded(id, entries);
                    }
                }
                Response::Full { path, result } => self.on_full(path, result),
                Response::Svg { path, result } => self.on_svg(path, result),
                Response::Thumb { path, result } => self.on_thumb(path, result),
            }
        }
    }

    fn on_dir_loaded(&mut self, id: NodeId, entries: Vec<crate::tree::Entry>) {
        self.tree.set_children(id, entries);
        self.retarget_needed = true;
        self.thumbs_dirty = true;
        if let Some(target) = &self.pending_select {
            let found = self.tree.nodes[id]
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

    /// A texture is only created if the image is still wanted.
    fn on_full(&mut self, path: PathBuf, result: Result<formats::Decoded, String>) {
        if !self.loader.is_wanted(&path) || self.cache.contains(&path) {
            return;
        }
        match result {
            Ok(d) => {
                let cached = self.make_cached(&path, d);
                self.cache.put(path, cached, 1);
            }
            Err(e) => {
                self.failed.insert(path, e);
            }
        }
    }

    fn make_cached(&self, path: &Path, d: formats::Decoded) -> Cached {
        let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let raster_side = d.pixels.size[0].max(d.pixels.size[1]);
        let texture = self
            .ctx
            .load_texture(path.to_string_lossy(), d.pixels, TEXTURE_OPTIONS);
        Cached {
            texture,
            original_size: d.original_size,
            file_size,
            raster_side,
        }
    }

    /// New SVG rasterisation: replaces the cached and displayed texture.
    fn on_svg(&mut self, path: PathBuf, result: Result<formats::Decoded, String>) {
        self.svg_inflight = None;
        if !self.loader.is_wanted(&path) {
            return;
        }
        if let Ok(d) = result {
            let cached = self.make_cached(&path, d);
            if let Some(display) = self.display.as_mut().filter(|d| d.path == path) {
                display.item = Ok(cached.clone());
            }
            self.cache.put(path, cached, 1);
        }
    }

    /// Initial rasterisation side of an SVG: the display area, rounded up.
    fn svg_hint(&self) -> usize {
        let ppp = self.ctx.pixels_per_point();
        let side = (self.last_avail.x.max(self.last_avail.y) * ppp).ceil() as usize;
        let side = if side == 0 { 2048 } else { side };
        (side.div_ceil(SVG_BUCKET) * SVG_BUCKET).min(self.max_side.min(MAX_SVG_SIDE))
    }

    /// Re-rasterises the displayed SVG if the zoom requires it, once the zoom has settled.
    fn sync_svg(&mut self, path: &Path, img: Vec2, zoom: f32, current_side: usize) {
        if formats::kind_of(path) != Some(Kind::Svg) {
            return;
        }
        let needed = (img.x.max(img.y) * zoom).ceil() as usize;
        let want =
            (needed.div_ceil(SVG_BUCKET).max(1) * SVG_BUCKET).min(self.max_side.min(MAX_SVG_SIDE));
        // Enlarge as soon as the texture is too small; only shrink by a factor >= 4.
        let stale = want > current_side || want * 4 < current_side;
        let inflight = self
            .svg_inflight
            .as_ref()
            .is_some_and(|(p, w)| p == path && *w == want);
        if !stale || inflight {
            self.svg_pending = None;
            return;
        }
        match &self.svg_pending {
            Some((p, w, since)) if p == path && *w == want => {
                let elapsed = since.elapsed();
                if elapsed >= SVG_DEBOUNCE {
                    self.loader.request_svg(path.to_path_buf(), want);
                    self.svg_inflight = Some((path.to_path_buf(), want));
                    self.svg_pending = None;
                } else {
                    self.ctx.request_repaint_after(SVG_DEBOUNCE - elapsed);
                }
            }
            _ => {
                self.svg_pending = Some((path.to_path_buf(), want, Instant::now()));
                self.ctx.request_repaint_after(SVG_DEBOUNCE);
            }
        }
    }

    /// A thumbnail texture is only created if it is still wanted.
    fn on_thumb(&mut self, path: PathBuf, result: Result<egui::ColorImage, String>) {
        if !self.loader.is_thumb_wanted(&path) || self.thumbs.contains(&path) {
            return;
        }
        match result {
            Ok(img) => {
                let weight = img.size[0] * img.size[1] * 4;
                let tex = self
                    .ctx
                    .load_texture(path.to_string_lossy(), img, TEXTURE_OPTIONS);
                self.thumbs.put(path, tex, weight);
            }
            Err(_) => {
                self.thumbs_failed.insert(path);
            }
        }
    }

    fn select(&mut self, id: Option<NodeId>) {
        if self.selected != id {
            self.selected = id;
            self.scroll_to_selected = true;
            self.retarget_needed = true;
        }
    }

    /// Re-roots the tree on the parent folder.
    fn go_up(&mut self) {
        let old = self.tree.nodes[Tree::ROOT].path.clone();
        let Some(parent) = old.parent() else { return };
        self.epoch += 1;
        self.tree = Tree::new(parent.to_path_buf());
        self.selected = None;
        self.display = None;
        self.retarget_needed = true;
        self.pending_select = Some(old);
        self.thumbs_dirty = true;
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

    /// Current folder: the selected folder, or the folder of the selected image.
    fn current_folder(&self) -> NodeId {
        match self.selected {
            Some(s) if self.tree.nodes[s].is_dir => s,
            Some(s) => self.tree.nodes[s].parent.unwrap_or(Tree::ROOT),
            None => Tree::ROOT,
        }
    }

    /// Images of the current folder, in display order.
    fn folder_images(&self) -> Vec<NodeId> {
        self.tree
            .visible_children(self.current_folder(), self.show_hidden)
            .into_iter()
            .filter(|&c| self.is_image(c))
            .collect()
    }

    fn step_image(&mut self, delta: isize) {
        let imgs = self.folder_images();
        if imgs.is_empty() {
            return;
        }
        let cur = self
            .selected
            .and_then(|s| imgs.iter().position(|&i| i == s));
        let next = match cur {
            Some(i) => (i as isize + delta).clamp(0, imgs.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => imgs.len() - 1,
        };
        self.select(Some(imgs[next]));
    }

    fn handle_keys(&mut self, ctx: &Context) {
        let rows = self.tree.visible_rows(self.show_hidden);
        let pos = self
            .selected
            .and_then(|s| rows.iter().position(|&r| r == s));
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
            action.quit = i.consume_key(Modifiers::NONE, Key::Q);
            action.tab = i.consume_key(Modifiers::NONE, Key::Tab);
            action.escape = i.consume_key(Modifiers::NONE, Key::Escape);
            // `?` needs Shift on many layouts, so accept it with or without the modifier.
            action.help = i.consume_key(Modifiers::NONE, Key::Questionmark)
                || i.consume_key(Modifiers::SHIFT, Key::Questionmark);
            action.fullscreen = i.consume_key(Modifiers::NONE, Key::F11);
            if i.consume_key(Modifiers::NONE, Key::F) {
                self.view_cmd = Some(ViewCmd::Fit);
            } else if i.consume_key(Modifiers::NONE, Key::Num1) {
                self.view_cmd = Some(ViewCmd::Actual);
            } else if i.consume_key(Modifiers::NONE, Key::Plus)
                || i.consume_key(Modifiers::SHIFT, Key::Plus)
                || i.consume_key(Modifiers::NONE, Key::Equals)
                || i.consume_key(Modifiers::SHIFT, Key::Equals)
            {
                self.view_cmd = Some(ViewCmd::Zoom(KEY_STEP));
            } else if i.consume_key(Modifiers::NONE, Key::Minus) {
                self.view_cmd = Some(ViewCmd::Zoom(1.0 / KEY_STEP));
            }
        });

        if action.fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if action.help {
            self.show_help = !self.show_help;
        }
        if action.escape && self.show_help {
            self.show_help = false;
        } else if action.escape && self.fullscreen {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        } else if action.quit || action.escape {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if action.tab {
            self.mode = match self.mode {
                Mode::Tree => Mode::Thumbs,
                Mode::Thumbs => Mode::Tree,
            };
            self.scroll_to_selected = true;
        }
        if action.dot {
            self.show_hidden = !self.show_hidden;
            self.retarget_needed = true;
            self.thumbs_dirty = true;
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
        if self.mode == Mode::Thumbs {
            self.grid_keys(&action);
            return;
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

    /// Arrow keys in the grid: left/right by one thumbnail, up/down by one row.
    fn grid_keys(&mut self, action: &Keys) {
        let imgs = self.folder_images();
        if imgs.is_empty() {
            return;
        }
        let cols = self.grid_cols.max(1);
        let cur = self
            .selected
            .and_then(|s| imgs.iter().position(|&i| i == s));
        let last = imgs.len() - 1;
        let next = match cur {
            None if action.left || action.right || action.up || action.down => 0,
            None => return,
            Some(i) if action.right => (i + 1).min(last),
            Some(i) if action.left => i.saturating_sub(1),
            Some(i) if action.down => {
                if i + cols <= last {
                    i + cols
                } else if i / cols < last / cols {
                    last
                } else {
                    i
                }
            }
            Some(i) if action.up => i.saturating_sub(cols),
            Some(_) => return,
        };
        self.select(Some(imgs[next]));
    }

    /// F5: re-reads the selection's folder (or the root).
    fn refresh_current_folder(&mut self) {
        let folder = self.current_folder();
        // The existing children become orphans in the arena: acceptable, since
        // the tree is rebuilt on every re-root.
        self.tree.nodes[folder].children = None;
        self.selected = Some(folder);
        self.display = None;
        self.cache.clear();
        self.failed.clear();
        self.thumbs.clear();
        self.thumbs_failed.clear();
        self.thumbs_dirty = true;
        self.retarget_needed = true;
        self.request_dir(folder);
    }

    /// Path of the selected image, if there is one.
    fn selected_image_path(&self) -> Option<PathBuf> {
        self.selected
            .filter(|&s| self.is_image(s))
            .map(|s| self.tree.nodes[s].path.clone())
    }

    /// Wanted targets: current image, then neighbours n±1 and n±2.
    fn targets(&self) -> Vec<(PathBuf, Priority)> {
        let Some(sel) = self.selected.filter(|&s| self.is_image(s)) else {
            return vec![];
        };
        let imgs = self.folder_images();
        let Some(idx) = imgs.iter().position(|&i| i == sel) else {
            return vec![];
        };
        let path = |i: usize| self.tree.nodes[imgs[i]].path.clone();
        let mut out = vec![(path(idx), Priority::Current)];
        for (d, prio) in [(1, Priority::Neighbor1), (2, Priority::Neighbor2)] {
            if idx + d < imgs.len() {
                out.push((path(idx + d), prio));
            }
            if idx >= d {
                out.push((path(idx - d), prio));
            }
        }
        out
    }

    /// Updates the loader requests and the displayed image.
    fn sync_images(&mut self, ctx: &Context) {
        self.max_side = ctx.input(|i| i.max_texture_side);
        if std::mem::take(&mut self.retarget_needed) {
            let targets = self.targets();
            // Keeps neighbours that are already ready "fresh" in the LRU.
            let ready: Vec<bool> = targets
                .iter()
                .map(|(p, _)| self.cache.get(p).is_some() || self.failed.contains_key(p))
                .collect();
            let known: std::collections::HashSet<&PathBuf> = targets
                .iter()
                .zip(&ready)
                .filter(|(_, r)| **r)
                .map(|((p, _), _)| p)
                .collect();
            let svg_hint = self.svg_hint();
            self.loader
                .retarget(&targets, self.max_side, svg_hint, |p| {
                    known.contains(&p.to_path_buf())
                });
        }
        let Some(path) = self.selected_image_path() else {
            self.display = None;
            return;
        };
        if self.display.as_ref().is_some_and(|d| d.path == path) {
            return;
        }
        let item = if let Some(c) = self.cache.get(&path) {
            Ok(c.clone())
        } else if let Some(e) = self.failed.get(&path) {
            Err(e.clone())
        } else {
            return;
        };
        self.display = Some(Display { path, item });
        self.view.set_fit();
        // Otherwise keep the previous image on screen until the new one arrives.
    }

    /// Issues thumbnail requests for the visible area (plus a margin).
    fn sync_thumbs(&mut self) {
        let key = (self.mode == Mode::Thumbs).then(|| (self.current_folder(), self.thumb_range));
        if key == self.thumb_key && !self.thumbs_dirty {
            return;
        }
        self.thumbs_dirty = false;
        self.thumb_key = key;
        let mut targets = Vec::new();
        if let Some((folder, (first, end))) = key {
            if self.tree.nodes[folder].children.is_none() {
                // Folder never read (selected without being expanded): load it.
                self.request_dir(folder);
            }
            let imgs = self.folder_images();
            let margin = self.grid_cols * THUMB_MARGIN_ROWS;
            let lo = first.saturating_sub(margin);
            let hi = (end + margin).min(imgs.len());
            for (i, &id) in imgs.iter().enumerate().take(hi).skip(lo) {
                let prio = if (first..end).contains(&i) {
                    Priority::Thumb
                } else {
                    Priority::Rest
                };
                targets.push((self.tree.nodes[id].path.clone(), prio));
            }
        }
        let (thumbs, failed) = (&mut self.thumbs, &self.thumbs_failed);
        // `get` refreshes the LRU for thumbnails that are already ready.
        let ready: HashSet<PathBuf> = targets
            .iter()
            .filter(|(p, _)| thumbs.get(p).is_some() || failed.contains(p))
            .map(|(p, _)| p.clone())
            .collect();
        self.loader.retarget_thumbs(&targets, |p| ready.contains(p));
    }

    /// Scrolls so that row `row` is visible, if requested.
    fn reveal(&mut self, mut area: ScrollArea, row: Option<usize>, row_h: f32) -> ScrollArea {
        if std::mem::take(&mut self.scroll_to_selected)
            && let Some(row) = row
        {
            let top = row as f32 * row_h;
            let bottom = top + row_h;
            if top < self.scroll_offset {
                area = area.vertical_scroll_offset(top);
            } else if bottom > self.scroll_offset + self.viewport_height {
                area = area.vertical_scroll_offset(bottom - self.viewport_height);
            }
        }
        area
    }

    fn tree_panel(&mut self, ui: &mut egui::Ui) {
        let rows = self.tree.visible_rows(self.show_hidden);
        let has_parent = self.tree.nodes[Tree::ROOT].path.parent().is_some();
        let up_rows = usize::from(has_parent);
        let total = rows.len() + up_rows;

        // No spacing between rows: the scroll computation relies on ROW_HEIGHT.
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let sel_row = self
            .selected
            .and_then(|s| rows.iter().position(|&r| r == s))
            .map(|p| p + up_rows);
        let area = self.reveal(
            ScrollArea::vertical().auto_shrink([false, false]),
            sel_row,
            ROW_HEIGHT,
        );

        let mut clicked: Option<Click> = None;
        let out = area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
            for i in range {
                let width = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
                if i < up_rows {
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(row_bg(rect), 4.0, Color32::from_white_alpha(10));
                    }
                    paint_label(ui, rect, 0, "..", TEXT_DIM);
                    if resp.clicked() {
                        clicked = Some(Click::Up);
                    }
                    continue;
                }
                let id = rows[i - up_rows];
                let node = &self.tree.nodes[id];
                let selected = self.selected == Some(id);
                if selected {
                    ui.painter().rect_filled(row_bg(rect), 4.0, SELECTION);
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(row_bg(rect), 4.0, Color32::from_white_alpha(10));
                }
                if node.is_dir {
                    paint_chevron(ui, rect, node.depth, node.expanded);
                }
                paint_icon(ui, rect, node.depth, node.is_dir, node.hidden);
                let color = if selected {
                    Color32::WHITE
                } else if node.is_dir {
                    FOLDER_TEXT
                } else if node.hidden {
                    TEXT_DIM
                } else {
                    TEXT_NORMAL
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

    fn thumbs_panel(&mut self, ui: &mut egui::Ui) {
        let folder = self.current_folder();
        let count = self.folder_images().len();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(&self.tree.nodes[folder].name)
                    .strong()
                    .color(TEXT_NORMAL),
            );
            ui.label(egui::RichText::new(count.to_string()).color(TEXT_DIM));
        });
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing = Vec2::ZERO;

        let imgs = self.folder_images();
        let cols = grid_columns(ui.available_width(), TARGET_CELL);
        let cell = (ui.available_width() / cols as f32).floor();
        self.grid_cols = cols;
        let total_rows = imgs.len().div_ceil(cols);
        let sel_row = self
            .selected
            .and_then(|s| imgs.iter().position(|&i| i == s))
            .map(|i| i / cols);
        let area = ScrollArea::vertical()
            .id_salt(("grid", folder))
            .auto_shrink([false, false]);
        let area = self.reveal(area, sel_row, cell);

        let mut clicked = None;
        let mut visible = (0, 0);
        let out = area.show_rows(ui, cell, total_rows, |ui, rows| {
            visible = (rows.start * cols, (rows.end * cols).min(imgs.len()));
            for row in rows {
                let (row_rect, _) =
                    ui.allocate_exact_size(vec2(cell * cols as f32, cell), Sense::hover());
                for col in 0..cols {
                    let idx = row * cols + col;
                    let Some(&id) = imgs.get(idx) else { break };
                    let rect = Rect::from_min_size(
                        row_rect.min + vec2(col as f32 * cell, 0.0),
                        vec2(cell, cell),
                    );
                    let node = &self.tree.nodes[id];
                    let resp = ui
                        .interact(rect, ui.scope_id().with(("cell", idx)), Sense::click())
                        .on_hover_text(&node.name);
                    if self.selected == Some(id) {
                        ui.painter().rect_filled(rect.shrink(2.0), 4.0, SELECTION);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(
                            rect.shrink(2.0),
                            4.0,
                            Color32::from_white_alpha(12),
                        );
                    }
                    let inner = rect.shrink(6.0);
                    if let Some(tex) = self.thumbs.get(&node.path) {
                        let size = tex.size_vec2();
                        let scale = (inner.width() / size.x)
                            .min(inner.height() / size.y)
                            .min(1.0);
                        ui.painter().image(
                            tex.id(),
                            Rect::from_center_size(inner.center(), size * scale),
                            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else if self.thumbs_failed.contains(&node.path) {
                        ui.painter().text(
                            inner.center(),
                            egui::Align2::CENTER_CENTER,
                            "×",
                            egui::FontId::proportional(18.0),
                            Color32::DARK_GRAY,
                        );
                    } else {
                        ui.painter().rect_filled(
                            inner.shrink(inner.width() * 0.12),
                            3.0,
                            Color32::from_white_alpha(8),
                        );
                    }
                    if resp.clicked() {
                        clicked = Some(id);
                    }
                }
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.viewport_height = out.inner_rect.height();
        if visible != self.thumb_range {
            self.thumb_range = visible;
            ui.ctx().request_repaint();
        }
        if clicked.is_some() {
            self.select(clicked);
        }
    }

    /// Status bar text: (left: file, right: position and zoom).
    fn status_parts(&self) -> (String, String) {
        let Some(d) = &self.display else {
            let folder = self.current_folder();
            let count = self.folder_images().len();
            let name = &self.tree.nodes[folder].name;
            let noun = if count > 1 { "images" } else { "image" };
            return (format!("{name}   ·   {count} {noun}"), String::new());
        };
        let name = d
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let imgs = self.folder_images();
        let pos = imgs
            .iter()
            .position(|&i| self.tree.nodes[i].path == d.path)
            .map(|p| format!("{} / {}", p + 1, imgs.len()))
            .unwrap_or_default();
        let loading = if self.selected_image_path().as_ref() == Some(&d.path) {
            ""
        } else {
            "   …"
        };
        match &d.item {
            Ok(c) => {
                let [w, h] = c.original_size;
                (
                    format!(
                        "{name}   ·   {w} × {h}   ·   {}{loading}",
                        human_size(c.file_size)
                    ),
                    format!("{}   ·   {pos}", self.zoom_label(c)),
                )
            }
            Err(_) => (format!("{name}{loading}"), pos),
        }
    }

    fn zoom_label(&self, c: &Cached) -> String {
        let ppp = self.ctx.pixels_per_point();
        let img = Vec2::new(c.original_size[0] as f32, c.original_size[1] as f32);
        let avail = self.last_avail;
        let z = self.view.zoom(img, avail * ppp);
        let mode = if self.view.is_fit() { " (fit)" } else { "" };
        format!("{:.0} %{mode}", z * 100.0)
    }

    fn image_panel(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, BG_IMAGE);
        self.last_avail = rect.size();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let Some(display) = &self.display else {
            paint_empty_state(ui, rect);
            return;
        };
        let cached = match &display.item {
            Ok(c) => c.clone(),
            Err(err) => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    err,
                    egui::FontId::proportional(14.0),
                    Color32::LIGHT_RED,
                );
                return;
            }
        };

        let ppp = ui.ctx().pixels_per_point();
        let img = Vec2::new(
            cached.original_size[0] as f32,
            cached.original_size[1] as f32,
        );
        let avail = rect.size() * ppp;
        // Cursor position relative to the area centre, in physical pixels.
        let cursor = resp
            .hover_pos()
            .map_or(Vec2::ZERO, |p| (p - rect.center()) * ppp);

        match self.view_cmd.take() {
            Some(ViewCmd::Fit) => self.view.set_fit(),
            Some(ViewCmd::Actual) => self.view.zoom_to(img, avail, Vec2::ZERO, 1.0),
            Some(ViewCmd::Zoom(f)) => self.view.zoom_by(img, avail, Vec2::ZERO, f),
            None => {}
        }
        if resp.hovered() {
            let (scroll, pinch) = ui
                .ctx()
                .input(|i| (i.smooth_scroll_delta().y, i.zoom_delta()));
            let factor = pinch * (scroll * WHEEL_SENSITIVITY).exp();
            if (factor - 1.0).abs() > f32::EPSILON {
                self.view.zoom_by(img, avail, cursor, factor);
            }
        }
        if resp.dragged() {
            self.view.pan(resp.drag_delta() * ppp, img, avail);
        }
        if resp.double_clicked() {
            self.view.toggle_actual(img, avail, cursor);
        }
        // The area size may have changed since the last frame.
        self.view.clamp(img, avail);

        let zoom = self.view.zoom(img, avail);
        if let Some(path) = self.display.as_ref().map(|d| d.path.clone()) {
            self.sync_svg(&path, img, zoom, cached.raster_side);
        }
        let size = img * zoom / ppp;
        let center = rect.center() + self.view.offset() / ppp;
        let mut img_rect = Rect::from_center_size(center, size);
        // Snap to the physical pixel grid for perfect sharpness at 100 %.
        let min = pos2(
            (img_rect.min.x * ppp).round() / ppp,
            (img_rect.min.y * ppp).round() / ppp,
        );
        img_rect = Rect::from_min_size(min, size);
        if !self.view.is_fit() && (img * zoom).max_elem() > avail.min_elem() {
            let icon = if resp.dragged() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            };
            ui.ctx().set_cursor_icon(icon);
        }
        ui.painter().with_clip_rect(rect).image(
            cached.texture.id(),
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
        self.sync_images(ctx);
        self.sync_thumbs();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.fullscreen {
            let (left, right) = self.status_parts();
            let status = Panel::bottom("status")
                .frame(
                    egui::Frame::new()
                        .fill(BG_PANEL)
                        .inner_margin(egui::Margin::symmetric(12, 0)),
                )
                .exact_size(28.0)
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.label(egui::RichText::new(left).size(12.5).color(TEXT_NORMAL));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(right).size(12.5).color(TEXT_DIM));
                        });
                    });
                });
            let rect = status.response.rect;
            ui.painter()
                .hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, DIVIDER));
            let side = Panel::left("tree")
                .frame(
                    egui::Frame::new()
                        .fill(BG_PANEL)
                        .inner_margin(egui::Margin::symmetric(2, 6)),
                )
                .resizable(true)
                .default_size(300.0)
                .show(ui, |ui| match self.mode {
                    Mode::Tree => self.tree_panel(ui),
                    Mode::Thumbs => self.thumbs_panel(ui),
                });
            let rect = side.response.rect;
            ui.painter().vline(
                rect.right(),
                rect.y_range(),
                egui::Stroke::new(1.0, DIVIDER),
            );
        }
        CentralPanel::default()
            .frame(egui::Frame::new().fill(BG_IMAGE))
            .show(ui, |ui| self.image_panel(ui));
        if self.show_help {
            help_window(ui.ctx());
        }
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
    escape: bool,
    tab: bool,
    fullscreen: bool,
    help: bool,
    next_image: bool,
    prev_image: bool,
}

/// Pending zoom command: applied in `image_panel`, which knows the area.
enum ViewCmd {
    Fit,
    Actual,
    Zoom(f32),
}

enum Click {
    Up,
    Select(NodeId),
    Toggle(NodeId),
}

/// Shortcut reference shown in the help window: (section, [(keys, action)]).
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("↑  ↓", "Previous / next item"),
            ("←  →", "Collapse / expand (tree), previous / next (grid)"),
            ("Enter", "Expand / collapse a folder"),
            ("Space  Shift+Space", "Next / previous image in the folder"),
            ("Tab", "Switch between tree and thumbnails"),
            ("Backspace", "Go to the parent folder"),
            (".", "Show / hide hidden files"),
            ("F5", "Re-read the current folder"),
        ],
    ),
    (
        "View",
        &[
            ("f", "Fit to window"),
            ("1", "Actual size (100 %)"),
            ("+  -", "Zoom in / out"),
            ("F11", "Fullscreen"),
        ],
    ),
    (
        "Mouse",
        &[
            ("Wheel", "Zoom around the cursor"),
            ("Drag", "Pan the image"),
            ("Double-click", "Toggle fit / 100 %"),
        ],
    ),
    (
        "General",
        &[("?", "Show / hide this help"), ("q  Esc", "Quit")],
    ),
];

/// Help window listing the keyboard and mouse shortcuts.
fn help_window(ctx: &Context) {
    egui::Window::new("help")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(
            egui::Frame::new()
                .fill(BG_PANEL)
                .stroke(egui::Stroke::new(1.0, DIVIDER))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::same(20))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(140),
                }),
        )
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("Keyboard & mouse")
                    .size(17.0)
                    .strong()
                    .color(Color32::WHITE),
            );
            for (section, rows) in SHORTCUTS {
                ui.add_space(12.0);
                ui.label(egui::RichText::new(*section).size(12.0).color(TEXT_DIM));
                ui.add_space(2.0);
                egui::Grid::new(("help", *section))
                    .num_columns(2)
                    .spacing([18.0, 6.0])
                    .show(ui, |ui| {
                        for (keys, action) in *rows {
                            ui.label(
                                egui::RichText::new(*keys)
                                    .monospace()
                                    .size(12.5)
                                    .color(Color32::WHITE)
                                    .background_color(Color32::from_rgb(50, 50, 60)),
                            );
                            ui.label(egui::RichText::new(*action).size(13.0).color(TEXT_NORMAL));
                            ui.end_row();
                        }
                    });
            }
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new("Press ? or Esc to close")
                    .size(12.0)
                    .color(TEXT_DIM),
            );
        });
}

/// Welcome screen: faded image icon and a reminder of the shortcuts.
fn paint_empty_state(ui: &egui::Ui, rect: Rect) {
    let p = ui.painter();
    let c = rect.center() + vec2(0.0, -24.0);
    let frame = Rect::from_center_size(c, vec2(64.0, 52.0));
    let dim = Color32::from_rgb(58, 58, 68);
    p.rect_stroke(
        frame,
        8.0,
        egui::Stroke::new(2.5, dim),
        egui::StrokeKind::Inside,
    );
    p.circle_filled(frame.min + vec2(46.0, 15.0), 5.0, dim);
    p.add(egui::Shape::convex_polygon(
        vec![
            frame.left_bottom() + vec2(6.0, -6.0),
            frame.left_bottom() + vec2(22.0, -26.0),
            frame.left_bottom() + vec2(34.0, -12.0),
            frame.left_bottom() + vec2(41.0, -19.0),
            frame.right_bottom() + vec2(-6.0, -6.0),
        ],
        dim,
        egui::Stroke::NONE,
    ));
    p.text(
        c + vec2(0.0, 54.0),
        egui::Align2::CENTER_CENTER,
        "Select an image",
        egui::FontId::proportional(15.0),
        Color32::from_rgb(120, 120, 132),
    );
    p.text(
        c + vec2(0.0, 78.0),
        egui::Align2::CENTER_CENTER,
        "?: help   Space: next   Tab: thumbnails   F11: fullscreen   q: quit",
        egui::FontId::proportional(12.0),
        Color32::from_rgb(86, 86, 98),
    );
}

/// Row background: slightly inset from the panel edges.
fn row_bg(rect: Rect) -> Rect {
    rect.shrink2(vec2(4.0, 1.0))
}

/// Label truncated with "…" rather than cut off at the panel edge.
fn paint_label(ui: &egui::Ui, rect: Rect, depth: usize, text: &str, color: Color32) {
    let x = rect.left() + 38.0 + depth as f32 * INDENT;
    let max_width = (rect.right() - 10.0 - x).max(0.0);
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::FontId::proportional(13.5),
        color,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    let galley = ui.painter().layout_job(job);
    let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, color);
}

/// Hand-drawn icon: folder (tab + body) or image (frame + mountain).
fn paint_icon(ui: &egui::Ui, rect: Rect, depth: usize, is_dir: bool, hidden: bool) {
    let c = pos2(rect.left() + 24.0 + depth as f32 * INDENT, rect.center().y);
    let p = ui.painter();
    if is_dir {
        let color = Color32::from_rgb(214, 178, 96);
        p.rect_filled(
            Rect::from_min_size(c + vec2(-6.0, -5.0), vec2(5.0, 3.0)),
            1.0,
            color,
        );
        p.rect_filled(
            Rect::from_min_size(c + vec2(-6.0, -3.0), vec2(12.0, 8.0)),
            1.5,
            color,
        );
    } else {
        let color = if hidden {
            Color32::from_gray(90)
        } else {
            Color32::from_rgb(110, 150, 210)
        };
        let frame = Rect::from_min_size(c + vec2(-5.5, -5.5), vec2(11.0, 11.0));
        p.rect_filled(frame, 2.0, color.gamma_multiply(0.35));
        p.add(egui::Shape::convex_polygon(
            vec![
                frame.left_bottom() + vec2(1.0, -1.0),
                frame.left_bottom() + vec2(4.5, -6.0),
                frame.left_bottom() + vec2(7.0, -3.0),
                frame.left_bottom() + vec2(8.5, -4.5),
                frame.right_bottom() + vec2(-1.0, -1.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
    }
}

/// Dark theme: neutral surfaces, no needless borders, slightly rounded corners.
fn apply_theme(ctx: &Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_PANEL;
    v.extreme_bg_color = BG_IMAGE;
    v.window_stroke = egui::Stroke::NONE;
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = egui::Stroke::NONE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_stroke = egui::Stroke::NONE;
        w.corner_radius = egui::CornerRadius::same(4);
    }
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| s.spacing.scroll = egui::style::ScrollStyle::thin());
}

fn paint_chevron(ui: &egui::Ui, rect: Rect, depth: usize, open: bool) {
    let c = pos2(rect.left() + 8.0 + depth as f32 * INDENT, rect.center().y);
    let pts = if open {
        vec![
            c + vec2(-3.5, -2.0),
            c + vec2(3.5, -2.0),
            c + vec2(0.0, 3.0),
        ]
    } else {
        vec![
            c + vec2(-2.0, -3.5),
            c + vec2(3.0, 0.0),
            c + vec2(-2.0, 3.5),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        pts,
        Color32::GRAY,
        egui::Stroke::NONE,
    ));
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(20 * 1024 * 1024), "20.0 MB");
    }
}
