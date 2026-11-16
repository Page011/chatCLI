//! Rolling log file: `chatcli.log` in the data folder, rolled to `chatcli.log.1` past 5 MB.
//!
//! Records connection, sign-in, subscription, relay and send events with their errors, so a
//! failure is findable after the fact. Never pass tokens, secrets or message text here.

use std::io::Write;
use std::sync::Mutex;

const MAX_BYTES: u64 = 5 * 1024 * 1024;

static LOCK: Mutex<()> = Mutex::new(());

pub fn info(area: &str, text: &str) {
    write("INFO", area, text);
}

pub fn error(area: &str, text: &str) {
    write("ERROR", area, text);
}

fn write(level: &str, area: &str, text: &str) {
    // Logging must never take the app down: every failure here is swallowed.
    let Ok(path) = crate::paths::log_file() else {
        return;
    };
    let _guard = LOCK.lock();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    // One line per entry: newlines inside the text would split it.
    let text = text.replace(['\r', '\n'], " ");
    let _ = writeln!(file, "{when} {level} [{area}] {text}");
}
