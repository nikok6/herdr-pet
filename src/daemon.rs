//! The pet daemon: one pet image per target pane, drawn through the pane's own
//! terminal output and animated from herdr agent status events. Single-threaded
//! event loop over an mpsc channel fed by socket reader threads, the signal
//! thread, and the drag tap.
use crate::config::{Mode, PetConfig};
use crate::socket;
use crate::sprites::{self, Pet, Row, CELL_H, CELL_W};
use crate::tty::{OpenError, PaneTty, WinSize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Msg {
    /// Pushed event envelope from the global subscription.
    Event(Value),
    EventsClosed(Option<String>),
    /// Pushed event from the per-pane status subscription (generation-tagged).
    StatusClosed(u64),
    ReloadConfig,
    /// A background pet load finished (tagged with the load generation that requested it).
    PetLoaded { gen: u64, result: Result<(Option<Pet>, Grid), String> },
    Shutdown,
    // Only the macOS event tap constructs the drag messages (enum-level allow above).
    DragStart,
    Drag { dx: f64, dy: f64 },
    DragEnd,
    DragInfo(String),
}

pub fn log(msg: impl AsRef<str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    println!("{h:02}:{m:02}:{s:02}.{:03} {}", now.subsec_millis(), msg.as_ref());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl Status {
    fn parse(s: &str) -> Status {
        match s {
            "idle" => Status::Idle,
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            "done" => Status::Done,
            _ => Status::Unknown,
        }
    }
    fn row(self) -> Row {
        match self {
            Status::Idle | Status::Unknown => Row::Idle,
            Status::Working => Row::Running,
            Status::Blocked => Row::Waiting,
            Status::Done => Row::Review,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Done => "done",
            Status::Unknown => "unknown",
        }
    }
}

/// Host cell size in pixels; the pet's width in cells follows from it.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Grid {
    pub cols: u32,
    pub rows: u32,
}

/// Width in cells follows the 192x208 cell aspect at the host cell size.
fn grid_for(rows_wanted: u32, cell: Cell) -> Grid {
    let rows = rows_wanted.max(2);
    let cols = (rows as f64 * cell.height_px as f64 * CELL_W as f64 / (CELL_H as f64 * cell.width_px.max(1) as f64)).ceil() as u32;
    Grid { cols: cols.max(2), rows }
}

/// Resolve + size + decode a pet for `rows` terminal rows (None when disabled).
fn load_for(name: &str, rows: u32, cell: Cell, quantize: bool) -> Result<(Option<Pet>, Grid), Box<dyn std::error::Error>> {
    if name.is_empty() {
        return Err("no pet configured — pick one in the settings popover".into());
    }
    let dir = sprites::resolve_pet(name).ok_or_else(|| format!("no pet named {name:?} (see `herdr-pet list`)"))?;
    let grid = grid_for(rows, cell);
    let t0 = Instant::now();
    let pet = sprites::load_pet(&dir, Some(grid.rows * cell.height_px), quantize)?;
    log(format!(
        "loaded {} from {} @ {}x{}px ({:.0}ms)",
        pet.manifest.label(),
        dir.display(),
        pet.width,
        pet.height,
        t0.elapsed().as_secs_f64() * 1000.0
    ));
    Ok((Some(pet), grid))
}

/// Frame holds for `row`: the pet's own, or the Codex table while no pet is loaded.
fn durations(shared: &Shared, row: Row) -> &[u64] {
    match &shared.pet {
        Some(pet) => pet.durations(row),
        None => row.durations(),
    }
}

/// Frame duration after the configured speed multiplier (clamped to keep it sane).
fn frame_duration(ms: u64, cfg: &PetConfig) -> Duration {
    let speed = if cfg.speed.is_finite() { cfg.speed.clamp(0.25, 4.0) } else { 1.0 };
    Duration::from_millis(((ms as f64) / speed).round().max(16.0) as u64)
}

/// Live pet/config/grid shared by every instance; replaced in place on reload.
pub struct Shared {
    /// None while `pet = "disabled"`.
    pub pet: Option<Pet>,
    pub cfg: PetConfig,
    pub grid: Grid,
    pub cell: Cell,
}

struct PaneInfo {
    pane_id: String,
    focused: bool,
    agent: Option<String>,
    status: Status,
}

impl PaneInfo {
    fn from_value(v: &Value) -> Option<PaneInfo> {
        Some(PaneInfo {
            pane_id: v["pane_id"].as_str()?.to_owned(),
            focused: v["focused"].as_bool().unwrap_or(false),
            agent: v["agent"].as_str().map(str::to_owned),
            status: Status::parse(v["agent_status"].as_str().unwrap_or("unknown")),
        })
    }
}

/// Where the pet is drawn inside a pane, in cells from the pane's top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Placement {
    col: i32,
    row: i32,
    cols: u32,
    rows: u32,
}

