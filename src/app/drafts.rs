//! The composers' drafts as the app holds them, with a revision that
//! moves whenever what is saved of them may have, so saving them needs no
//! look at every draft on every frame.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use super::Draft;

/// Every composer's draft by its key ([`App::draft_key`](super::App::draft_key)).
///
/// Each change goes through a method that moves [`Drafts::revision`].
/// The composers edit a draft in place for a whole frame, so they
/// [`take`](Drafts::take) it and [`put_back`](Drafts::put_back) it, which
/// moves the revision only when what is saved of it changed.
#[derive(Default)]
pub struct Drafts {
    map: HashMap<String, Draft>,
    revision: u64,
}

/// A draft taken out to be edited, and what to compare it with when it
/// is put back.
pub struct Taken {
    /// The draft, to edit.
    pub draft: Draft,
    kept: u64,
}

/// What is saved of a draft (see [`crate::drafts::Saved`]), as a number:
/// only the composers on screen are looked at, and they lay their text
/// out each frame anyway.
fn kept(draft: &Draft) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&draft.text, &draft.mentions, draft.broadcast).hash(&mut hasher);
    hasher.finish()
}

impl Drafts {
    /// A number that changes whenever a draft may have been saved
    /// differently: written, sent, dropped or added.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn changed(&mut self) {
        self.revision = crate::revision::next();
    }

    /// The draft under `key`, if there is one.
    pub fn get(&self, key: &str) -> Option<&Draft> {
        self.map.get(key)
    }

    /// Every draft and its key, in no order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Draft)> {
        self.map.iter()
    }

    /// The draft under `key`, made if missing, to change.
    pub fn edit(&mut self, key: String) -> &mut Draft {
        self.changed();
        self.map.entry(key).or_default()
    }

    /// Puts `draft` under `key`.
    pub fn insert(&mut self, key: String, draft: Draft) {
        self.changed();
        self.map.insert(key, draft);
    }

    /// Takes the draft under `key` away for good.
    pub fn remove(&mut self, key: &str) -> Option<Draft> {
        let removed = self.map.remove(key);
        if removed.is_some() {
            self.changed();
        }
        removed
    }

    /// Keeps only the drafts `keep` lets through.
    pub fn retain(&mut self, keep: impl FnMut(&String, &mut Draft) -> bool) {
        let before = self.map.len();
        self.map.retain(keep);
        if self.map.len() != before {
            self.changed();
        }
    }

    /// The draft under `key` (an empty one if missing), out of the store
    /// to be edited while the rest of the app is borrowed. Hand it to
    /// [`Drafts::put_back`] once done.
    pub fn take(&mut self, key: &str) -> Taken {
        let draft = self.map.remove(key).unwrap_or_default();
        Taken {
            kept: kept(&draft),
            draft,
        }
    }

    /// Returns a [`Drafts::take`]n draft, moving the revision only if
    /// what is saved of it changed meanwhile.
    pub fn put_back(&mut self, key: String, taken: Taken) {
        if kept(&taken.draft) != taken.kept {
            self.changed();
        }
        self.map.insert(key, taken.draft);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_left_alone_keeps_the_revision() {
        let mut drafts = Drafts::default();
        drafts.insert("T1/C1".into(), Draft::default());
        let revision = drafts.revision();
        for _ in 0..3 {
            // A frame: the composer takes it, draws it, puts it back.
            let mut taken = drafts.take("T1/C1");
            taken.draft.selected = 2;
            taken.draft.suggesting = true;
            drafts.put_back("T1/C1".into(), taken);
            let _ = drafts.get("T1/C1");
            let _ = drafts.iter().count();
        }
        assert_eq!(drafts.revision(), revision, "nothing saved changed");
        let taken = drafts.take("T1/C9");
        drafts.put_back("T1/C9".into(), taken);
        assert_eq!(drafts.revision(), revision, "an empty composer opened");
        drafts.remove("T1/C404");
        drafts.retain(|_, _| true);
        assert_eq!(drafts.revision(), revision, "nothing went");
    }

    #[test]
    fn every_saved_change_moves_the_revision() {
        let mut drafts = Drafts::default();
        let mut revisions = vec![drafts.revision()];
        let mut step = |drafts: &Drafts| {
            assert!(
                !revisions.contains(&drafts.revision()),
                "step {} kept the revision",
                revisions.len()
            );
            revisions.push(drafts.revision());
        };
        let mut taken = drafts.take("T1/C1");
        taken.draft.text.push('h');
        drafts.put_back("T1/C1".into(), taken);
        step(&drafts);
        let mut taken = drafts.take("T1/C1");
        taken.draft.text = "g".into();
        drafts.put_back("T1/C1".into(), taken);
        step(&drafts);
        let mut taken = drafts.take("T1/C1");
        taken.draft.mentions.push(("@Ann".into(), "<@U1>".into()));
        drafts.put_back("T1/C1".into(), taken);
        step(&drafts);
        let mut taken = drafts.take("T1/C1");
        taken.draft.broadcast = true;
        drafts.put_back("T1/C1".into(), taken);
        step(&drafts);
        drafts.edit("T1/C2".into()).text.push('x');
        step(&drafts);
        drafts.insert("T1/C3".into(), Draft::default());
        step(&drafts);
        drafts.remove("T1/C3");
        step(&drafts);
        drafts.retain(|key, _| key != "T1/C2");
        step(&drafts);
        assert_eq!(drafts.get("T1/C1").map(|d| d.text.as_str()), Some("g"));
    }
}
