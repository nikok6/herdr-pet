//! herdr-pet: Codex-style animated pet floating over herdr agent panes.
//! Plugin actions call this binary; `daemon` (hidden) is the long-running process.
mod config;
mod daemon;
#[cfg(target_os = "macos")]
mod drag;
mod settings;
mod socket;
mod sprites;
mod tty;

use config::{config_path, plugin_root, state_dir, PetConfig};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn log_path() -> PathBuf {
    state_dir().join("pet.log")
}

/// Locked pidfile: the daemon holds an exclusive flock on it for its lifetime,
/// so liveness is the lock (released by the kernel when the process dies), not
/// a pid number that another process may have inherited after a crash/reboot.
mod pidfile {
    use super::state_dir;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, Write};
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    fn path() -> PathBuf {
        state_dir().join("pet.pid")
    }

    fn open() -> std::io::Result<File> {
        std::fs::create_dir_all(state_dir())?;
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path())
    }

    fn try_lock(file: &File) -> std::io::Result<bool> {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// Daemon side: take the lock and record our pid. Fails if another daemon holds it.
    pub fn acquire() -> std::io::Result<File> {
        let mut file = open()?;
        if !try_lock(&file)? {
            return Err(std::io::Error::other("another herdr-pet daemon is already running"));
        }
        file.set_len(0)?;
        file.rewind()?;
        write!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(file)
    }

    /// CLI side: the pid of the daemon holding the lock, if any.
    pub fn holder() -> Option<i32> {
        let mut file = open().ok()?;
        if try_lock(&file).ok()? {
            // We got the lock, so nobody else holds it; release by dropping `file`.
            return None;
        }
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        text.trim().parse().ok().filter(|pid| *pid > 0)
    }
}

fn running_pid() -> Option<i32> {
    pidfile::holder()
}

fn wait_until(mut done: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    done()
}

fn start() -> Result<(), String> {
    if let Some(pid) = running_pid() {
        println!("pet: already running (pid {pid})");
        return Ok(());
    }
    fs::create_dir_all(state_dir()).map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .current_dir(plugin_root())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log));
    {
        // Own process group so herdr's action runner cannot take the daemon down with it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    // The daemon takes the pidfile lock first thing; if two starts race, the
    // loser exits by itself with "already running" in the log.
    if wait_until(|| running_pid().is_some(), std::time::Duration::from_secs(2)) {
        println!("pet: started (pid {}), log: {}", running_pid().unwrap_or(child.id() as i32), log_path().display());
        Ok(())
    } else {
        Err(format!("daemon did not start; see {}", log_path().display()))
    }
}

/// SIGTERM, wait for the lock to be released, escalate to SIGKILL if needed.
fn stop() -> Result<(), String> {
    let Some(pid) = running_pid() else {
        println!("pet: not running");
        return Ok(());
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    if !wait_until(|| running_pid().is_none(), std::time::Duration::from_secs(3)) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        if !wait_until(|| running_pid().is_none(), std::time::Duration::from_secs(2)) {
            return Err(format!("daemon (pid {pid}) did not exit"));
        }
    }
    println!("pet: stopped (pid {pid})");
    Ok(())
}

fn status() -> Result<(), String> {
    let cfg = PetConfig::load()?;
    let state = match running_pid() {
        Some(pid) => format!("running (pid {pid})"),
        None => "not running".into(),
    };
    println!("daemon: {state}");
    let resolved = if cfg.pet.is_empty() {
        "(none configured)".into()
    } else {
        sprites::resolve_pet(&cfg.pet).map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())
    };
    println!("pet: {} → {resolved}{}", cfg.pet, if cfg.enabled { "" } else { " (hidden)" });
    println!(
        "mode={} size={} position={} transitions={} speed={} quantize={} drag={} warm_panes={}",
        cfg.mode.as_str(),
        cfg.size,
        cfg.position.map(|[c, r]| format!("{c},{r}")).unwrap_or_else(|| "bottom-right".into()),
        cfg.transitions,
        cfg.speed,
        cfg.quantize,
        cfg.drag.modifiers().unwrap_or("off"),
        cfg.warm_panes
    );
    println!("config: {}", config_path().display());
    println!("log: {}", log_path().display());
    Ok(())
}

/// Persist the config and tell the running daemon to apply it. Returns a one-line summary.
fn apply(cfg: &PetConfig) -> Result<String, String> {
    cfg.save().map_err(|e| format!("cannot write {}: {e}", config_path().display()))?;
    let pid = running_pid();
    if let Some(pid) = pid {
        unsafe { libc::kill(pid, libc::SIGUSR1) };
    }
    Ok(format!(
        "enabled={} pet={} size={} speed={}{}",
        cfg.enabled,
        cfg.pet,
        cfg.size,
        cfg.speed,
        if pid.is_some() { "" } else { " (daemon not running; applies on start)" }
    ))
}

fn update_config(mutate: impl FnOnce(&mut PetConfig)) -> Result<(), String> {
    let mut cfg = PetConfig::load()?;
    mutate(&mut cfg);
    println!("pet: {}", apply(&cfg)?);
    Ok(())
}