/// Default: bottom-right corner with a one-cell margin, or the configured/dragged
/// cell; always inside the pane.
fn placement_for(shared: &Shared, drag: (i32, i32), ws: WinSize) -> Placement {
    let (inner_w, inner_h) = (ws.cols as i32, ws.rows as i32);
    let (cols, rows) = (shared.grid.cols as i32, shared.grid.rows as i32);
    let [base_col, base_row] = shared.cfg.position.unwrap_or([inner_w - cols - 1, inner_h - rows - 1]);
    Placement {
        col: (base_col + drag.0).clamp(0, (inner_w - cols).max(0)),
        row: (base_row + drag.1).clamp(0, (inner_h - rows).max(0)),
        cols: shared.grid.cols,
        rows: shared.grid.rows,
    }
}

struct PetInstance {
    pane_id: String,
    /// The pane's pty while attached; frames are written to it.
    tty: Option<PaneTty>,
    retry_at: Option<Instant>,
    row: Row,
    frame: usize,
    next_at: Instant,
    /// Row that plays once before returning to `base`.
    one_shot: Option<Row>,
    base: Row,
    /// Where the last frame was drawn.
    placement: Placement,
    /// A frame is on screen at `placement`.
    drawn: bool,
    /// Live drag displacement in cells; folded into margins on release.
    drag: (i32, i32),
    /// False until the pane is known to be on screen; nothing is sent before that.
    placed: bool,
    /// herdr reported the pane id as unknown; the daemon removes the instance.
    gone: bool,
    status: Status,
    /// In the active tab of the focused workspace; otherwise nothing is drawn.
    visible: bool,
    /// Loop the animation (vs. hold a static frame that only changes with status).
    animated: bool,
    last_focus: Instant,
}

/// Standing still, the pet is redrawn this often so it keeps its spot while the pane scrolls.
const KEEPALIVE: Duration = Duration::from_secs(2);

impl Drop for PetInstance {
    /// herdr cannot know who owns an image a pane's output created, so the pet
    /// takes itself off the screen when its instance goes away.
    fn drop(&mut self) {
        if let Some(tty) = self.tty.as_mut() {
            tty.clear();
        }
    }
}

impl PetInstance {
    fn new(pane_id: String) -> PetInstance {
        PetInstance {
            pane_id,
            tty: None,
            retry_at: None,
            row: Row::Idle,
            frame: 0,
            next_at: Instant::now(),
            one_shot: None,
            base: Row::Idle,
            placement: Placement::default(),
            drawn: false,
            drag: (0, 0),
            placed: false,
            gone: false,
            status: Status::Unknown,
            visible: true,
            animated: true,
            last_focus: Instant::now(),
        }
    }

    fn set_visible(&mut self, visible: bool, animated: bool) {
        if (visible && !self.visible) || (animated && !self.animated) {
            // Resume right away instead of waiting out a keep-alive interval.
            self.next_at = Instant::now();
        }
        self.visible = visible;
        self.animated = animated;
        if !animated && self.one_shot.take().is_some() {
            // A static pet cannot finish a one-shot; settle on the base row.
            self.row = self.base;
            self.frame = 0;
        }
    }

    fn start(&mut self, shared: &Shared) {
        if shared.cfg.transitions {
            self.play(Row::Waving, Some(Row::Idle), shared);
        }
    }

