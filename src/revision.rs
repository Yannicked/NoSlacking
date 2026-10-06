//! Revision numbers, so a frame can tell in one comparison whether data
//! it drew from changed, instead of hashing all of it every frame.
//!
//! Every revision comes from one counter for the whole program, so two
//! revisions are equal only when nothing changed in between: a value made
//! afresh (a workspace signed in again, a list replaced whole) never takes
//! a number something remembered was drawn from. And of several values,
//! the largest revision changes whenever any of theirs does.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A revision never handed out before.
pub fn next() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A value with a revision that changes whenever it may have: on every
/// mutable borrow. Reading goes through [`Deref`], changing through
/// [`DerefMut`], so no change can go unnoticed; a borrow that changes
/// nothing still counts, which costs one extra rebuild and nothing worse.
pub struct Revised<T> {
    value: T,
    revision: u64,
}

impl<T> Revised<T> {
    /// `value`, at a fresh revision.
    pub fn new(value: T) -> Self {
        Self {
            value,
            revision: next(),
        }
    }

    /// The revision of the value as it is now.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

impl<T> Deref for Revised<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Revised<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.revision = next();
        &mut self.value
    }
}

impl<T> From<T> for Revised<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default> Default for Revised<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// A copy holds the same value, so it keeps the revision: anything drawn
/// from one is right for the other until either changes.
impl<T: Clone> Clone for Revised<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            revision: self.revision,
        }
    }
}

/// Compares the values only.
impl<T: PartialEq> PartialEq for Revised<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

/// Prints the value only.
impl<T: std::fmt::Debug> std::fmt::Debug for Revised<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

impl<'a, T> IntoIterator for &'a Revised<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&self.value).into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_keeps_the_revision_and_changing_moves_it() {
        let mut list = Revised::new(vec![1, 2, 3]);
        let first = list.revision();
        assert_eq!(list.len(), 3);
        assert_eq!(list.iter().sum::<i32>(), 6);
        assert_eq!((&list).into_iter().count(), 3);
        assert_eq!(list.revision(), first, "reads leave it alone");
        list.push(4);
        let pushed = list.revision();
        assert_ne!(pushed, first);
        list[0] = 9;
        assert_ne!(list.revision(), pushed);
    }

    #[test]
    fn a_value_made_afresh_never_reuses_a_revision() {
        let one = Revised::new(0);
        let two = Revised::new(0);
        assert_ne!(one.revision(), two.revision());
        let copy = one.clone();
        assert_eq!(copy.revision(), one.revision(), "a copy is the same data");
        assert!(two.revision() > one.revision(), "the newest is the largest");
    }
}
