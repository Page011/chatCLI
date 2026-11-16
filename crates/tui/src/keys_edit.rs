//! Changing the user's key bindings from the keys page. Pure: each function takes the live
//! keymap and the user's `[keys]` and returns the new `KeySettings` plus the `settings.toml`
//! edits that go with them, or the inline error text. `App` applies and queues them.
//!
//! Every edit is worked out on the context's own table (key text as written -> action name) and
//! then diffed against the old one, so a key that is dropped and set again is one plain set.

use std::collections::BTreeMap;

use chatcli_core::settings::{KeySettings, SettingEdit, SettingValue, Severity};
use crossterm::event::{KeyCode, KeyModifiers};

use crate::keymap::{
    common_applies, default_keys, parse_key, Action, Context, KeyChord, Keymap, CTRL_C,
};

const ESC: KeyChord = KeyChord {
    code: KeyCode::Esc,
    mods: KeyModifiers::NONE,
};

pub(crate) type Done = Result<(KeySettings, Vec<SettingEdit>), String>;

/// Why `chord` may not be taken away from `action`: Ctrl+C always quits, and Esc on `back` always
/// backs out, so there is always a way out.
pub(crate) fn locked(chord: KeyChord, action: Action) -> Option<&'static str> {
    if chord == CTRL_C {
        Some("Ctrl+C always quits")
    } else if chord == ESC && action == Action::Back {
        Some("Esc always backs out here")
    } else {
        None
    }
}

/// What a key pressed while capturing means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Captured {
    /// Esc: leave the capture, nothing changes.
    Cancel,
    /// Backspace: remove the action's first key.
    Remove,
    /// This key cannot be used: the message, and the capture stays open.
    Refused(String),
    /// The action already has this key.
    Same,
    /// Another action holds this key: ask before swapping.
    Conflict(Action),
    /// Free: bind it.
    Bind,
}

/// Sorts one key press during capture for `action` in `ctx`.
pub(crate) fn capture(keys: &Keymap, ctx: Context, action: Action, k: KeyChord) -> Captured {
    if k == ESC {
        return Captured::Cancel;
    }
    if k.code == KeyCode::Backspace && k.mods.is_empty() {
        return Captured::Remove;
    }
    if k.to_setting().is_none() {
        return Captured::Refused("This key cannot be saved: pick another".to_string());
    }
    if ctx.types_text() && k.types_text() {
        return Captured::Refused(format!(
            "{} types text here: use a Ctrl or Alt key",
            k.display()
        ));
    }
    match keys.find(ctx, k) {
        Some(a) if a == action => Captured::Same,
        Some(b) => Captured::Conflict(b),
        None => Captured::Bind,
    }
}

fn table_of(user: &KeySettings, ctx: Context) -> BTreeMap<String, String> {
    user.bindings.get(ctx.name()).cloned().unwrap_or_default()
}

fn spelled(k: KeyChord) -> Result<String, String> {
    k.to_setting()
        .ok_or_else(|| "This key cannot be saved: pick another".to_string())
}

/// True when chatcli itself binds `k` in `ctx`, to any action.
fn default_binds(ctx: Context, k: KeyChord) -> bool {
    Keymap::default().find(ctx, k).is_some()
}

/// `old` stops doing anything in this table: a user entry for it goes, and a key chatcli
/// binds (to this action or, after a move, to another) is written as unbound, so dropping the
/// entry never brings an older binding back.
fn unbind(t: &mut BTreeMap<String, String>, ctx: Context, old: KeyChord) -> Result<(), String> {
    t.retain(|key, _| parse_key(key).ok() != Some(old));
    if default_binds(ctx, old) {
        t.insert(spelled(old)?, Action::Unbound.name().to_string());
    }
    Ok(())
}

/// `k` runs `action` here: any entry for the same key goes first, so a key is never listed twice.
fn assign(t: &mut BTreeMap<String, String>, k: KeyChord, action: Action) -> Result<(), String> {
    t.retain(|key, _| parse_key(key).ok() != Some(k));
    t.insert(spelled(k)?, action.name().to_string());
    Ok(())
}