    fn attach(&mut self) {
        if self.tty.is_some() {
            return;
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        match PaneTty::open(&self.pane_id) {
            Ok(mut tty) => {
                // A daemon that died without cleaning up may have left a frame behind.
                tty.clear();
                self.tty = Some(tty);
                self.retry_at = None;
            }
            Err(e) => {
                if matches!(e, OpenError::Gone) {
                    // The pane id no longer exists (closed, or moved to another
                    // workspace, which re-ids it without a pane_closed event).
                    self.gone = true;
                }
                log(format!("pty {} open failed: {e}", self.pane_id));
                self.retry_at = Some(Instant::now() + Duration::from_secs(2));
            }
        }
    }

    fn set_status(&mut self, status: Status, shared: &Shared) {
        if status == self.status {
            return;
        }
        let prev = self.status;
        self.status = status;
        let base = status.row();
        let transitions = shared.cfg.transitions;
        if transitions && status == Status::Done && prev != Status::Done {
            self.play(Row::Jumping, Some(base), shared);
        } else if transitions && status == Status::Blocked && prev != Status::Blocked {
            self.play(Row::Waving, Some(base), shared);
        } else {
            self.play(base, None, shared);
        }
    }

    /// Drag in progress: face and "run" in the direction of travel (Codex/Orca behaviour).
    fn set_drag_row(&mut self, row: Row, shared: &Shared) {
        if self.row != row {
            self.play(row, None, shared);
        }
    }

    /// Drag released: back to the row for the current agent status.
    fn restore_status_row(&mut self, shared: &Shared) {
        let base = self.status.row();
        self.play(base, None, shared);
    }

    fn set_exited(&mut self, shared: &Shared) {
        self.status = Status::Unknown;
        self.play(Row::Failed, None, shared);
    }

    fn play(&mut self, row: Row, then_base: Option<Row>, shared: &Shared) {
        // Static pets skip one-shots (they could never finish them) and jump to the base row.
        let (row, then_base) = if self.animated { (row, then_base) } else { (then_base.unwrap_or(row), None) };
        match then_base {
            Some(base) => {
                self.one_shot = Some(row);
                self.base = base;
            }
            None => {
                self.one_shot = None;
                self.base = row;
            }
        }
        if self.row != row {
            log(format!(
                "{}: {} → {}{}",
                self.pane_id,
                self.status.name(),
                row.name(),
                then_base.map(|b| format!(" (then {})", b.name())).unwrap_or_default()
            ));
            self.row = row;
            self.frame = 0;
            self.next_at = Instant::now() + frame_duration(durations(shared, row)[0], &shared.cfg);
            self.send_frame(shared);
        }
    }

    /// The pane is on screen: draw (or move) the pet.
    fn set_placed(&mut self, shared: &Shared) {
        self.placed = true;
        self.apply_placement(shared);
    }

    fn apply_placement(&mut self, shared: &Shared) {
        // Layout events fire continuously while a split is dragged; only redraw
        // when the spot actually changed.
        if self.drawn {
            let ws = self.tty.as_ref().and_then(|t| t.winsize().ok());
            if ws.is_some_and(|ws| placement_for(shared, self.drag, ws) == self.placement) {
                return;
            }
        }
        self.send_frame(shared);
    }

    fn send_frame(&mut self, shared: &Shared) {
        let Some(pet) = &shared.pet else { return };
        if !self.placed || !self.visible {
            return;
        }
        self.attach();
        let Some(tty) = self.tty.as_mut() else { return };
        let placement = match tty.winsize() {
            Ok(ws) => placement_for(shared, self.drag, ws),
            Err(e) => {
                log(format!("pty {} size failed: {e}", self.pane_id));
                self.tty = None;
                self.drawn = false;
                self.retry_at = Some(Instant::now() + Duration::from_millis(500));
                return;
            }
        };
        let png = pet.frame(self.row, self.frame);
        match tty.draw(png, placement.col as u32, placement.row as u32, placement.cols, placement.rows) {
            Ok(()) => {
                self.placement = placement;
                self.drawn = true;
            }
            Err(e) => {
                log(format!("pty {} write failed: {e}", self.pane_id));
                self.tty = None;
                self.drawn = false;
                self.retry_at = Some(Instant::now() + Duration::from_millis(500));
            }
        }
    }

    /// Advance the animation when the current frame's duration has elapsed.
    /// A pet that is hidden is left alone; one standing still is redrawn at its
    /// spot now and then, because the pane's text may have scrolled under it.
    fn tick(&mut self, now: Instant, shared: &Shared) {
        // A due re-attach must run even between frames, or the loop spins on it.
        if self.tty.is_none() && self.retry_at.is_some_and(|at| now >= at) {
            self.send_frame(shared);
        }
        if now < self.next_at {
            return;
        }
        if !self.visible || !self.animated {
            self.next_at = now + KEEPALIVE;
            self.send_frame(shared);
            return;
        }
        let len = durations(shared, self.row).len();
        self.frame += 1;
        if self.frame >= len {
            self.frame = 0;
            if self.one_shot.take().is_some() {
                self.row = self.base;
            }
        }
        let holds = durations(shared, self.row);
        self.next_at = now + frame_duration(holds[self.frame % holds.len()], &shared.cfg);
        self.send_frame(shared);
    }

    fn drag_by(&mut self, dcols: i32, drows: i32, shared: &Shared) {
        self.drag.0 += dcols;
        self.drag.1 += drows;
        self.apply_placement(shared);
    }

    /// Drag released: persist the final cell as `position`.
    fn drag_end(&mut self, shared: &mut Shared) {
        let p = self.placement;
        shared.cfg.position = Some([p.col, p.row]);
        self.drag = (0, 0);
        self.apply_placement(shared);
        // Persist onto the current file contents (the popover may have changed
        // other keys meanwhile); never write over a file that does not parse.
        match PetConfig::load() {
            Ok(mut on_disk) => {
                on_disk.position = shared.cfg.position;
                if let Err(e) = on_disk.save() {
                    log(format!("config save failed: {e}"));
                }
            }
            Err(e) => log(format!("config: {e}; position not saved")),
        }
        log(format!("{}: dragged to col {} row {}", self.pane_id, p.col, p.row));
    }
}

pub struct Daemon {
    shared: Shared,
    tx: Sender<Msg>,
    instances: HashMap<String, PetInstance>,
    order: Vec<String>,
    focused: Option<String>,
    reconcile_at: Option<Instant>,
    /// `pane.agent_status_changed` is a per-pane subscription; rebuilt when the target set changes.
    status_sub: Option<socket::Connection>,
    status_gen: u64,
    status_key: String,
    drag_target: Option<String>,
    drag_acc: (f64, f64),
    /// Horizontal travel since the last direction decision; ±4pt flips the running row.
    drag_dir_acc: f64,
    /// Bumped per requested pet load so a stale background result is ignored.
    load_gen: u64,
    load_in_flight: bool,
    /// Latest request that arrived while a load was running (key repeat on Size/Pet).
    load_pending: Option<(String, u32, bool)>,
    /// When a load failed and nothing is shown, try again at this time.
    load_retry_at: Option<Instant>,
}

impl Daemon {
    fn new(shared: Shared, tx: Sender<Msg>) -> Daemon {
        Daemon {
            shared,
            tx,
            instances: HashMap::new(),
            order: Vec::new(),
            focused: None,
            reconcile_at: None,
            status_sub: None,
            status_gen: 0,
            status_key: String::new(),
            drag_target: None,
            drag_acc: (0.0, 0.0),
            drag_dir_acc: 0.0,
            load_gen: 0,
            load_in_flight: false,
            load_pending: None,
            load_retry_at: None,
        }
    }

