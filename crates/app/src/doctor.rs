//! `chatcli doctor`: one line per check (OK, WARN, FAIL) with a fix line, exit code 1 when any
//! check fails. This file owns the local checks (config, settings, data folder, stored
//! subscription, WhatsApp session file, single instance) and the output; the Teams checks
//! (TLS, sign-in host, relay, secrets, `--online` subscription) come from `chatcli_teams::doctor`.
//!
//! Nothing here prints a token, a secret, an id or message text: checks name what is missing,
//! never the value.

use std::path::Path;

use chatcli_core::config::{self, Config};
use chatcli_core::doctor::{Check, Level};
use chatcli_core::settings::{self, LoadReport, Severity};
use chatcli_core::store::Store;
use chatcli_core::{lock, paths, Result};

/// Meta key the Teams backend writes the live Graph subscription id under (names only here).
const SUB_ID_KEY: &str = "teams.subscription_id";

fn check(name: &str, level: Level, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        name: name.to_string(),
        level,
        detail: detail.into(),
        fix: fix.map(str::to_string),
    }
}

/// Entry point for `chatcli doctor [--online] [--json]`.
pub async fn run(args: &[&str]) -> Result<()> {
    let mut online = false;
    let mut json = false;
    for a in args {
        match *a {
            "--online" => online = true,
            "--json" => json = true,
            other => {
                return Err(format!(
                    "Unknown option '{other}': use chatcli doctor [--online] [--json]."
                ))
            }
        }
    }
    let mut checks = Vec::new();
    // Probe the lock first: the other checks must not open a second WhatsApp connection while
    // the full client runs.
    // The lock stays held for the whole run, so a client started meanwhile cannot race a token
    // refresh that `--online` makes.
    let (instance, app_running, _held) = instance_check(&paths::data_dir()?.join("chatcli.lock"));
    checks.extend(local_checks(app_running));
    let (ask_graph, skipped) = online_plan(online, app_running);
    checks.extend(chatcli_teams::doctor::checks(ask_graph).await);
    checks.extend(skipped);
    checks.push(instance);
    print!(
        "{}",
        if json {
            render_json(&checks)
        } else {
            render_text(&checks)
        }
    );
    match failure_summary(&checks) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Config, settings, data folder, stored subscription and WhatsApp session file.
fn local_checks(app_running: bool) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(config_check(&config::load()));
    out.push(settings_check(&crate::cli::with_tui_problems(
        settings::load(),
    )));
    match paths::data_dir() {
        Ok(dir) => out.push(data_dir_check(&dir)),
        Err(e) => out.push(check("Data folder", Level::Fail, e, None)),
    }
    match paths::store_db() {
        Ok(db) => out.push(subscription_check(&db, app_running)),
        Err(e) => out.push(check("Teams: live subscription", Level::Fail, e, None)),
    }
    match paths::whatsapp_db() {
        Ok(db) => out.push(whatsapp_check(&db)),
        Err(e) => out.push(check("WhatsApp: session file", Level::Fail, e, None)),
    }
    out
}

/// config.json parsed; ids and relay URL look valid. Values are never printed.
fn config_check(loaded: &Result<Config>) -> Check {
    const NAME: &str = "Config: config.json";
    let cfg = match loaded {
        Ok(c) => c,
        Err(e) => return check(
            NAME,
            Level::Fail,
            e.clone(),
            Some(
                "Fix or delete config.json in the data folder, then run chatcli teams setup again.",
            ),
        ),
    };
    let mut bad = Vec::new();
    if cfg
        .teams_client_id
        .as_deref()
        .is_some_and(|v| !config::is_guid(v))
    {
        bad.push("client id");
    }
    if cfg
        .teams_tenant_id
        .as_deref()
        .is_some_and(|v| !config::is_tenant(v))
    {
        bad.push("tenant id");
    }
    if cfg
        .relay_url
        .as_deref()
        .is_some_and(|v| config::normalize_relay_url(v).is_err())
    {
        bad.push("relay URL");
    }
    if bad.is_empty() {
        check(NAME, Level::Ok, "parses, values look valid", None)
    } else {
        check(
            NAME,
            Level::Fail,
            format!("not valid: {}", bad.join(", ")),
            Some("Run chatcli teams setup <client-id> <tenant> and chatcli relay url <url> again."),
        )
    }
}

