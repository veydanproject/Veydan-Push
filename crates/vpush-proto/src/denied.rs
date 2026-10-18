//! The keys denied on every server of the network (Veydan Cloud,
//! `services/cloud/spec/cloud-api.md` §14): a file the agent of vcloud
//! lays on the host, one key a line, which vpush reads to refuse the
//! registration of a device and vguard reads to refuse a key at the relay.
//! vcloud judges the file before it is laid and the agent judges it again;
//! here a file that does not read never replaces one that did.
//!
//! The shape: one key a line, 64 lowercase hex (the x-only key, as the
//! `pubkey` of an event); `#` starts a comment; empty lines are nothing.
//! An empty file and no file both deny nobody: a host without the file
//! (a kit before the denials, a vcloud that has not written yet) works as
//! before.
//!
//! The groups closed on our relays (`cloud-api.md` §14.7) come the same
//! way, in a file of their own beside it (`denied-groups.txt`, read by
//! vguard alone): one id of a group a line (the `h` of its events), 1 to
//! 128 printable ASCII characters without a space; `#` starts a comment.
//! One reader serves both ([`DeniedList`]); only the shape of a line and
//! the comparison differ (a key is hex in any case, an id is as it is).
//!
//! Only std lives here, as serde does above: a client of this crate pulls
//! in nothing of the server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

/// The name of the file wherever it lies.
pub const FILE_NAME: &str = "denied-keys.txt";
/// The name of the file of the closed groups, beside it.
pub const GROUPS_FILE_NAME: &str = "denied-groups.txt";
/// The longest id of a group a line holds.
pub const MAX_GROUP_ID: usize = 128;
/// How often a reader looks at the file for a change of its mtime: the
/// agent lays a new one within a minute of a denial, and a reader one
/// minute later at most.
pub const RELOAD_EVERY: Duration = Duration::from_secs(60);
/// The most keys a file may carry: far above any real list, so that a
/// file gone wrong is refused instead of filling the memory.
pub const MAX_KEYS: usize = 1_000_000;

/// Is `s` a key as the file carries it?
fn is_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Is `s` an id of a group as the file carries it?
fn is_group_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_GROUP_ID && s.bytes().all(|b| b.is_ascii_graphic())
}

/// What a line of a list is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shape {
    /// A key, 64 lowercase hex: `denied-keys.txt`.
    #[default]
    Keys,
    /// An id of a group: `denied-groups.txt`.
    Groups,
}

/// The keys of a file's text, or the first line that is not one.
pub fn parse(text: &str) -> Result<HashSet<String>, String> {
    parse_as(Shape::Keys, text)
}

/// The ids of a file of closed groups, or the first line that is not one.
pub fn parse_groups(text: &str) -> Result<HashSet<String>, String> {
    parse_as(Shape::Groups, text)
}

fn parse_as(shape: Shape, text: &str) -> Result<HashSet<String>, String> {
    let mut items = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let l = line.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        match shape {
            Shape::Keys if !is_key(l) => return Err(format!("line {}: not a key as 64 lowercase hex", n + 1)),
            Shape::Groups if !is_group_id(l) => return Err(format!("line {}: not an id of a group (1 to {MAX_GROUP_ID} printable characters)", n + 1)),
            _ => {}
        }
        items.insert(l.to_string());
        if items.len() > MAX_KEYS {
            return Err(format!("more than {MAX_KEYS} lines"));
        }
    }
    Ok(items)
}

/// What a look at the file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refreshed {
    /// No file is configured, or it did not change.
    Same,
    /// A new list was read: this many keys.
    Loaded(usize),
    /// The file is not there (any more): nobody is denied.
    Missing,
    /// The file changed but does not read; the list before stays.
    Refused(String),
}

#[derive(Default)]
struct State {
    keys: HashSet<String>,
    /// The mtime and the size of the file read last.
    stamp: Option<(SystemTime, u64)>,
    /// Whether the file was there at the last look.
    present: bool,
    /// Whether it was looked at yet.
    looked: bool,
}

/// The denied keys, or the closed groups, of one file, re-read when it
/// changes.
#[derive(Default)]
pub struct DeniedList {
    path: Option<PathBuf>,
    shape: Shape,
    state: RwLock<State>,
}

/// The list of the denied keys.
pub type DeniedKeys = DeniedList;
/// The list of the closed groups.
pub type DeniedGroups = DeniedList;

impl DeniedList {
    /// No file: nobody is denied, ever (the config names none).
    pub fn none() -> Self {
        Self::default()
    }

    /// The keys of `path`, read now; a missing file denies nobody until it
    /// is laid.
    pub fn from_file(path: &Path) -> Self {
        Self::read(path, Shape::Keys)
    }

    /// The closed groups of `path`, read now; a missing file closes none.
    pub fn groups_from_file(path: &Path) -> Self {
        Self::read(path, Shape::Groups)
    }

    fn read(path: &Path, shape: Shape) -> Self {
        let denied = Self { path: Some(path.to_path_buf()), shape, state: RwLock::default() };
        denied.refresh();
        denied
    }

    /// A fixed list of keys, for the tests of the readers.
    pub fn of(keys: &[&str]) -> Self {
        let state = State { keys: keys.iter().map(|k| k.to_ascii_lowercase()).collect(), stamp: None, present: true, looked: true };
        Self { path: None, shape: Shape::Keys, state: RwLock::new(state) }
    }

    /// A fixed list of groups, for the tests of the readers.
    pub fn groups_of(ids: &[&str]) -> Self {
        let state = State { keys: ids.iter().map(|g| g.to_string()).collect(), stamp: None, present: true, looked: true };
        Self { path: None, shape: Shape::Groups, state: RwLock::new(state) }
    }

