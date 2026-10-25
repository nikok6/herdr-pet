//! Settings popover: a small crossterm TUI run inside a herdr popup pane.
//! ↑/↓ pick a setting, ←/→ change it (applied live), Esc/q/Enter close.
use crate::config::{DragSetting, Mode, PetConfig};
use crate::socket;
use crate::sprites;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};
use serde_json::json;
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Enabled,
    Pet,
    Mode,
    Size,
    Speed,
    Transitions,
    Quantize,
    /// Only listed in ROWS on macOS, where drag exists.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Drag,
    WarmPanes,
    Position,
}

// Position is not a setting: drag the pet (macOS) or `herdr-pet corner|nudge`;
// the dropped position is remembered, like Codex's and Orca's overlays.
const ROWS: &[Row] = &[
    Row::Enabled,
    Row::Pet,
    Row::Mode,
    Row::Size,
    Row::Speed,
    Row::Transitions,
    Row::Quantize,
    #[cfg(target_os = "macos")]
    Row::Drag,
    Row::WarmPanes,
    Row::Position,
];

impl Row {
    fn label(self) -> &'static str {
        match self {
            Row::Enabled => "Enabled",
            Row::Pet => "Pet",
            Row::Mode => "Mode",
            Row::Size => "Size",
            Row::Speed => "Speed",
            Row::Transitions => "Transitions",
            Row::Quantize => "Quantize",
            Row::Drag => "Drag",
            Row::WarmPanes => "Warm panes",
            Row::Position => "Position…",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Row::Enabled => "show the pet (also prefix+shift+p)",
            Row::Pet => "which pet to show (pets/ or ~/.codex/pets)",
            Row::Mode => "all: follows the focused pane · agents: one per agent pane",
            Row::Size => "height in terminal rows",
            Row::Speed => "playback speed multiplier",
            Row::Transitions => "wave on blocked, jump on done",
            Row::Quantize => "compress frames to 256 colours (fine for sprites)",
            Row::Drag => "hold these keys and drag to move the pet",
            Row::WarmPanes => "recently focused panes that keep their pet ready",
            Row::Position => "press enter: tap a spot on a map of the pane",
        }
    }

    fn value(self, cfg: &PetConfig) -> String {
        match self {
            Row::Enabled => if cfg.enabled { "on" } else { "off" }.into(),
            Row::Pet => if cfg.pet.is_empty() { "(pick one)".into() } else { cfg.pet.clone() },
            Row::Mode => cfg.mode.as_str().into(),
            Row::Size => format!("{} rows", cfg.size),
            Row::Speed => format!("{:.2}x", cfg.speed),
            Row::Transitions => if cfg.transitions { "on" } else { "off" }.into(),
            Row::Quantize => if cfg.quantize { "on" } else { "off" }.into(),
            Row::Drag => cfg.drag.modifiers().unwrap_or("off").into(),
            Row::WarmPanes => cfg.warm_panes.to_string(),
            Row::Position => cfg.position.map(|[c, r]| format!("{c},{r}")).unwrap_or_else(|| "corner".into()),
        }
    }

    /// Apply a step (+1 / -1). Returns true when something changed.
    fn step(self, cfg: &mut PetConfig, dir: i32, pets: &[String]) -> bool {
        let before = cfg.clone();
        match self {
            Row::Enabled => cfg.enabled = !cfg.enabled,
            Row::Pet => {
                if !pets.is_empty() {
                    let idx = pets.iter().position(|n| *n == cfg.pet).unwrap_or(0) as i32;
                    cfg.pet = pets[(idx + dir).rem_euclid(pets.len() as i32) as usize].clone();
                }
            }
            Row::Mode => cfg.mode = if cfg.mode == Mode::All { Mode::Agents } else { Mode::All },
            Row::Size => cfg.size = (cfg.size as i32 + dir).clamp(3, 24) as u32,
            Row::Speed => cfg.speed = if dir > 0 { (cfg.speed * 1.25).min(4.0) } else { (cfg.speed / 1.25).max(0.25) },
            Row::Transitions => cfg.transitions = !cfg.transitions,
            Row::Quantize => cfg.quantize = !cfg.quantize,
            Row::Drag => {
                cfg.drag = if cfg.drag.modifiers().is_some() {
                    DragSetting::Enabled(false)
                } else {
                    DragSetting::Modifiers("control+option".into())
                }
            }
            Row::WarmPanes => cfg.warm_panes = (cfg.warm_panes as i32 + dir).clamp(1, 16) as usize,
            Row::Position => return false, // handled by the picker view
        }
        *cfg != before
    }
}