/// settings.toml problems by count; `chatcli settings check` lists them.
fn settings_check(report: &LoadReport) -> Check {
    const NAME: &str = "Config: settings.toml";
    let errors = report
        .problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .count();
    let warnings = report.problems.len() - errors;
    let fix = "Run chatcli settings check to see each line.";
    if errors > 0 {
        check(
            NAME,
            Level::Fail,
            format!("{errors} error(s), {warnings} warning(s)"),
            Some(fix),
        )
    } else if warnings > 0 {
        check(
            NAME,
            Level::Warn,
            format!("{warnings} warning(s)"),
            Some(fix),
        )
    } else {
        check(NAME, Level::Ok, "no problems", None)
    }
}

/// Creates and removes a probe file: the only way to know the folder is writable.
fn data_dir_check(dir: &Path) -> Check {
    const NAME: &str = "Data folder";
    let probe = dir.join(format!("doctor-probe-{}.tmp", std::process::id()));
    let result = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&probe, b"x"))
        .and_then(|()| std::fs::remove_file(&probe));
    match result {
        Ok(()) => check(NAME, Level::Ok, "exists and is writable", None),
        Err(e) => check(
            NAME,
            Level::Fail,
            format!("cannot write there: {e}"),
            Some("Make the chatcli data folder writable, or set CHATCLI_DATA_DIR to a folder you own."),
        ),
    }
}

/// Is a Graph subscription id kept in the store? (Its expiry is not stored: the Teams
/// `--online` check asks Graph.) Skipped while the full client runs, which owns the file.
fn subscription_check(db: &Path, app_running: bool) -> Check {
    const NAME: &str = "Teams: live subscription";
    if app_running {
        return check(NAME, Level::Ok, "skipped: chatcli is running", None);
    }
    if !db.exists() {
        return check(NAME, Level::Ok, "no local store yet", None);
    }
    match Store::open_read_only(db).and_then(|s| s.meta(SUB_ID_KEY)) {
        Ok(Some(_)) => check(NAME, Level::Ok, "one subscription is recorded", None),
        Ok(None) => check(
            NAME,
            Level::Ok,
            "none recorded (made when chatcli next starts with a relay)",
            None,
        ),
        // Doctor never upgrades the file: an older layout is a note, not a fault.
        Err(e) if e.starts_with("The store is not upgraded yet") => check(
            NAME,
            Level::Warn,
            "store not upgraded yet",
            Some("Back up store.db, start chatcli once, then run chatcli doctor again."),
        ),
        Err(e) => check(
            NAME,
            Level::Fail,
            format!("cannot read the store: {e}"),
            Some("Close other chatcli windows and run chatcli doctor again."),
        ),
    }
}

/// File presence only: the database is never opened, so a running client is never disturbed.
fn whatsapp_check(db: &Path) -> Check {
    const NAME: &str = "WhatsApp: session file";
    match std::fs::metadata(db) {
        Ok(m) if m.len() > 0 => check(
            NAME,
            Level::Ok,
            "whatsapp.db present (linked state is shown in the app)",
            None,
        ),
        Ok(_) => check(
            NAME,
            Level::Warn,
            "whatsapp.db is empty",
            Some("Open chatcli, go to the WhatsApp tab and scan the QR code."),
        ),
        Err(_) => check(
            NAME,
            Level::Warn,
            "not linked yet",
            Some("Open chatcli, go to the WhatsApp tab and scan the QR code."),
        ),
    }
}

/// `--online` reads the stored sign-in and may refresh it. A running client may be refreshing it
/// too, and two refreshes of one token can undo each other, so with a client running the online
/// call is skipped and the report says why. Pure.
fn online_plan(online: bool, app_running: bool) -> (bool, Option<Check>) {
    if online && app_running {
        let skipped = check(
            "Teams: live subscription (--online)",
            Level::Warn,
            "skipped: chatcli is running and may be refreshing the sign-in",
            Some("Quit chatcli (Ctrl+C or :quit), then run chatcli doctor --online."),
        );
        (false, Some(skipped))
    } else {
        (online, None)
    }
}