/// `pet.settings` action: open the manifest's `settings` popup pane through herdr.
fn open_settings() -> Result<(), String> {
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let plugin = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "pet".into());
    let out = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", &plugin, "--entrypoint", "settings", "--placement", "popup"])
        .output()
        .map_err(|e| format!("run herdr: {e}"))?;
    if !out.status.success() {
        return Err(format!("herdr plugin pane open failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

/// Dev aid: write a pet's encoded frames (at `rows` x 17px) to a directory.
fn dump(name: &str, out: &str) -> Result<(), String> {
    let dir = sprites::resolve_pet(name).ok_or_else(|| format!("unknown pet {name:?}"))?;
    let cfg = PetConfig::load()?;
    let pet = sprites::load_pet(&dir, Some(cfg.size * 17), cfg.quantize).map_err(|e| e.to_string())?;
    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    for row in sprites::Row::ALL {
        for i in 0..pet.durations(row).len() {
            fs::write(format!("{out}/{}-{i:02}.png", row.name()), pet.frame(row, i)).map_err(|e| e.to_string())?;
        }
    }
    println!("wrote {}x{} frames to {out}", pet.width, pet.height);
    Ok(())
}

const USAGE: &str = "usage: herdr-pet <toggle|settings|status|list|use <pet>|move <col> <row>|start|stop|restart|ensure|bigger|smaller|faster|slower|dump <pet> <dir>>";

fn main() {
    // `herdr-pet status | head -1` must not panic: let a closed stdout end us quietly.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("toggle");
    let arg = args.get(1).map(String::as_str);
    let result: Result<(), String> = match cmd {
        "daemon" => daemon::run().map_err(|e| e.to_string()),
        "start" | "ensure" => start(),
        "stop" => stop(),
        "restart" => stop().and_then(|_| start()),
        // Wake / tuck away. With no daemon running nothing is on screen, so the
        // press means "show the pet" — first use after an install starts it.
        // Otherwise flip; the daemon keeps running while tucked away.
        "toggle" => (|| {
            let mut cfg = PetConfig::load()?;
            cfg.enabled = if running_pid().is_none() { true } else { !cfg.enabled };
            let summary = apply(&cfg)?;
            println!("pet: {}", if cfg.enabled { summary } else { "tucked away".to_owned() });
            if cfg.enabled && running_pid().is_none() {
                start()
            } else {
                Ok(())
            }
        })(),
        "settings" => settings::run(&|cfg| {
            let _ = apply(cfg);
        })
        .map_err(|e| e.to_string()),
        "open-settings" => open_settings(),
        "status" => status(),
        "list" => {
            for (name, dir, label) in sprites::list_pets() {
                println!("{name}\t{label}\t{}", dir.display());
            }
            Ok(())
        }
        "use" => match arg {
            Some(name) if sprites::resolve_pet(name).is_some() => {
                // Paths are stored absolute: the daemon's cwd is the plugin root, not this shell's.
                let value = if name.contains('/') {
                    fs::canonicalize(name).map(|p| p.display().to_string()).unwrap_or_else(|_| name.to_owned())
                } else {
                    name.to_owned()
                };
                update_config(|cfg| {
                    cfg.pet = value;
                    cfg.enabled = true;
                })
            }
            other => Err(format!("unknown pet {:?} (see `herdr-pet list`)", other.unwrap_or(""))),
        },
        // Explicit position (cells from the pane's top-left); `move default` restores the corner.
        "move" => match (arg, args.get(2)) {
            (Some("default"), _) => update_config(|cfg| cfg.position = None),
            (Some(c), Some(r)) => match (c.parse::<i32>(), r.parse::<i32>()) {
                (Ok(c), Ok(r)) => update_config(|cfg| cfg.position = Some([c.max(0), r.max(0)])),
                _ => Err("move <col> <row> | move default".into()),
            },
            _ => Err("move <col> <row> | move default".into()),
        },
        "bigger" => update_config(|cfg| cfg.size = (cfg.size + 1).min(24)),
        "smaller" => update_config(|cfg| cfg.size = cfg.size.saturating_sub(1).max(3)),
        "faster" => update_config(|cfg| cfg.speed = (cfg.speed * 1.25).min(4.0)),
        "slower" => update_config(|cfg| cfg.speed = (cfg.speed / 1.25).max(0.25)),
        "dump" => match (arg, args.get(2)) {
            (Some(name), Some(out)) => dump(name, out),
            _ => Err("dump <pet> <outdir>".into()),
        },
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("pet: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use crate::config::test_support::{temp_dir, ENV_LOCK};

    #[test]
    fn pidfile_lock_is_the_liveness_signal() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_dir("pidfile");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);

        assert_eq!(super::pidfile::holder(), None, "no daemon yet");
        let lock = super::pidfile::acquire().unwrap();
        assert_eq!(super::pidfile::holder(), Some(std::process::id() as i32));
        // A second daemon must be refused while the lock is held.
        assert!(super::pidfile::acquire().is_err());
        drop(lock);
        // The stale pid number remains in the file, but the lock is gone.
        assert_eq!(super::pidfile::holder(), None);
    }
}