    fn schedule_reconcile(&mut self) {
        if self.reconcile_at.is_none() {
            self.reconcile_at = Some(Instant::now() + Duration::from_millis(40));
        }
    }

    fn reconcile(&mut self) {
        self.reconcile_at = None;
        let t0 = Instant::now();
        if self.shared.pet.is_none() {
            self.instances.clear();
            self.order.clear();
            self.resubscribe_status();
            return;
        }
        // Panes that must have a pet right now, with their current status.
        let panes: Vec<PaneInfo> = match self.shared.cfg.mode {
            Mode::Agents => match socket::request("pane.list", json!({})) {
                Ok(res) => res["panes"]
                    .as_array()
                    .map(|a| a.iter().filter_map(PaneInfo::from_value).filter(|p| p.agent.is_some() || p.focused).collect())
                    .unwrap_or_default(),
                Err(e) => {
                    log(format!("reconcile: {e}; retrying"));
                    self.reconcile_at = Some(Instant::now() + Duration::from_secs(1));
                    return;
                }
            },
            Mode::All => match socket::request("pane.current", json!({})) {
                Ok(res) => PaneInfo::from_value(&res["pane"]).into_iter().collect(),
                Err(e) if matches!(e.code(), "not_found" | "pane_not_found") => Vec::new(),
                Err(e) => {
                    log(format!("reconcile: {e}; retrying"));
                    self.reconcile_at = Some(Instant::now() + Duration::from_secs(1));
                    return;
                }
            },
        };
        if let Some(f) = panes.iter().find(|p| p.focused) {
            self.focused = Some(f.pane_id.clone());
        }
        let now = Instant::now();
        for info in &panes {
            if !self.instances.contains_key(&info.pane_id) {
                let mut inst = PetInstance::new(info.pane_id.clone());
                inst.start(&self.shared);
                self.instances.insert(info.pane_id.clone(), inst);
                self.order.push(info.pane_id.clone());
            }
            let inst = self.instances.get_mut(&info.pane_id).expect("inserted");
            if info.focused {
                inst.last_focus = now;
            }
            inst.set_status(info.status, &self.shared);
        }
        // `all` keeps a few recently focused panes warm (for instant switch-back);
        // `agents` keeps every agent pane.
        let keep: HashSet<String> = match self.shared.cfg.mode {
            Mode::Agents => panes.iter().map(|p| p.pane_id.clone()).collect(),
            Mode::All => {
                let mut by_recency: Vec<(&String, Instant)> =
                    self.instances.iter().map(|(id, i)| (id, i.last_focus)).collect();
                by_recency.sort_by_key(|e| std::cmp::Reverse(e.1));
                by_recency.iter().take(self.shared.cfg.warm_panes.max(1)).map(|(id, _)| (*id).clone()).collect()
            }
        };
        self.instances.retain(|id, _| keep.contains(id));
        self.order.retain(|id| keep.contains(id));
        self.refresh_visibility();
        self.resubscribe_status();
        if std::env::var_os("PET_TRACE").is_some() {
            log(format!("trace: reconcile {} pane(s), {} warm, in {:.1}ms", panes.len(), self.instances.len(), t0.elapsed().as_secs_f64() * 1000.0));
        }
    }

