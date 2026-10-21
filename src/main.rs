mod cache;
mod scan;

use eframe::egui;
use scan::{Node, Scanner, Space, human};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Instant;

/// Maximum number of subdirectories shown under one entry.
const MAX_CHILDREN: usize = 500;
const BAR_WIDTH: f32 = 160.0;

fn main() -> eframe::Result {
    let root = std::env::args().nth(1).unwrap_or_else(|| "/".to_string());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "diskuse",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc.egui_ctx.clone(), root)))),
    )
}

/// A finished scan, sent from a worker thread back to the UI.
struct ScanResult {
    generation: u64,
    path: PathBuf,
    node: Node,
    status: String,
}

struct App {
    ctx: egui::Context,
    path_input: String,
    root_path: PathBuf,
    root: Option<Node>,
    /// Incremented on every full scan, so results of stale scans are dropped.
    generation: u64,
    /// Scanner of the running full scan, for progress display.
    full_scan: Option<Arc<Scanner>>,
    /// Subdirectories currently being refreshed.
    refreshing: HashSet<PathBuf>,
    /// Summary of the last finished scan.
    status: String,
    /// Directories smaller than this percentage of the whole scanned tree
    /// are hidden; 0 shows everything.
    min_percent: f64,
    /// Used and free space of the volumes shown, refreshed after each scan.
    volumes: RefCell<HashMap<PathBuf, Option<Space>>>,
    tx: mpsc::Sender<ScanResult>,
    rx: mpsc::Receiver<ScanResult>,
}