/// The check, whether another chatcli holds the lock, and our own lock when we got it (the
/// caller keeps it until the report is done).
fn instance_check(lock_path: &Path) -> (Check, bool, Option<std::fs::File>) {
    const NAME: &str = "Single instance";
    match lock::try_acquire(lock_path) {
        Ok(Some(held)) => (
            check(NAME, Level::Ok, "no other chatcli is running", None),
            false,
            Some(held),
        ),
        Ok(None) => (
            check(
                NAME,
                Level::Warn,
                "another chatcli is running",
                Some("That is fine for this report. Close it before starting a second one."),
            ),
            true,
            None,
        ),
        Err(e) => (
            check(
                NAME,
                Level::Fail,
                format!("cannot open the lock file: {e}"),
                Some("Make the chatcli data folder writable."),
            ),
            false,
            None,
        ),
    }
}

fn label(level: Level) -> &'static str {
    match level {
        Level::Ok => "OK",
        Level::Warn => "WARN",
        Level::Fail => "FAIL",
    }
}

/// `OK    name: detail`, then `      fix: ...` for anything not OK. Pure.
fn render_text(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        s.push_str(&format!("{:<5} {}: {}\n", label(c.level), c.name, c.detail));
        if c.level != Level::Ok {
            if let Some(fix) = &c.fix {
                s.push_str(&format!("      fix: {fix}\n"));
            }
        }
    }
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    s.push_str(&format!("{fails} failed, {warns} warning(s).\n"));
    s
}

/// JSON text escaping (quotes, backslash, control chars). Pure.
fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `{"ok":bool,"checks":[{"name","level","detail","fix"}]}`, one line. Pure.
fn render_json(checks: &[Check]) -> String {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            let fix = c.fix.as_deref().map_or("null".to_string(), json_str);
            format!(
                "{{\"name\":{},\"level\":{},\"detail\":{},\"fix\":{}}}",
                json_str(&c.name),
                json_str(&label(c.level).to_lowercase()),
                json_str(&c.detail),
                fix
            )
        })
        .collect();
    format!(
        "{{\"ok\":{},\"checks\":[{}]}}\n",
        failure_summary(checks).is_none(),
        items.join(",")
    )
}