fn errors(user: &KeySettings) -> usize {
    Keymap::build(user)
        .1
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .count()
}

/// The new `KeySettings` with `t` as the table of `ctx`, and the edits that get there. A change
/// that makes the keymap report a new error is refused (defensive: the page only writes keys
/// that parse).
fn finish(user: &KeySettings, ctx: Context, t: BTreeMap<String, String>) -> Done {
    let before = table_of(user, ctx);
    let table = format!("keys.{}", ctx.name());
    let mut edits: Vec<SettingEdit> = before
        .keys()
        .filter(|k| !t.contains_key(*k))
        .map(|k| SettingEdit::remove(&table, k))
        .collect();
    edits.extend(
        t.iter()
            .filter(|(k, v)| before.get(*k) != Some(*v))
            .map(|(k, v)| SettingEdit::set(&table, k, SettingValue::Str(v.clone()))),
    );
    let mut new = user.clone();
    if t.is_empty() {
        new.bindings.remove(ctx.name());
    } else {
        new.bindings.insert(ctx.name().to_string(), t);
    }
    if errors(&new) > errors(user) {
        let problem = Keymap::build(&new)
            .1
            .into_iter()
            .rfind(|p| p.severity == Severity::Error);
        return Err(format!(
            "KEY NOT SAVED - {}",
            problem.map_or("the key is not valid".to_string(), |p| p.message)
        ));
    }
    Ok((new, edits))
}

/// `k` becomes a key of `action` in `ctx`, replacing the action's first key. A
/// locked first key (Esc that backs out, Ctrl+C) stays and `k` is added. When `give` is set
/// (the swap) that action takes the key `action` gave up, if it gave one up.
fn rebind(
    keys: &Keymap,
    user: &KeySettings,
    ctx: Context,
    action: Action,
    k: KeyChord,
    give: Option<Action>,
) -> Done {
    let mut t = table_of(user, ctx);
    let old = keys
        .keys_in(ctx, action)
        .first()
        .copied()
        .filter(|c| locked(*c, action).is_none());
    if let Some(old) = old {
        unbind(&mut t, ctx, old)?;
    }
    assign(&mut t, k, action)?;
    if let (Some(other), Some(old)) = (give, old) {
        assign(&mut t, old, other)?;
    }
    finish(user, ctx, t)
}

/// Binds the free key `k` to `action` in `ctx`.
pub(crate) fn bind(
    keys: &Keymap,
    user: &KeySettings,
    ctx: Context,
    action: Action,
    k: KeyChord,
) -> Done {
    rebind(keys, user, ctx, action, k, None)
}

/// `action` takes `k` from `other`; `other` gets the key `action` had, or just loses `k`.
pub(crate) fn swap(
    keys: &Keymap,
    user: &KeySettings,
    ctx: Context,
    action: Action,
    k: KeyChord,
    other: Action,
) -> Done {
    rebind(keys, user, ctx, action, k, Some(other))
}

/// Backspace: the action's first key goes. A locked one is refused with the reason.
pub(crate) fn remove_first(
    keys: &Keymap,
    user: &KeySettings,
    ctx: Context,
    action: Action,
) -> Done {
    let Some(first) = keys.keys_in(ctx, action).first().copied() else {
        return Ok((user.clone(), Vec::new()));
    };
    remove_key(user, ctx, action, first)
}

/// `d` on the keys page: `k` stops running `action` in `ctx`. A locked key is refused with
/// the reason.
pub(crate) fn remove_key(user: &KeySettings, ctx: Context, action: Action, k: KeyChord) -> Done {
    if let Some(why) = locked(k, action) {
        return Err(why.to_string());
    }
    let mut t = table_of(user, ctx);
    unbind(&mut t, ctx, k)?;
    finish(user, ctx, t)
}

/// What a key pressed while adding a key to an action means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Adding {
    /// Esc: leave, nothing changes.
    Cancel,
    /// This key cannot be added: the message, and the capture stays open.
    Refused(String),
    /// The action already has this key.
    Same,
    /// The key is free here but overlaps a binding that is live at the same time: the warning,
    /// and Enter adds it anyway.
    Overlaps(String),
    /// Free: add it.
    Free,
}