    /// The file read, when one is configured.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Is `pubkey` (hex) denied, or the group `pubkey` names closed? Nothing
    /// is allocated while the list is empty, which it is on almost every
    /// look. A key is compared in any case, an id of a group as it is: the
    /// `h` of a group is a word of its own, not hex.
    pub fn contains(&self, pubkey: &str) -> bool {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.keys.is_empty() {
            return false;
        }
        if self.shape == Shape::Keys && pubkey.bytes().any(|b| b.is_ascii_uppercase()) {
            state.keys.contains(&pubkey.to_ascii_lowercase())
        } else {
            state.keys.contains(pubkey)
        }
    }

    /// Whether nobody is denied.
    pub fn is_empty(&self) -> bool {
        self.state.read().unwrap_or_else(|e| e.into_inner()).keys.is_empty()
    }

    /// How many keys are denied.
    pub fn len(&self) -> usize {
        self.state.read().unwrap_or_else(|e| e.into_inner()).keys.len()
    }

    /// One look at the file: read again when its mtime or size changed.
    /// The caller looks every [`RELOAD_EVERY`].
    pub fn refresh(&self) -> Refreshed {
        let Some(path) = &self.path else { return Refreshed::Same };
        let stamp = std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let Some(stamp) = stamp else {
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            let was = state.present || !state.looked;
            state.keys.clear();
            state.stamp = None;
            state.present = false;
            state.looked = true;
            return if was { Refreshed::Missing } else { Refreshed::Same };
        };
        if self.state.read().unwrap_or_else(|e| e.into_inner()).stamp == Some(stamp) {
            return Refreshed::Same;
        }
        let read = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|text| parse_as(self.shape, &text));
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        // Remembered either way: a file that does not read is not read
        // again every minute, only when it changes once more.
        state.stamp = Some(stamp);
        state.present = true;
        state.looked = true;
        match read {
            Ok(keys) => {
                let n = keys.len();
                state.keys = keys;
                Refreshed::Loaded(n)
            }
            Err(why) => Refreshed::Refused(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: &str) -> String {
        byte.repeat(32)
    }

    #[test]
    fn a_file_is_one_key_a_line_with_comments() {
        let keys = parse(&format!("# laid by vcloud\n{}\n\n{} # report 7\n{}\n", key("ab"), key("cd"), key("ab"))).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(parse("").unwrap().is_empty());
        assert_eq!(parse(&format!("{}\nnpub1xyz\n", key("ab"))).unwrap_err(), "line 2: not a key as 64 lowercase hex");
        assert!(parse(&key("AB")).is_err(), "lowercase, as vcloud writes it");
        assert!(parse(&key("a")).is_err());
    }

    #[test]
    fn a_file_of_groups_is_one_id_a_line_compared_as_it_is() {
        let ids = parse_groups(&format!("# closed by vcloud\n{}\nOther-Client_1 # report 9\n", "9f".repeat(32))).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(parse_groups("a b\n").is_err() && parse_groups(&"x".repeat(MAX_GROUP_ID + 1)).is_err());
        assert!(parse(&format!("{}\n", "Other-Client_1")).is_err(), "a key file takes no group");
        let groups = DeniedGroups::groups_of(&["Other-Client_1"]);
        assert!(groups.contains("Other-Client_1") && !groups.contains("other-client_1"), "an id is not folded");
        let dir = std::env::temp_dir().join(format!("vpush-proto-groups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GROUPS_FILE_NAME);
        std::fs::write(&path, "g1\n").unwrap();
        let groups = DeniedGroups::groups_from_file(&path);
        assert!(groups.contains("g1") && groups.len() == 1);
        std::fs::write(&path, "g1\ng2\n").unwrap();
        assert_eq!(groups.refresh(), Refreshed::Loaded(2));
        std::fs::write(&path, "g 3\n").unwrap();
        assert!(matches!(groups.refresh(), Refreshed::Refused(_)));
        assert!(groups.contains("g2"), "the list before stays");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_is_read_again_when_it_changes_and_a_bad_one_keeps_the_list() {
        let dir = std::env::temp_dir().join(format!("vpush-proto-denied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        // No file yet: nobody denied.
        let denied = DeniedKeys::from_file(&path);
        assert!(denied.is_empty() && !denied.contains(&key("ab")));
        assert_eq!(denied.refresh(), Refreshed::Same, "still missing");
        std::fs::write(&path, format!("{}\n", key("ab"))).unwrap();
        assert_eq!(denied.refresh(), Refreshed::Loaded(1));
        assert!(denied.contains(&key("ab")) && denied.contains(&key("AB")) && !denied.contains(&key("cd")));
        assert_eq!(denied.refresh(), Refreshed::Same, "unchanged");
        // A file that does not read: the list before stays.
        std::fs::write(&path, "garbage\n").unwrap();
        assert!(matches!(denied.refresh(), Refreshed::Refused(_)));
        assert!(denied.contains(&key("ab")));
        assert_eq!(denied.refresh(), Refreshed::Same, "not read again until it changes");
        // A new list, then none.
        std::fs::write(&path, format!("{}\n{}\n", key("cd"), key("ef"))).unwrap();
        assert_eq!(denied.refresh(), Refreshed::Loaded(2));
        assert!(!denied.contains(&key("ab")) && denied.contains(&key("ef")));
        assert_eq!(denied.len(), 2);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(denied.refresh(), Refreshed::Missing);
        assert!(denied.is_empty());
        // No file configured at all.
        let none = DeniedKeys::none();
        assert_eq!((none.refresh(), none.contains(&key("ab")), none.path()), (Refreshed::Same, false, None));
        assert!(DeniedKeys::of(&[&key("AB")]).contains(&key("ab")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