/// The error for the exit code: Some when any check is FAIL. Pure.
fn failure_summary(checks: &[Check]) -> Option<String> {
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    (fails > 0).then(|| format!("{fails} check(s) failed: fix them and run chatcli doctor again."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::settings::SettingsProblem;

    fn level_of(c: &Check) -> Level {
        c.level
    }

    #[test]
    fn config_check_fails_on_an_unreadable_file_and_bad_ids_without_printing_them() {
        let c = config_check(&Err("config.json is not valid JSON".into()));
        assert_eq!(level_of(&c), Level::Fail);
        assert!(c.fix.as_deref().unwrap().contains("chatcli teams setup"));

        let cfg = Config {
            teams_client_id: Some("secret-looking-not-a-guid".into()),
            teams_tenant_id: Some("contoso.onmicrosoft.com".into()),
            relay_url: Some("http://example.invalid".into()),
        };
        let c = config_check(&Ok(cfg));
        assert_eq!(level_of(&c), Level::Fail);
        assert_eq!(c.detail, "not valid: client id, relay URL");
        assert!(!c.detail.contains("secret-looking"));

        let cfg = Config {
            teams_client_id: Some("00000000-0000-0000-0000-000000000000".into()),
            teams_tenant_id: Some("contoso.onmicrosoft.com".into()),
            relay_url: Some("https://relay.example.invalid".into()),
        };
        assert_eq!(level_of(&config_check(&Ok(cfg))), Level::Ok);
        assert_eq!(level_of(&config_check(&Ok(Config::default()))), Level::Ok);
    }

    fn problem(severity: Severity) -> SettingsProblem {
        SettingsProblem {
            severity,
            line: Some(3),
            section: "layout".into(),
            key: Some("density".into()),
            message: "x".into(),
        }
    }

    fn report(problems: Vec<SettingsProblem>) -> LoadReport {
        let mut r = settings::parse("", Path::new("settings.toml"));
        r.problems = problems;
        r
    }

    #[test]
    fn settings_check_levels_follow_the_worst_problem() {
        assert_eq!(level_of(&settings_check(&report(vec![]))), Level::Ok);
        let warn = settings_check(&report(vec![problem(Severity::Warning)]));
        assert_eq!(level_of(&warn), Level::Warn);
        let fail = settings_check(&report(vec![
            problem(Severity::Warning),
            problem(Severity::Error),
        ]));
        assert_eq!(level_of(&fail), Level::Fail);
        assert_eq!(fail.detail, "1 error(s), 1 warning(s)");
        assert_eq!(
            fail.fix.as_deref(),
            Some("Run chatcli settings check to see each line.")
        );
    }

    #[test]
    fn data_dir_check_passes_on_a_writable_folder_and_fails_when_a_file_is_in_the_way() {
        let dir = std::env::temp_dir().join(format!("chatcli-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(level_of(&data_dir_check(&dir)), Level::Ok);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "probe file left behind"
        );

        // A regular file where the folder should be: cannot be created or written into.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let c = data_dir_check(&blocker);
        assert_eq!(level_of(&c), Level::Fail);
        assert!(c.fix.as_deref().unwrap().contains("CHATCLI_DATA_DIR"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whatsapp_check_never_opens_the_file_and_reads_presence_and_size() {
        let dir = std::env::temp_dir().join(format!("chatcli-doctor-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("whatsapp.db");
        let c = whatsapp_check(&db);
        assert_eq!(level_of(&c), Level::Warn);
        assert_eq!(c.detail, "not linked yet");
        std::fs::write(&db, b"").unwrap();
        assert_eq!(whatsapp_check(&db).detail, "whatsapp.db is empty");
        std::fs::write(&db, b"not a real database").unwrap();
        assert_eq!(level_of(&whatsapp_check(&db)), Level::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscription_check_skips_a_running_client_and_a_missing_store() {
        let missing = std::env::temp_dir()
            .join("chatcli-doctor-no-such-dir")
            .join("store.db");
        assert_eq!(
            subscription_check(&missing, true).detail,
            "skipped: chatcli is running"
        );
        assert_eq!(
            subscription_check(&missing, false).detail,
            "no local store yet"
        );
    }

    #[test]
    fn online_is_skipped_with_a_warning_while_the_client_runs() {
        let (ask, note) = online_plan(true, true);
        assert!(!ask);
        let note = note.expect("a skipped check");
        assert_eq!(level_of(&note), Level::Warn);
        assert!(note.fix.as_deref().unwrap().contains("Quit chatcli"));
        assert_eq!(online_plan(true, false), (true, None));
        assert_eq!(online_plan(false, true), (false, None));
        assert_eq!(online_plan(false, false), (false, None));
    }

    #[test]
    fn instance_check_sees_a_held_lock() {
        let dir = std::env::temp_dir().join(format!("chatcli-doctor-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chatcli.lock");
        let (c, running, ours) = instance_check(&path);
        assert_eq!((level_of(&c), running), (Level::Ok, false));
        // The lock is still ours after the check: nobody else can take it meanwhile.
        assert!(ours.is_some());
        assert!(lock::try_acquire(&path).unwrap().is_none());
        drop(ours);
        let held = lock::try_acquire(&path).unwrap().expect("free lock");
        let (c, running, ours) = instance_check(&path);
        assert_eq!(
            (level_of(&c), running, ours.is_none()),
            (Level::Warn, true, true)
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample() -> Vec<Check> {
        vec![
            check("A", Level::Ok, "fine", None),
            check("B", Level::Warn, "odd \"quote\"", Some("do x")),
            check("C", Level::Fail, "bad", Some("do y")),
        ]
    }

    #[test]
    fn text_output_shows_fixes_only_for_problems_and_a_summary() {
        let t = render_text(&sample());
        assert_eq!(
            t,
            "OK    A: fine\nWARN  B: odd \"quote\"\n      fix: do x\nFAIL  C: bad\n      fix: do y\n1 failed, 1 warning(s).\n"
        );
    }

    #[test]
    fn json_output_escapes_and_reports_ok_false_when_a_check_fails() {
        let j = render_json(&sample());
        assert!(j.starts_with("{\"ok\":false,\"checks\":["), "{j}");
        assert!(j.contains("\"detail\":\"odd \\\"quote\\\"\""), "{j}");
        assert!(j.contains("\"fix\":null"), "{j}");
        assert!(render_json(&sample()[..2]).starts_with("{\"ok\":true"));
        assert_eq!(json_str("a\\b\n\u{1}"), "\"a\\\\b\\n\\u0001\"");
    }

    #[test]
    fn the_exit_code_follows_failures_only() {
        let all = sample();
        assert!(failure_summary(&all)
            .unwrap()
            .starts_with("1 check(s) failed"));
        assert!(failure_summary(&all[..2]).is_none());
    }
}