/// Sorts one key press while adding a key to `action` in `ctx`. Another action of the same
/// context is refused (one key runs one action per context) and named, so it can be deleted
/// there. A key that only overlaps another context that is live at the same time is allowed
/// after a warning.
pub(crate) fn capture_add(keys: &Keymap, ctx: Context, action: Action, k: KeyChord) -> Adding {
    if k == ESC {
        return Adding::Cancel;
    }
    if k.to_setting().is_none() {
        return Adding::Refused("This key cannot be saved: pick another".to_string());
    }
    if ctx.types_text() && k.types_text() {
        return Adding::Refused(format!(
            "{} types text here: use a Ctrl or Alt key",
            k.display()
        ));
    }
    match keys.find(ctx, k) {
        Some(a) if a == action => return Adding::Same,
        Some(other) => {
            return Adding::Refused(format!(
                "{} is already {} ({}): delete it there first",
                k.display(),
                other.label(),
                ctx.title()
            ))
        }
        None => {}
    }
    let notes = overlaps(keys, ctx, action, k);
    if notes.is_empty() {
        Adding::Free
    } else {
        Adding::Overlaps(format!("{} {}", k.display(), notes.join("; ")))
    }
}

/// How a new `k` for `action` in `ctx` would overlap bindings that are live at the same time.
/// A context reads its own keys, then `[keys.common]` (where its action applies), so only those
/// pairs can clash.
fn overlaps(keys: &Keymap, ctx: Context, action: Action, k: KeyChord) -> Vec<String> {
    let mut out = Vec::new();
    if ctx == Context::Common {
        for c in Context::ALL {
            let reaches = c != Context::Common
                && common_applies(action, c)
                && !(c.types_text() && k.prints());
            if !reaches {
                continue;
            }
            if let Some(other) = keys.find(c, k) {
                out.push(format!(
                    "is {} in {}, which wins there",
                    other.label(),
                    c.title()
                ));
            } else if !keys.sequences_from(c, k).is_empty() {
                out.push(format!(
                    "starts key sequences in {}, which stop working",
                    c.title()
                ));
            }
        }
    } else {
        if let Some(other) = keys.common_for(ctx, k) {
            out.push(format!(
                "is {} everywhere: in {} it will be {} instead",
                other.label(),
                ctx.title(),
                action.label()
            ));
        }
        let seqs = keys.sequences_from(ctx, k);
        if !seqs.is_empty() {
            out.push(format!("starts {}, which stop working", seqs.join(", ")));
        }
    }
    out
}

/// `a` on the keys page: `k` becomes one more key of `action` in `ctx`. A built-in key of the
/// action that was unbound comes back by dropping the unbinding.
pub(crate) fn add(user: &KeySettings, ctx: Context, action: Action, k: KeyChord) -> Done {
    let mut t = table_of(user, ctx);
    if default_keys(ctx, action).contains(&k) {
        t.retain(|key, _| parse_key(key).ok() != Some(k));
    } else {
        assign(&mut t, k, action)?;
    }
    finish(user, ctx, t)
}

/// `r`: every entry that names `action`, and every unbinding of one of its built-in keys, goes.
pub(crate) fn reset_action(user: &KeySettings, ctx: Context, action: Action) -> Done {
    let own = default_keys(ctx, action);
    let mut t = table_of(user, ctx);
    t.retain(|key, value| {
        let unbinds =
            value == Action::Unbound.name() && parse_key(key).is_ok_and(|c| own.contains(&c));
        value != action.name() && !unbinds
    });
    finish(user, ctx, t)
}