fn draw(out: &mut impl Write, cfg: &PetConfig, selected: usize) -> io::Result<()> {
    let (cols, _) = terminal::size().unwrap_or((60, 20));
    let width = cols.max(30) as usize;
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(out, SetAttribute(Attribute::Bold), Print(" herdr-pet settings"), SetAttribute(Attribute::Reset))?;
    queue!(out, SetAttribute(Attribute::Dim), Print("   ↑↓ select · ←→ change · esc close"), SetAttribute(Attribute::Reset))?;
    for (i, row) in ROWS.iter().enumerate() {
        let y = 2 + i as u16;
        let marker = if i == selected { "›" } else { " " };
        let line = format!("{marker} {:<12} ‹ {} ›", row.label(), row.value(cfg));
        queue!(out, cursor::MoveTo(0, y))?;
        if i == selected {
            queue!(out, SetAttribute(Attribute::Reverse), Print(format!("{line:<width$}")), SetAttribute(Attribute::Reset))?;
        } else {
            queue!(out, Print(line))?;
        }
    }
    let hint_y = 2 + ROWS.len() as u16 + 1;
    queue!(out, cursor::MoveTo(0, hint_y), SetAttribute(Attribute::Dim), Print(format!(" {}", ROWS[selected].hint())), SetAttribute(Attribute::Reset))?;
    out.flush()
}

/// The pane the picker maps onto: inner cell dimensions of the focused pane.
fn pane_inner() -> Option<(i32, i32)> {
    let cur = socket::request("pane.current", json!({})).ok()?;
    let ws = crate::tty::pane_size(cur["pane"]["pane_id"].as_str()?)?;
    Some(((ws.cols as i32).max(1), (ws.rows as i32).max(1)))
}

/// The box on screen that represents the pane, sized to the popup.
fn picker_box(inner: (i32, i32)) -> (u16, u16, u16, u16) {
    let (cols, rows) = terminal::size().unwrap_or((60, 16));
    let h = rows.saturating_sub(6).clamp(4, 12);
    // Terminal cells are ~1:2, so double the width to keep the pane's shape.
    let ideal_w = (h as i32 * 2 * inner.0 / inner.1.max(1)).max(8) as u16;
    let w = ideal_w.min(cols.saturating_sub(4)).max(8);
    let x = (cols - w) / 2;
    (x, 2, w, h)
}

fn draw_picker(out: &mut impl Write, cfg: &PetConfig, inner: (i32, i32)) -> io::Result<()> {
    let (bx, by, bw, bh) = picker_box(inner);
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(out, SetAttribute(Attribute::Bold), Print(" Position — the box is your pane"), SetAttribute(Attribute::Reset))?;
    for y in 0..bh {
        queue!(out, cursor::MoveTo(bx, by + y))?;
        let line: String = (0..bw)
            .map(|x| match (x, y) {
                (0, 0) => '┌',
                (x, 0) if x == bw - 1 => '┐',
                (0, y) if y == bh - 1 => '└',
                (x, y) if x == bw - 1 && y == bh - 1 => '┘',
                (_, 0) | (_, _) if y == 0 || y == bh - 1 => '─',
                (0, _) => '│',
                (x, _) if x == bw - 1 => '│',
                _ => ' ',
            })
            .collect();
        queue!(out, Print(line))?;
    }
    // Marker at the pet's spot (default corner when unset).
    let [pc, pr] = cfg.position.unwrap_or([inner.0 - 1, inner.1 - 1]);
    let fx = (pc.clamp(0, inner.0 - 1)) as f64 / (inner.0 - 1).max(1) as f64;
    let fy = (pr.clamp(0, inner.1 - 1)) as f64 / (inner.1 - 1).max(1) as f64;
    let mx = bx + 1 + (fx * (bw.saturating_sub(3)) as f64).round() as u16;
    let my = by + 1 + (fy * (bh.saturating_sub(3)) as f64).round() as u16;
    queue!(out, cursor::MoveTo(mx, my), SetAttribute(Attribute::Bold), Print("◉"), SetAttribute(Attribute::Reset))?;
    let below = by + bh;
    queue!(out, cursor::MoveTo(0, below + 1), SetAttribute(Attribute::Dim),
        Print(format!(" pet at {} of a {}x{} pane", cfg.position.map(|[c, r]| format!("{c},{r}")).unwrap_or_else(|| "the corner".into()), inner.0, inner.1)))?;
    queue!(out, cursor::MoveTo(0, below + 2), Print(" tap or drag to place   arrows nudge   d corner   esc back"), SetAttribute(Attribute::Reset))?;
    out.flush()
}