    /// One `pane.layout` for the focused pane's tab says which panes are on
    /// screen: those instances get drawn, everything else is hidden and left
    /// alone (it is redrawn when it becomes visible again).
    fn refresh_visibility(&mut self) {
        let mut on_screen: HashSet<String> = HashSet::new();
        if let Some(id) = &self.focused {
            match socket::request("pane.layout", json!({ "pane_id": id })) {
                Ok(res) => {
                    // A zoomed pane covers the rest of the tab.
                    let zoomed = res["layout"]["zoomed"].as_bool().unwrap_or(false);
                    for p in res["layout"]["panes"].as_array().into_iter().flatten() {
                        if let Some(pid) = p["pane_id"].as_str().filter(|pid| !zoomed || *pid == id) {
                            on_screen.insert(pid.to_owned());
                        }
                    }
                }
                Err(e) => {
                    // Keep the current placements rather than hiding everything.
                    log(format!("layout: {e}"));
                    self.schedule_reconcile();
                    return;
                }
            }
        }
        // In `all` mode a warm pet must not linger next to the focused one: a pane
        // that is visible but not focused loses its pet (re-showing it is cheap).
        if self.shared.cfg.mode == Mode::All {
            let focused = self.focused.clone();
            self.instances.retain(|id, _| !on_screen.contains(id) || focused.as_deref() == Some(id.as_str()));
            self.order.retain(|id| self.instances.contains_key(id));
        }
        for (id, inst) in self.instances.iter_mut() {
            if on_screen.contains(id) {
                let animated = self.focused.as_deref() == Some(id.as_str());
                inst.set_visible(true, animated);
                inst.set_placed(&self.shared);
            } else {
                inst.set_visible(false, false);
            }
        }
    }

    fn resubscribe_status(&mut self) {
        let mut ids: Vec<&String> = self.instances.keys().collect();
        ids.sort();
        let key = ids.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
        if key == self.status_key {
            return;
        }
        self.status_key = key;
        self.status_sub = None;
        if ids.is_empty() {
            return;
        }
        self.status_gen += 1;
        let gen = self.status_gen;
        let subs = ids.iter().map(|id| json!({ "type": "pane.agent_status_changed", "pane_id": id })).collect();
        let (tx_ev, tx_close) = (self.tx.clone(), self.tx.clone());
        match socket::subscribe(
            subs,
            move |ev| {
                let _ = tx_ev.send(Msg::Event(ev));
            },
            move |_| {
                let _ = tx_close.send(Msg::StatusClosed(gen));
            },
        ) {
            Ok(conn) => self.status_sub = Some(conn),
            Err(e) => {
                self.status_key.clear();
                log(format!("status subscription: {e}"));
            }
        }
    }

    fn on_event(&mut self, envelope: &Value) {
        let data = &envelope["data"];
        // Global events name the kind in snake_case (`pane_focused`); per-pane
        // subscriptions use the dotted form (`pane.agent_status_changed`) and
        // carry flat data with no `type`. Normalise on the envelope's `event`.
        let kind = envelope["event"].as_str().unwrap_or("").replace('.', "_");
        let kind = kind.as_str();
        if std::env::var_os("PET_TRACE").is_some() {
            log(format!("trace: event {kind} {}", data["pane_id"].as_str().unwrap_or("")));
        }
        let pane_id = data["pane_id"].as_str().map(str::to_owned);
        match kind {
            "pane_agent_status_changed" => {
                let status = Status::parse(data["agent_status"].as_str().unwrap_or("unknown"));
                match pane_id.and_then(|id| self.instances.get_mut(&id)) {
                    Some(inst) => inst.set_status(status, &self.shared),
                    None => self.schedule_reconcile(),
                }
            }
            "pane_exited" => {
                if let Some(inst) = pane_id.and_then(|id| self.instances.get_mut(&id)) {
                    inst.set_exited(&self.shared);
                }
            }
            "pane_closed" => {
                if let Some(id) = pane_id {
                    self.instances.remove(&id);
                    self.order.retain(|o| *o != id);
                }
                self.schedule_reconcile();
            }
            // Focus changes carry the pane id: act now, no debounce, so the pet
            // lands in the same render as the switched-to content.
            "pane_focused" => self.reconcile(),
            "layout_updated" => self.refresh_visibility(),
            _ => self.schedule_reconcile(),
        }
    }