impl App {
    fn new(ctx: egui::Context, root: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            ctx,
            path_input: root,
            root_path: PathBuf::new(),
            root: None,
            generation: 0,
            full_scan: None,
            refreshing: HashSet::new(),
            status: String::new(),
            min_percent: 0.1,
            volumes: RefCell::default(),
            tx,
            rx,
        };
        app.start_full_scan(true);
        app
    }

    /// Scans the directory in the input field, starting from the cache
    /// in ~/.diskuse.json unless `use_cache` is false.
    fn start_full_scan(&mut self, use_cache: bool) {
        self.generation += 1;
        self.root_path = expand_tilde(&self.path_input);
        self.root = None;
        self.refreshing.clear();
        let scanner = Arc::new(Scanner::new());
        self.full_scan = Some(scanner.clone());
        let path = self.root_path.clone();
        self.spawn_scan(scanner, path.clone(), move || {
            if use_cache { cache::load(&path) } else { None }
        });
    }

    /// Rescans one directory, reading again only the directories whose
    /// modification time changed, or all of them if `force` is true.
    fn start_refresh(&mut self, path: PathBuf, force: bool) {
        if !self.refreshing.insert(path.clone()) {
            return;
        }
        let baseline = match (&self.root, force) {
            (Some(root), false) => scan::find(root, &self.root_path, &path).cloned(),
            _ => None,
        };
        self.spawn_scan(Arc::new(Scanner::new()), path, move || baseline);
    }

    fn spawn_scan(
        &self,
        scanner: Arc<Scanner>,
        path: PathBuf,
        baseline: impl FnOnce() -> Option<Node> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let generation = self.generation;
        thread::spawn(move || {
            let start = Instant::now();
            let baseline = baseline();
            let node = scanner.scan(&path, baseline.as_ref());
            let status = format!(
                "{}: {} directories, {} unchanged, {:.1}s",
                path.display(),
                scanner.dirs.load(Ordering::Relaxed),
                scanner.reused.load(Ordering::Relaxed),
                start.elapsed().as_secs_f32()
            );
            let saved = node.clone();
            let _ = tx.send(ScanResult {
                generation,
                path: path.clone(),
                node,
                status,
            });
            ctx.request_repaint();
            if let Err(e) = cache::store(&path, saved) {
                eprintln!("diskuse: cannot save cache: {e}");
            }
        });
    }

    fn receive_results(&mut self) {
        while let Ok(result) = self.rx.try_recv() {
            if result.generation != self.generation {
                continue;
            }
            self.status = result.status;
            self.volumes.borrow_mut().clear();
            if result.path == self.root_path && self.full_scan.is_some() {
                self.root = Some(result.node);
                self.full_scan = None;
                continue;
            }
            self.refreshing.remove(&result.path);
            if let Some(root) = &mut self.root {
                scan::replace(root, &self.root_path, &result.path, result.node);
            }
        }
    }

    fn volume_space(&self, path: &Path) -> Option<Space> {
        *self
            .volumes
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_insert_with(|| scan::volume_space(path))
    }

    /// Size of the whole scanned tree.
    fn total(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.size)
    }

    fn is_visible(&self, size: u64) -> bool {
        let total = self.total();
        self.min_percent <= 0.0 || size as f64 > total as f64 * self.min_percent / 100.0
    }

    /// Draws one directory and, if expanded, its subdirectories.
    /// Returns the path of a directory whose refresh button was clicked,
    /// and whether shift was held (rescan ignoring the cache).
    fn show_node(
        &self,
        ui: &mut egui::Ui,
        node: &Node,
        path: &Path,
        parent_size: u64,
    ) -> Option<(PathBuf, bool)> {
        let mut clicked = None;
        let row = |ui: &mut egui::Ui, clicked: &mut Option<(PathBuf, bool)>| {
            // The top level and mounted volumes show the volume's free space
            // instead of their share of the parent.
            let space = (path == self.root_path || node.mount)
                .then(|| self.volume_space(path))
                .flatten();
            match space {
                Some(space) => space_bar(ui, space),
                None => size_bar(ui, node.size, parent_size, self.total()),
            }
            ui.monospace(format!("{:>10}", human(node.size)));
            if self.refreshing.contains(path) {
                ui.spinner();
            } else if ui
                .small_button("⟳")
                .on_hover_text("Rescan changed directories\nShift-click: rescan everything")
                .clicked()
            {
                let force = ui.input(|i| i.modifiers.shift);
                *clicked = Some((path.to_path_buf(), force));
            }
            if ui
                .small_button("👁")
                .on_hover_text("Reveal in Finder")
                .clicked()
            {
                reveal(path);
            }
            ui.label(&node.name);
            if node.error {
                ui.colored_label(ui.visuals().warn_fg_color, "⚠")
                    .on_hover_text("Some entries could not be read");
            }
        };

        if node.children.is_empty() {
            ui.horizontal(|ui| {
                // Keep leaves aligned with the collapsible rows.
                ui.add_space(ui.spacing().indent);
                row(ui, &mut clicked);
            });
            return clicked;
        }

        let id = ui.make_persistent_id(path);
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            id,
            path == self.root_path,
        )
        .show_header(ui, |ui| row(ui, &mut clicked))
        .body(|ui| {
            // Children are sorted by size, so the visible ones come first.
            let visible = node
                .children
                .iter()
                .take_while(|c| self.is_visible(c.size))
                .count();
            for child in &node.children[..visible.min(MAX_CHILDREN)] {
                let child_path = path.join(&child.name);
                if let Some(p) = self.show_node(ui, child, &child_path, node.size) {
                    clicked = Some(p);
                }
            }
            if visible > MAX_CHILDREN {
                ui.weak(format!("… {} more directories", visible - MAX_CHILDREN));
            }
            if visible < node.children.len() {
                let hidden = &node.children[visible..];
                ui.weak(format!(
                    "… {} smaller directories hidden ({})",
                    hidden.len(),
                    human(hidden.iter().map(|c| c.size).sum())
                ));
            }
            if node.files_count > 0 {
                ui.horizontal(|ui| {
                    ui.add_space(ui.spacing().indent);
                    size_bar(ui, node.files_size, node.size, self.total());
                    ui.monospace(format!("{:>10}", human(node.files_size)));
                    ui.weak(format!("({} files)", node.files_count));
                });
            }
        });
        clicked
    }
}