/// Map a click in the box to a pane cell for the pet's top-left.
fn click_to_position(mx: u16, my: u16, inner: (i32, i32)) -> Option<[i32; 2]> {
    let (bx, by, bw, bh) = picker_box(inner);
    if mx < bx || my < by || mx >= bx + bw || my >= by + bh {
        return None;
    }
    let fx = (mx.saturating_sub(bx + 1)) as f64 / (bw.saturating_sub(3)).max(1) as f64;
    let fy = (my.saturating_sub(by + 1)) as f64 / (bh.saturating_sub(3)).max(1) as f64;
    Some([
        (fx.clamp(0.0, 1.0) * (inner.0 - 1) as f64).round() as i32,
        (fy.clamp(0.0, 1.0) * (inner.1 - 1) as f64).round() as i32,
    ])
}

enum View {
    List,
    Picker { inner: (i32, i32) },
}

/// Runs the settings UI on the current terminal. `apply` persists + signals the daemon.
pub fn run(apply: &dyn Fn(&PetConfig)) -> io::Result<()> {
    let pets: Vec<String> = sprites::list_pets().into_iter().map(|(name, _, _)| name).collect();
    let mut cfg = PetConfig::load().map_err(io::Error::other)?;
    let mut selected = 0usize;
    let mut out = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
    let mut view = View::List;
    let set_position = |cfg: &mut PetConfig, pos: Option<[i32; 2]>| {
        if let Ok(mut fresh) = PetConfig::load() {
            fresh.position = pos;
            apply(&fresh);
            *cfg = fresh;
        }
    };
    let result = (|| -> io::Result<()> {
        loop {
            match view {
                View::List => draw(&mut out, &cfg, selected)?,
                View::Picker { inner } => draw_picker(&mut out, &cfg, inner)?,
            }
            match event::read()? {
                Event::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => {}
                Event::Key(KeyEvent { code, modifiers, .. }) => match view {
                    View::Picker { inner } => match code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => view = View::List,
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('d') | KeyCode::Backspace => set_position(&mut cfg, None),
                        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                            let [c, r] = cfg.position.unwrap_or([inner.0 - 1, inner.1 - 1]);
                            let (dc, dr) = match code {
                                KeyCode::Left => (-1, 0),
                                KeyCode::Right => (1, 0),
                                KeyCode::Up => (0, -1),
                                _ => (0, 1),
                            };
                            set_position(&mut cfg, Some([(c + dc).clamp(0, inner.0 - 1), (r + dr).clamp(0, inner.1 - 1)]));
                        }
                        _ => {}
                    },
                    View::List => {
                        let dir = match code {
                            KeyCode::Esc | KeyCode::Char('q') => break,
                            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                            KeyCode::Enter if ROWS[selected] == Row::Position => {
                                view = View::Picker { inner: pane_inner().unwrap_or((80, 24)) };
                                continue;
                            }
                            KeyCode::Enter => 1,
                            KeyCode::Up | KeyCode::Char('k') => {
                                selected = (selected + ROWS.len() - 1) % ROWS.len();
                                continue;
                            }
                            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                                selected = (selected + 1) % ROWS.len();
                                continue;
                            }
                            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => -1,
                            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char(' ') => 1,
                            _ => continue,
                        };
                        // Change the file's current contents, not our snapshot: the daemon
                        // may have written a dragged `position` since the popover opened.
                        let Ok(mut fresh) = PetConfig::load() else { continue };
                        if ROWS[selected].step(&mut fresh, dir, &pets) {
                            apply(&fresh);
                        }
                        cfg = fresh;
                    }
                },
                Event::Mouse(MouseEvent { kind, column, row, .. }) => {
                    if let View::Picker { inner } = view {
                        let pressed = matches!(kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left));
                        if pressed {
                            if let Some(pos) = click_to_position(column, row, inner) {
                                set_position(&mut cfg, Some(pos));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    execute!(out, DisableMouseCapture, cursor::Show, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    result
}