    /// Re-read pet.json (SIGUSR1 from the CLI): reposition, resize, or swap the pet without restarting.
    fn apply_config(&mut self, next: PetConfig) {
        let prev = std::mem::replace(&mut self.shared.cfg, next.clone());
        if next.pet != prev.pet || next.size != prev.size || next.enabled != prev.enabled || next.quantize != prev.quantize {
            if !next.enabled {
                self.load_gen += 1;
                self.load_pending = None;
                self.shared.pet = None;
                log("config: pet disabled");
            } else {
                // Resize now with the frames already loaded (the terminal scales them
                // into the new cell box), so the change is visible immediately …
                if self.shared.pet.is_some() && next.size != prev.size && next.pet == prev.pet {
                    self.shared.grid = grid_for(next.size, self.shared.cell);
                }
                // … and re-decode at the new pixel size off the event loop; the crisp
                // frames swap in via Msg::PetLoaded while the pet keeps animating.
                self.request_load(next.pet.clone(), next.size, next.quantize);
            }
        }
        log(format!(
            "config: enabled={} pet={} mode={} size={} position={:?}",
            next.enabled,
            next.pet,
            next.mode.as_str(),
            next.size,
            next.position
        ));
        if next.mode != prev.mode || next.pet != prev.pet || next.enabled != prev.enabled {
            self.reconcile();
        } else {
            self.refresh_visibility();
        }
    }

    /// One decode at a time; a burst of requests (key repeat) keeps only the latest.
    fn request_load(&mut self, name: String, size: u32, quantize: bool) {
        if self.load_in_flight {
            self.load_pending = Some((name, size, quantize));
            return;
        }
        self.load_in_flight = true;
        self.load_gen += 1;
        let (gen, tx, cell) = (self.load_gen, self.tx.clone(), self.shared.cell);
        std::thread::spawn(move || {
            let result = load_for(&name, size, cell, quantize).map_err(|e| e.to_string());
            let _ = tx.send(Msg::PetLoaded { gen, result });
        });
    }

    /// The pet a mouse drag should move: the one on the focused pane, else the first.
    fn drag_target_id(&self) -> Option<String> {
        self.focused
            .as_ref()
            .filter(|id| self.instances.contains_key(*id))
            .cloned()
            .or_else(|| self.order.first().cloned())
    }

    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Event(ev) => self.on_event(&ev),
            Msg::EventsClosed(reason) => {
                log(format!("event stream closed{}; exiting", reason.map(|r| format!(": {r}")).unwrap_or_default()));
                return false;
            }
            Msg::StatusClosed(gen) => {
                if gen == self.status_gen {
                    // Server dropped it (pane gone or restart); the next reconcile rebuilds it.
                    self.status_key.clear();
                    self.schedule_reconcile();
                }
            }
            Msg::ReloadConfig => match PetConfig::load() {
                Ok(cfg) => self.apply_config(cfg),
                Err(e) => log(format!("config: {e}; keeping current settings")),
            },
            Msg::PetLoaded { gen, result } => {
                self.load_in_flight = false;
                if let Some((name, size, quantize)) = self.load_pending.take() {
                    self.request_load(name, size, quantize);
                }
                if gen != self.load_gen {
                    return true; // superseded by a newer load or a disable
                }
                match result {
                    Ok((pet, grid)) => {
                        self.shared.pet = pet;
                        self.shared.grid = grid;
                        // Frame indices may be out of range for the new pet; restart every row.
                        for inst in self.instances.values_mut() {
                            inst.frame = 0;
                            inst.next_at = Instant::now();
                        }
                        self.reconcile();
                    }
                    Err(e) => {
                        if self.shared.pet.is_none() {
                            log(format!("config: {e}; retrying in 5s"));
                            self.load_retry_at = Some(Instant::now() + Duration::from_secs(5));
                        } else {
                            log(format!("config: {e}; keeping current pet"));
                        }
                    }
                }
            }
            Msg::Shutdown => return false,
            Msg::DragInfo(text) => log(format!("drag: {text}")),
            Msg::DragStart => {
                self.drag_target = self.drag_target_id();
                self.drag_acc = (0.0, 0.0);
                self.drag_dir_acc = 0.0;
            }
            Msg::Drag { dx, dy } => {
                let Some(id) = self.drag_target.clone() else { return true };
                let cell = self.shared.cell;
                // Direction: a horizontal move of >= 4pt picks the facing; slow
                // diagonal drags still accumulate, grab-and-hold keeps the state.
                self.drag_dir_acc += dx;
                let dir = if self.drag_dir_acc >= 4.0 {
                    Some(Row::RunningRight)
                } else if self.drag_dir_acc <= -4.0 {
                    Some(Row::RunningLeft)
                } else {
                    None
                };
                if let Some(row) = dir {
                    self.drag_dir_acc = 0.0;
                    if let Some(inst) = self.instances.get_mut(&id) {
                        inst.set_drag_row(row, &self.shared);
                    }
                }
                self.drag_acc.0 += dx;
                self.drag_acc.1 += dy;
                let dc = (self.drag_acc.0 / cell.width_px as f64).trunc() as i32;
                let dr = (self.drag_acc.1 / cell.height_px as f64).trunc() as i32;
                if dc != 0 || dr != 0 {
                    self.drag_acc.0 -= dc as f64 * cell.width_px as f64;
                    self.drag_acc.1 -= dr as f64 * cell.height_px as f64;
                    if let Some(inst) = self.instances.get_mut(&id) {
                        inst.drag_by(dc, dr, &self.shared);
                    }
                }
            }
            Msg::DragEnd => {
                if let Some(inst) = self.drag_target.take().and_then(|id| self.instances.remove(&id)) {
                    let mut inst = inst;
                    inst.drag_end(&mut self.shared);
                    inst.restore_status_row(&self.shared);
                    self.instances.insert(inst.pane_id.clone(), inst);
                }
            }
        }
        true
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            let now = Instant::now();
            let mut deadline = now + Duration::from_secs(1);
            for inst in self.instances.values() {
                deadline = deadline.min(inst.next_at);
                if let Some(at) = inst.retry_at {
                    deadline = deadline.min(at);
                }
            }
            if let Some(at) = self.reconcile_at {
                deadline = deadline.min(at);
            }
            if let Some(at) = self.load_retry_at {
                deadline = deadline.min(at);
            }
            match rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(msg) => {
                    if !self.handle(msg) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            if self.reconcile_at.is_some_and(|at| now >= at) {
                self.reconcile();
            }
            for inst in self.instances.values_mut() {
                inst.tick(now, &self.shared);
            }
            if self.instances.values().any(|i| i.gone) {
                self.instances.retain(|_, i| !i.gone);
                self.order.retain(|id| self.instances.contains_key(id));
                self.resubscribe_status();
            }
            if self.load_retry_at.is_some_and(|at| now >= at) {
                self.load_retry_at = None;
                let cfg = &self.shared.cfg;
                if cfg.enabled && self.shared.pet.is_none() {
                    self.request_load(cfg.pet.clone(), cfg.size, cfg.quantize);
                }
            }
        }
        // Dropping instances takes their pets off the screen.
        self.instances.clear();
        self.status_sub = None;
    }
}

