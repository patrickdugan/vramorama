//! Respawns: the same command, started again under the same parent, after it was killed.
//!
//! A long-running `watch` or `gui` sees a process die and a new one with the same command line
//! appear under the same live parent. That is the signature of a supervisor, and it is why
//! killing the child in Task Manager frees nothing for long.

use std::collections::{HashMap, HashSet};

/// A process identity: pid and creation time (pids are reused).
pub type Ident = (u32, u64);

#[derive(Hash, PartialEq, Eq, Clone)]
struct Key {
    parent: Ident,
    name: String,
    cmd: String,
}

#[derive(Default)]
pub struct Tracker {
    seen: HashMap<Key, Vec<Ident>>,
}

/// A command that was started again under the same parent.
#[derive(Clone, Debug, PartialEq)]
pub struct Respawn {
    pub parent: Ident,
    pub name: String,
    pub cmd: String,
    /// How many times it was started again since the tracker first saw it.
    pub restarts: usize,
}

impl Tracker {
    /// Record that `child` is running under `parent` with this command line.
    pub fn observe(&mut self, parent: Ident, name: &str, cmd: &str, child: Ident) {
        let key = Key { parent, name: name.to_ascii_lowercase(), cmd: cmd.to_string() };
        let gens = self.seen.entry(key).or_default();
        if !gens.contains(&child) {
            gens.push(child);
        }
    }

    /// Forget parents that have exited, so a long-running `gui` does not keep every job it ever saw.
    pub fn retain_parents(&mut self, live: &HashSet<Ident>) {
        self.seen.retain(|k, _| live.contains(&k.parent));
    }

    /// Commands started again under `parent`, most restarts first.
    pub fn respawns(&self) -> Vec<Respawn> {
        let mut out: Vec<Respawn> = self
            .seen
            .iter()
            .filter(|(_, g)| g.len() > 1)
            .map(|(k, g)| Respawn { parent: k.parent, name: k.name.clone(), cmd: k.cmd.clone(), restarts: g.len() - 1 })
            .collect();
        out.sort_by(|a, b| b.restarts.cmp(&a.restarts).then(a.parent.cmp(&b.parent)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_started_again_under_the_same_parent_is_a_respawn() {
        let mut t = Tracker::default();
        let parent = (100, 5);
        t.observe(parent, "llama-server.exe", "llama-server -m x.gguf", (200, 10));
        t.observe(parent, "llama-server.exe", "llama-server -m x.gguf", (200, 10));
        assert!(t.respawns().is_empty(), "the same process seen twice is not a restart");
        t.observe(parent, "LLAMA-SERVER.EXE", "llama-server -m x.gguf", (300, 20));
        t.observe(parent, "llama-server.exe", "llama-server -m x.gguf", (400, 30));
        let r = t.respawns();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].restarts, 2);
    }

    #[test]
    fn a_reused_pid_is_a_new_process() {
        let mut t = Tracker::default();
        t.observe((1, 1), "a.exe", "a", (50, 10));
        t.observe((1, 1), "a.exe", "a", (50, 99));
        assert_eq!(t.respawns()[0].restarts, 1);
    }

    #[test]
    fn different_commands_or_parents_do_not_add_up() {
        let mut t = Tracker::default();
        t.observe((1, 1), "a.exe", "a --x", (10, 1));
        t.observe((1, 1), "a.exe", "a --y", (11, 2));
        t.observe((2, 1), "a.exe", "a --x", (12, 3));
        assert!(t.respawns().is_empty());
    }

    #[test]
    fn exited_parents_are_forgotten() {
        let mut t = Tracker::default();
        t.observe((1, 1), "a.exe", "a", (10, 1));
        t.observe((1, 1), "a.exe", "a", (11, 2));
        t.retain_parents(&HashSet::new());
        assert!(t.respawns().is_empty());
    }
}