/// Draws a bar split between used (red) and free (green) volume space.
fn space_bar(ui: &mut egui::Ui, space: Space) {
    let total = space.used + space.free;
    let used = if total == 0 {
        0.0
    } else {
        space.used as f32 / total as f32
    };
    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(BAR_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    let split = rect.left() + rect.width() * used;
    painter.rect_filled(
        rect.with_min_x(split),
        2.0,
        egui::Color32::from_rgb(46, 139, 87),
    );
    painter.rect_filled(
        rect.with_max_x(split),
        2.0,
        egui::Color32::from_rgb(190, 50, 50),
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!(
            "used {:.0}% · free {:.0}%",
            used * 100.0,
            (1.0 - used) * 100.0
        ),
        egui::FontId::proportional(height * 0.8),
        egui::Color32::WHITE,
    );
    response.on_hover_text(format!(
        "Volume: {} used, {} free of {}",
        human(space.used),
        human(space.free),
        human(total)
    ));
}

/// Draws a bar showing `size` as a fraction of `total` (the parent),
/// labelled "relative% (absolute%)" where absolute is the fraction of
/// `grand_total` (the whole scanned tree).
fn size_bar(ui: &mut egui::Ui, size: u64, total: u64, grand_total: u64) {
    let ratio = |of: u64| {
        if of == 0 {
            0.0
        } else {
            size as f32 / of as f32
        }
    };
    let fraction = ratio(total);
    let absolute = ratio(grand_total);
    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(BAR_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let mut filled = rect;
    filled.set_width(rect.width() * fraction);
    painter.rect_filled(filled, 2.0, ui.visuals().selection.bg_fill);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{:.1}% ({:.1}%)", fraction * 100.0, absolute * 100.0),
        egui::FontId::proportional(height * 0.8),
        ui.visuals().text_color(),
    );
    response.on_hover_text(format!(
        "{:.1}% of the parent ({})\n{:.1}% of the whole scan ({})",
        fraction * 100.0,
        human(total),
        absolute * 100.0,
        human(grand_total)
    ));
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_results();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Directory:");
                let input = ui.text_edit_singleline(&mut self.path_input);
                let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui
                    .button("Scan")
                    .on_hover_text("Rescan changed directories")
                    .clicked()
                    || enter
                {
                    self.start_full_scan(true);
                }
                if ui
                    .button("Full rescan")
                    .on_hover_text("Ignore the cache")
                    .clicked()
                {
                    self.start_full_scan(false);
                }
                ui.separator();
                ui.label("Show directories bigger than:");
                ui.add(
                    egui::DragValue::new(&mut self.min_percent)
                        .range(0.0..=100.0)
                        .speed(0.01)
                        .max_decimals(2)
                        .suffix(" %"),
                )
                .on_hover_text("Percentage of the whole scanned directory; 0 shows everything");
            });
            ui.weak(&self.status);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(scanner) = &self.full_scan {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Scanning {}… {} directories, {} unchanged",
                        self.root_path.display(),
                        scanner.dirs.load(Ordering::Relaxed),
                        scanner.reused.load(Ordering::Relaxed)
                    ));
                });
                // Keep the progress counter moving.
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            let Some(root) = &self.root else { return };
            let mut refresh = None;
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                refresh = self.show_node(ui, root, &self.root_path, root.size);
            });
            if let Some((path, force)) = refresh {
                self.start_refresh(path, force);
            }
        });
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    match (input.strip_prefix('~'), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", home.to_string_lossy()))
        }
        _ => PathBuf::from(input),
    }
}

/// Shows `path` in the system file manager, selected where supported.
fn reveal(path: &Path) {
    use std::process::Command;
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path);
        Command::new("explorer").arg(select).spawn()
    } else {
        Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn()
    };
    match result {
        // Reap the child so it does not linger as a zombie.
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("diskuse: cannot reveal {}: {e}", path.display()),
    }
}