const EVENTS: &[&str] = &[
    "pane.focused",
    "pane.agent_detected",
    "pane.exited",
    "pane.closed",
    "pane.created",
    "pane.moved",
    "layout.updated",
    "tab.focused",
    "workspace.focused",
];

/// Pixel size of one cell, as a pane's pty reports it (herdr sets it from the
/// attached client, so every pane agrees). Unknown until a client with image
/// support is attached; a pane resized by a bare API client may report zero.
fn probe_cell_size() -> Result<(u32, u32), String> {
    let res = socket::request("pane.list", json!({})).map_err(|e| format!("herdr unavailable ({e}); retrying"))?;
    let mut panes: Vec<&Value> = res["panes"].as_array().into_iter().flatten().collect();
    if panes.is_empty() {
        return Err("no panes yet".to_owned());
    }
    // The focused pane first.
    panes.sort_by_key(|p| !p["focused"].as_bool().unwrap_or(false));
    let mut problem = None;
    for id in panes.iter().filter_map(|p| p["pane_id"].as_str()) {
        let ws = PaneTty::open(id).map_err(|e| e.to_string()).and_then(|tty| tty.winsize().map_err(|e| e.to_string()));
        match ws {
            Ok(ws) => {
                if let Some(px) = ws.cell_px() {
                    return Ok(px);
                }
            }
            Err(e) => {
                problem.get_or_insert(format!("cannot reach pane {id}'s terminal ({e}); retrying"));
            }
        }
    }
    Err(problem.unwrap_or_else(|| {
        "herdr reports no cell pixel size yet: detach (prefix+q) and run `herdr` again from a terminal that draws images (Ghostty, kitty, WezTerm)".to_owned()
    }))
}