/// Reset all: every entry of every `[keys.<context>]` goes.
pub(crate) fn reset_all(user: &KeySettings) -> Done {
    let edits = user
        .bindings
        .iter()
        .flat_map(|(ctx, table)| {
            let name = format!("keys.{ctx}");
            table
                .keys()
                .map(|k| SettingEdit::remove(&name, k))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut new = user.clone();
    new.bindings.clear();
    new.lines.clear();
    Ok((new, edits))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> KeyChord {
        parse_key(s).unwrap()
    }

    fn keymap(user: &KeySettings) -> Keymap {
        Keymap::build(user).0
    }

    fn user(entries: &[(&str, &str, &str)]) -> KeySettings {
        let mut s = KeySettings::default();
        for (c, key, a) in entries {
            s.bindings
                .entry(c.to_string())
                .or_default()
                .insert(key.to_string(), a.to_string());
        }
        s
    }

    fn set(table: &str, key: &str, v: &str) -> SettingEdit {
        SettingEdit::set(table, key, SettingValue::str(v))
    }

    #[test]
    fn to_setting_round_trips_every_built_in_key() {
        let map = Keymap::default();
        for ctx in Context::ALL {
            for a in Action::ALL {
                for chord in map.keys_in(ctx, *a) {
                    let text = chord.to_setting().unwrap_or_else(|| {
                        panic!("{ctx:?} {a:?}: {} has no spelling", chord.display())
                    });
                    assert_eq!(parse_key(&text).unwrap(), chord, "{text}");
                }
            }
        }
    }

    #[test]
    fn to_setting_spells_the_awkward_keys() {
        let s = |c: KeyChord| c.to_setting();
        assert_eq!(s(k("space")).as_deref(), Some("space"));
        assert_eq!(s(k("ctrl+shift+v")).as_deref(), Some("ctrl+shift+v"));
        assert_eq!(s(k("alt+shift+g")).as_deref(), Some("alt+shift+g"));
        assert_eq!(s(k("shift+tab")).as_deref(), Some("shift+tab"));
        assert_eq!(s(k("G")).as_deref(), Some("G"));
        assert_eq!(s(k("+")).as_deref(), Some("+"));
        assert_eq!(s(k("ctrl++")).as_deref(), Some("ctrl++"));
        assert_eq!(s(k("f12")).as_deref(), Some("f12"));
        assert_eq!(s(k("pgdn")).as_deref(), Some("pgdn"));
        assert_eq!(s(KeyChord::new(KeyCode::Null, KeyModifiers::NONE)), None);
    }

    #[test]
    fn capture_sorts_a_key_press() {
        let map = keymap(&KeySettings::default());
        let c = |key: &str, ctx, a| capture(&map, ctx, a, k(key));
        assert_eq!(c("esc", Context::List, Action::Refresh), Captured::Cancel);
        assert_eq!(
            c("backspace", Context::List, Action::Refresh),
            Captured::Remove
        );
        assert_eq!(c("f5", Context::List, Action::Refresh), Captured::Bind);
        assert_eq!(c("r", Context::List, Action::Refresh), Captured::Same);
        assert_eq!(
            c("p", Context::List, Action::Refresh),
            Captured::Conflict(Action::Pin)
        );
        assert_eq!(
            c("x", Context::Composer, Action::Send),
            Captured::Refused("x types text here: use a Ctrl or Alt key".to_string())
        );
        // Ctrl keys type nothing, so a text context takes them.
        assert_eq!(c("ctrl+n", Context::Composer, Action::Send), Captured::Bind);
        // A text key is fine where nothing is typed.
        assert_eq!(c("x", Context::Select, Action::Reply), Captured::Bind);
        assert_eq!(
            capture(
                &map,
                Context::List,
                Action::Refresh,
                KeyChord::new(KeyCode::Null, KeyModifiers::NONE)
            ),
            Captured::Refused("This key cannot be saved: pick another".to_string())
        );
    }

    #[test]
    fn binding_f5_unbinds_the_built_in_key_and_writes_both() {
        let u = KeySettings::default();
        let map = keymap(&u);
        let (new, edits) = bind(&map, &u, Context::List, Action::Refresh, k("f5")).unwrap();
        assert_eq!(
            edits,
            vec![
                set("keys.list", "f5", "refresh"),
                set("keys.list", "r", "unbound"),
            ]
        );
        let after = keymap(&new);
        assert_eq!(after.lookup(Context::List, k("f5")), Some(Action::Refresh));
        assert_eq!(after.lookup(Context::List, k("r")), None);
        assert_eq!(
            after.keys_in(Context::List, Action::Refresh),
            vec![k("f5")],
            "the new key is the one hints show"
        );
    }

    #[test]
    fn a_user_entry_written_in_capitals_is_replaced_never_duplicated() {
        let u = user(&[("list", "Ctrl+N", "refresh")]);
        let map = keymap(&u);
        assert_eq!(
            map.lookup(Context::List, k("ctrl+n")),
            Some(Action::Refresh)
        );
        let (new, edits) = bind(&map, &u, Context::List, Action::Refresh, k("f6")).unwrap();
        assert!(edits.contains(&SettingEdit::remove("keys.list", "Ctrl+N")));
        assert!(edits.contains(&set("keys.list", "f6", "refresh")));
        assert_eq!(edits.len(), 2, "the first key was the user's: r stays");
        assert!(!new.bindings["list"].contains_key("Ctrl+N"));
        // Binding the same key again under its other spelling is one entry, not two.
        let u = user(&[("list", "Ctrl+N", "pin")]);
        let map = keymap(&u);
        let (new, edits) = swap(
            &map,
            &u,
            Context::List,
            Action::Refresh,
            k("ctrl+n"),
            Action::Pin,
        )
        .unwrap();
        assert!(edits.contains(&SettingEdit::remove("keys.list", "Ctrl+N")));
        assert_eq!(new.bindings["list"]["ctrl+n"], "refresh");
        assert_eq!(new.bindings["list"].len(), 2, "{:?}", new.bindings["list"]);
    }

    #[test]
    fn swap_gives_the_other_action_the_old_key() {
        let u = KeySettings::default();
        let map = keymap(&u);
        // p pins, r refreshes: refresh takes p, pin gets r.
        let (new, _) = swap(
            &map,
            &u,
            Context::List,
            Action::Refresh,
            k("p"),
            Action::Pin,
        )
        .unwrap();
        let after = keymap(&new);
        assert_eq!(after.lookup(Context::List, k("p")), Some(Action::Refresh));
        assert_eq!(after.lookup(Context::List, k("r")), Some(Action::Pin));
        assert_eq!(new.bindings["list"].len(), 2, "no stray unbound entry");
    }

    #[test]
    fn swap_without_an_old_key_just_takes_the_key() {
        let u = user(&[("list", "r", "unbound")]);
        let map = keymap(&u);
        assert!(map.keys_in(Context::List, Action::Refresh).is_empty());
        let (new, _) = swap(
            &map,
            &u,
            Context::List,
            Action::Refresh,
            k("p"),
            Action::Pin,
        )
        .unwrap();
        let after = keymap(&new);
        assert_eq!(after.lookup(Context::List, k("p")), Some(Action::Refresh));
        assert!(after.keys_in(Context::List, Action::Pin).is_empty());
    }

    #[test]
    fn a_locked_first_key_stays_and_the_new_key_is_added() {
        let u = KeySettings::default();
        let map = keymap(&u);
        let (new, edits) = bind(&map, &u, Context::Common, Action::Quit, k("ctrl+q")).unwrap();
        assert_eq!(edits, vec![set("keys.common", "ctrl+q", "quit")]);
        let after = keymap(&new);
        assert_eq!(
            after.lookup(Context::Common, k("ctrl+q")),
            Some(Action::Quit)
        );
        assert_eq!(
            after.lookup(Context::Common, k("ctrl+c")),
            Some(Action::Quit)
        );
        // The same for Esc on a cancel action.
        let (new, edits) = bind(&map, &u, Context::Help, Action::Back, k("x")).unwrap();
        assert_eq!(edits, vec![set("keys.help", "x", "back")]);
        assert_eq!(
            keymap(&new).lookup(Context::Help, k("esc")),
            Some(Action::Back)
        );
    }

    #[test]
    fn backspace_removes_the_first_key_unless_it_is_locked() {
        let u = KeySettings::default();
        let map = keymap(&u);
        let (new, edits) = remove_first(&map, &u, Context::List, Action::Refresh).unwrap();
        assert_eq!(edits, vec![set("keys.list", "r", "unbound")]);
        assert_eq!(keymap(&new).lookup(Context::List, k("r")), None);
        assert_eq!(
            remove_first(&map, &u, Context::Help, Action::Back).unwrap_err(),
            "Esc always backs out here"
        );
        assert_eq!(
            remove_first(&map, &u, Context::Common, Action::Quit).unwrap_err(),
            "Ctrl+C always quits"
        );
        // Nothing bound: nothing to do.
        let (same, edits) = remove_first(&map, &u, Context::List, Action::FilterStart).unwrap();
        assert!(edits.is_empty() && same == u);
        // A user entry is removed as written, not unbound.
        let u = user(&[("list", "Ctrl+N", "refresh")]);
        let map = keymap(&u);
        let (new, edits) = remove_first(&map, &u, Context::List, Action::Refresh).unwrap();
        assert_eq!(edits, vec![SettingEdit::remove("keys.list", "Ctrl+N")]);
        assert!(new.bindings.is_empty());
    }

    #[test]
    fn add_keeps_the_old_keys_and_writes_one_entry() {
        let u = KeySettings::default();
        let map = keymap(&u);
        assert_eq!(
            capture_add(&map, Context::List, Action::Refresh, k("f5")),
            Adding::Free
        );
        let (new, edits) = add(&u, Context::List, Action::Refresh, k("f5")).unwrap();
        assert_eq!(edits, vec![set("keys.list", "f5", "refresh")]);
        let after = keymap(&new);
        assert_eq!(
            after.keys_in(Context::List, Action::Refresh),
            vec![k("f5"), k("r")],
            "your keys come first, so hints show the one you set"
        );
        // A third key.
        let (new, _) = add(&new, Context::List, Action::Refresh, k("ctrl+r")).unwrap();
        assert_eq!(
            keymap(&new).keys_in(Context::List, Action::Refresh).len(),
            3
        );
    }

    #[test]
    fn adding_back_an_unbound_built_in_key_drops_the_unbinding() {
        let u = user(&[("list", "r", "unbound")]);
        let (new, edits) = add(&u, Context::List, Action::Refresh, k("r")).unwrap();
        assert_eq!(edits, vec![SettingEdit::remove("keys.list", "r")]);
        assert!(new.bindings.is_empty());
    }

    #[test]
    fn add_refuses_a_key_of_another_action_in_the_same_context_and_names_it() {
        let map = keymap(&KeySettings::default());
        let c = |ctx, a, key| capture_add(&map, ctx, a, k(key));
        assert_eq!(
            c(Context::List, Action::Refresh, "p"),
            Adding::Refused("p is already Pin (Chat list): delete it there first".to_string())
        );
        assert_eq!(c(Context::List, Action::Refresh, "r"), Adding::Same);
        assert_eq!(c(Context::List, Action::Refresh, "esc"), Adding::Cancel);
        assert_eq!(
            c(Context::Composer, Action::Send, "x"),
            Adding::Refused("x types text here: use a Ctrl or Alt key".to_string())
        );
        // The same key in a context that is never live with this one is no overlap.
        assert_eq!(c(Context::Select, Action::Reply, "p"), Adding::Free);
    }

    #[test]
    fn add_warns_when_a_key_overlaps_global_and_still_adds_it() {
        let map = keymap(&KeySettings::default());
        // Ctrl+F searches everywhere: on the chat list it would refresh instead.
        let Adding::Overlaps(note) = capture_add(&map, Context::List, Action::Refresh, k("ctrl+f"))
        else {
            panic!("expected an overlap warning");
        };
        assert_eq!(
            note,
            "Ctrl+F is Search everywhere: in Chat list it will be Refresh instead"
        );
        // A modal context never reads Global: no warning there.
        assert_eq!(
            capture_add(&map, Context::Help, Action::Back, k("ctrl+f")),
            Adding::Free
        );
        // Global the other way round: the chat list's own key wins there.
        let u = user(&[("list", "ctrl+g", "refresh")]);
        let map = keymap(&u);
        let Adding::Overlaps(note) =
            capture_add(&map, Context::Common, Action::Search, k("ctrl+g"))
        else {
            panic!("expected an overlap warning");
        };
        assert_eq!(note, "Ctrl+G is Refresh in Chat list, which wins there");
        let (new, _) = add(&u, Context::Common, Action::Search, k("ctrl+g")).unwrap();
        assert_eq!(
            keymap(&new).keys_in(Context::Common, Action::Search),
            vec![k("ctrl+g"), k("ctrl+f")]
        );
    }

    #[test]
    fn remove_key_deletes_the_chosen_key_and_keeps_the_others() {
        let u = user(&[("list", "f5", "refresh"), ("list", "ctrl+r", "refresh")]);
        let map = keymap(&u);
        assert_eq!(
            map.keys_in(Context::List, Action::Refresh).len(),
            3,
            "r, F5, Ctrl+R"
        );
        // A user key: its entry goes.
        let (new, edits) = remove_key(&u, Context::List, Action::Refresh, k("f5")).unwrap();
        assert_eq!(edits, vec![SettingEdit::remove("keys.list", "f5")]);
        let after = keymap(&new);
        assert_eq!(
            after.keys_in(Context::List, Action::Refresh),
            vec![k("ctrl+r"), k("r")]
        );
        // A built-in key: written as unbound.
        let (new, edits) = remove_key(&new, Context::List, Action::Refresh, k("r")).unwrap();
        assert_eq!(edits, vec![set("keys.list", "r", "unbound")]);
        assert_eq!(
            keymap(&new).keys_in(Context::List, Action::Refresh),
            vec![k("ctrl+r")]
        );
        // Locked keys stay, with the reason.
        assert_eq!(
            remove_key(&u, Context::Common, Action::Quit, k("ctrl+c")).unwrap_err(),
            "Ctrl+C always quits"
        );
        assert_eq!(
            remove_key(&u, Context::Help, Action::Back, k("esc")).unwrap_err(),
            "Esc always backs out here"
        );
    }

    #[test]
    fn removing_a_moved_built_in_key_does_not_bring_its_old_action_back() {
        // p was moved from Pin to Refresh. Deleting it from Refresh leaves it doing nothing,
        // not pinning again.
        let u = user(&[("list", "p", "refresh")]);
        let map = keymap(&u);
        assert_eq!(map.lookup(Context::List, k("p")), Some(Action::Refresh));
        let (new, edits) = remove_key(&u, Context::List, Action::Refresh, k("p")).unwrap();
        assert_eq!(edits, vec![set("keys.list", "p", "unbound")]);
        assert_eq!(keymap(&new).lookup(Context::List, k("p")), None);
    }

    #[test]
    fn reset_one_drops_its_entries_and_its_unbindings() {
        let u = user(&[
            ("list", "f5", "refresh"),
            ("list", "r", "unbound"),
            ("list", "p", "pin"),
            ("list", "x", "unbound"),
        ]);
        let (new, edits) = reset_action(&u, Context::List, Action::Refresh).unwrap();
        assert_eq!(
            edits,
            vec![
                SettingEdit::remove("keys.list", "f5"),
                SettingEdit::remove("keys.list", "r"),
            ]
        );
        let after = keymap(&new);
        assert_eq!(after.lookup(Context::List, k("r")), Some(Action::Refresh));
        assert_eq!(
            after.lookup(Context::List, k("x")),
            None,
            "other unbinds stay"
        );
    }

    #[test]
    fn reset_all_clears_every_binding() {
        let u = user(&[("list", "f5", "refresh"), ("select", "x", "reply")]);
        let (new, edits) = reset_all(&u).unwrap();
        assert_eq!(
            edits,
            vec![
                SettingEdit::remove("keys.list", "f5"),
                SettingEdit::remove("keys.select", "x"),
            ]
        );
        assert!(new.bindings.is_empty());
    }

    #[test]
    fn every_edit_leaves_a_keymap_with_no_new_errors() {
        let u = KeySettings::default();
        let map = keymap(&u);
        for ctx in Context::ALL {
            for a in crate::keymap::actions_for(ctx, &u) {
                for key in ["f9", "ctrl+n", "alt+z"] {
                    if let Captured::Bind = capture(&map, ctx, a, k(key)) {
                        let (new, _) = bind(&map, &u, ctx, a, k(key)).unwrap();
                        assert_eq!(errors(&new), 0, "{ctx:?} {a:?} {key}");
                    }
                }
            }
        }
    }
}