/// Keep polling for the cell size instead of dying.
fn wait_for_cell_size(rx: &Receiver<Msg>) -> Option<Cell> {
    let mut warned = String::new();
    loop {
        match probe_cell_size() {
            Ok((width_px, height_px)) => return Some(Cell { width_px, height_px }),
            Err(hint) => {
                if hint != warned {
                    log(format!("waiting: {hint}"));
                    warned = hint;
                }
            }
        }
        // Stay responsive to SIGTERM while waiting.
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return None,
            _ => {}
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Held until exit; the CLI probes this lock to know whether a daemon is alive.
    let _pidfile = crate::pidfile::acquire()?;
    let (tx, rx) = mpsc::channel::<Msg>();

    {
        use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGUSR1])?;
        std::thread::spawn(move || {
            for sig in signals.forever() {
                let _ = tx.send(if sig == SIGUSR1 { Msg::ReloadConfig } else { Msg::Shutdown });
            }
        });
    }

    let Some(cell) = wait_for_cell_size(&rx) else { return Ok(()) };
    // Loaded after the wait: toggles/popover changes made meanwhile must count.
    let cfg = PetConfig::load()?;
    // A missing or broken pet is not fatal (no pets ship with the plugin):
    // keep running, hint, and retry — installing one makes it appear.
    let (pet, grid) = if cfg.enabled {
        match load_for(&cfg.pet, cfg.size, cell, cfg.quantize) {
            Ok(loaded) => loaded,
            Err(e) => {
                log(format!("{e} — install one into ~/.codex/pets (see README); retrying"));
                (None, grid_for(cfg.size, cell))
            }
        }
    } else {
        log("pet disabled; waiting for settings");
        (None, grid_for(cfg.size, cell))
    };
    log(format!(
        "cell {}x{}px → pet {}x{} cells, mode={}, position={:?}",
        cell.width_px, cell.height_px, grid.cols, grid.rows, cfg.mode.as_str(), cfg.position
    ));

    #[cfg(target_os = "macos")]
    if let Some(mods) = cfg.drag.modifiers() {
        // Under SSH the mouse is on another machine, and the Accessibility prompt
        // would be attributed to sshd (`sshd-keygen-wrapper`): skip the tap.
        let over_ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
        if over_ssh {
            log("drag: running under SSH; event tap skipped (use `herdr-pet move`, or restart herdr from a local terminal)");
        } else {
            crate::drag::start(mods, tx.clone());
        }
    }

    let (tx_ev, tx_close) = (tx.clone(), tx.clone());
    let _events = socket::subscribe(
        EVENTS.iter().map(|t| json!({ "type": t })).collect(),
        move |ev| {
            let _ = tx_ev.send(Msg::Event(ev));
        },
        move |reason| {
            let _ = tx_close.send(Msg::EventsClosed(reason));
        },
    )?;

    let retry_needed = cfg.enabled && pet.is_none();
    let mut daemon = Daemon::new(Shared { pet, cfg, grid, cell }, tx);
    if retry_needed {
        daemon.load_retry_at = Some(Instant::now() + Duration::from_secs(5));
    }
    daemon.reconcile();
    log("running");
    daemon.run(rx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(w: u32, h: u32) -> Cell {
        Cell { width_px: w, height_px: h }
    }

    #[test]
    fn grid_keeps_cell_aspect_and_survives_extremes() {
        // 9 rows at 8x17px cells: ceil(9*17*192 / (208*8)) = 18 cols.
        let g = grid_for(9, cell(8, 17));
        assert_eq!((g.cols, g.rows), (18, 9));
        // Degenerate cell sizes must not divide by zero or overflow.
        let g = grid_for(24, cell(0, 100_000));
        assert!(g.cols >= 2 && g.rows == 24);
        let g = grid_for(0, cell(8, 16));
        assert_eq!(g.rows, 2);
    }

    #[test]
    fn pet_sits_in_the_corner_and_stays_inside_the_pane() {
        let shared = Shared { pet: None, cfg: PetConfig::default(), grid: Grid { cols: 10, rows: 6 }, cell: cell(8, 16) };
        let ws = |cols, rows| WinSize { cols, rows, width_px: cols * 8, height_px: rows * 16 };
        // Bottom-right with a one-cell margin.
        let p = placement_for(&shared, (0, 0), ws(100, 40));
        assert_eq!((p.col, p.row, p.cols, p.rows), (89, 33, 10, 6));
        // A drag past the edge is clamped; a pane smaller than the pet pins it to the origin.
        let p = placement_for(&shared, (50, 50), ws(100, 40));
        assert_eq!((p.col, p.row), (90, 34));
        let p = placement_for(&shared, (0, 0), ws(4, 3));
        assert_eq!((p.col, p.row), (0, 0));
        // A configured position wins over the corner.
        let shared = Shared { cfg: PetConfig { position: Some([5, 7]), ..PetConfig::default() }, ..shared };
        let p = placement_for(&shared, (0, 0), ws(100, 40));
        assert_eq!((p.col, p.row), (5, 7));
    }

    #[test]
    fn frame_duration_applies_and_clamps_speed() {
        let mut cfg = PetConfig { speed: 2.0, ..Default::default() };
        assert_eq!(frame_duration(120, &cfg), Duration::from_millis(60));
        cfg.speed = f64::NAN;
        assert_eq!(frame_duration(120, &cfg), Duration::from_millis(120));
        cfg.speed = 100.0; // clamped to 4x, and never below one tick
        assert_eq!(frame_duration(120, &cfg), Duration::from_millis(30));
        assert_eq!(frame_duration(16, &cfg), Duration::from_millis(16));
    }

    #[test]
    fn herdr_status_maps_to_the_codex_rows() {
        for (s, row) in [
            ("working", Row::Running),
            ("blocked", Row::Waiting),
            ("done", Row::Review),
            ("idle", Row::Idle),
            ("something-new", Row::Idle),
        ] {
            assert_eq!(Status::parse(s).row(), row, "{s}");
        }
    }
}
