//! Lazy file-row selection for one immutable, ordered listing.
//!
//! A [`ListingRevision`] identifies the *ordering and membership*, not a path.
//! Mint a fresh token after navigation, refresh, sorting or filtering, even when
//! row count is unchanged. Every index-bearing read/action checks that token.
//! [`Selection::reset`] deliberately clears indices; preserving selection needs
//! an explicit native-identity remap by the listing owner, outside this model.
//! Never resolve these indices against a different listing or display strings.
//!
//! Runs are sorted, disjoint, nonadjacent half-open ranges. Select All takes
//! constant space/time in row count; membership is O(log r), mutations O(r), and
//! count is O(1), where r is the number of disjoint runs. Additive extension keeps
//! a baseline copy (O(r)) so reversing an extension can shrink its live range.
//! No filesystem access, per-row bitset, viewport state or unsafe code is used.
//!
//! Ctrl+A selects without scrolling, Ctrl+arrows move only the cursor, and
//! Ctrl+Space toggles the cursor row. Modifier behavior:
//!
//! * Plain click/move replaces selection and sets cursor and anchor.
//! * Ctrl+click toggles the target and sets cursor and anchor.
//! * Shift replaces selection with the inclusive anchor-to-target range.
//! * Ctrl+Shift preserves the selection from the start of that range gesture
//!   and unions the current anchor-to-target range; reversing removes only the
//!   transient extension. Switching between Shift and Ctrl+Shift starts a new
//!   gesture. A non-range action ends the gesture.
//! * Ctrl+movement moves cursor only and sets the anchor to that new cursor.
//! * A missing anchor starts at the old cursor, or the target if no cursor exists.
//! * Clear preserves the cursor (for a later toggle), sets anchor to it and ends
//!   extension. Select All preserves cursor/anchor and ends extension. Neither
//!   operation implies scrolling; only the host owns scroll/reveal decisions.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Opaque, process-local identity for exactly one immutable listing ordering.
/// Clones refer to the same revision; `new` always creates a distinct revision.
#[derive(Clone, Debug)]
pub struct ListingRevision(Arc<()>);

impl ListingRevision {
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl Default for ListingRevision {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for ListingRevision {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ListingRevision {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub control: bool,
    pub shift: bool,
}

/// Index-space navigation. The host resolves view-specific axes/page sizes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Movement {
    First,
    Last,
    /// Saturates at listing boundaries; without a cursor starts at row zero.
    Offset(isize),
    /// An exact index, checked rather than silently clamped.
    To(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Click {
        index: usize,
        modifiers: Modifiers,
    },
    Move {
        movement: Movement,
        modifiers: Modifiers,
    },
    ToggleCursor,
    SelectAll,
    Clear,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Change {
    pub selection_changed: bool,
    pub cursor_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionError {
    StaleRevision,
    OutOfBounds { index: usize, len: usize },
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleRevision => f.write_str("selection belongs to a different listing revision"),
            Self::OutOfBounds { index, len } => {
                write!(f, "selection index {index} is outside listing length {len}")
            }
        }
    }
}
impl std::error::Error for SelectionError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Runs {
    ranges: Vec<Range<usize>>,
    count: usize,
}

impl Runs {
    fn contains(&self, index: usize) -> bool {
        let next = self.ranges.partition_point(|range| range.end <= index);
        self.ranges
            .get(next)
            .is_some_and(|range| range.start <= index)
    }

    fn replace(&mut self, range: Range<usize>) -> bool {
        let empty = range.is_empty();
        if (empty && self.ranges.is_empty())
            || (!empty && self.ranges.len() == 1 && self.ranges[0] == range)
        {
            return false;
        }
        self.ranges.clear();
        self.count = range.end - range.start;
        if !empty {
            self.ranges.push(range);
        }
        true
    }

    fn add(&mut self, range: Range<usize>) {
        let first = self.ranges.partition_point(|old| old.end < range.start);
        let mut start = range.start;
        let mut end = range.end;
        let mut last = first;
        while let Some(old) = self.ranges.get(last).filter(|old| old.start <= end) {
            start = start.min(old.start);
            end = end.max(old.end);
            self.count -= old.end - old.start;
            last += 1;
        }
        self.ranges.splice(first..last, std::iter::once(start..end));
        self.count += end - start;
    }

    fn toggle(&mut self, index: usize) {
        let next = self.ranges.partition_point(|range| range.end <= index);
        let Some(range) = self.ranges.get(next).filter(|range| range.start <= index) else {
            self.add(index..index + 1);
            return;
        };
        let range = range.clone();
        self.count -= 1;
        if range.start == index && range.end == index + 1 {
            self.ranges.remove(next);
        } else if range.start == index {
            self.ranges[next].start += 1;
        } else if range.end == index + 1 {
            self.ranges[next].end -= 1;
        } else {
            self.ranges[next].end = index;
            self.ranges.insert(next + 1, index + 1..range.end);
        }
    }
}

#[derive(Debug)]
struct Extension {
    additive: bool,
    baseline: Runs,
}

#[derive(Debug)]
pub struct Selection {
    revision: ListingRevision,
    len: usize,
    selected: Runs,
    cursor: Option<usize>,
    anchor: Option<usize>,
    extension: Option<Extension>,
}

impl Selection {
    pub fn new(revision: ListingRevision, len: usize) -> Self {
        Self {
            revision,
            len,
            selected: Runs::default(),
            cursor: None,
            anchor: None,
            extension: None,
        }
    }

    /// Explicitly discard old row identities. The caller must supply the token
    /// minted for the new immutable ordering, never recycle a prior token.
    pub fn reset(&mut self, revision: ListingRevision, len: usize) {
        *self = Self::new(revision, len);
    }

    pub fn revision(&self) -> &ListingRevision {
        &self.revision
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn count(&self) -> usize {
        self.selected.count
    }

    pub fn cursor(&self, revision: &ListingRevision) -> Result<Option<usize>, SelectionError> {
        self.check(revision)?;
        Ok(self.cursor)
    }
    pub fn anchor(&self, revision: &ListingRevision) -> Result<Option<usize>, SelectionError> {
        self.check(revision)?;
        Ok(self.anchor)
    }
    pub fn contains(
        &self,
        revision: &ListingRevision,
        index: usize,
    ) -> Result<bool, SelectionError> {
        self.check(revision)?;
        self.check_index(index)?;
        Ok(self.selected.contains(index))
    }
    /// Borrow lazy runs; materialize native entries on a worker while retaining
    /// the matching immutable listing. This never allocates one index per row.
    pub fn ranges(&self, revision: &ListingRevision) -> Result<&[Range<usize>], SelectionError> {
        self.check(revision)?;
        Ok(&self.selected.ranges)
    }

    pub fn apply(
        &mut self,
        revision: &ListingRevision,
        action: Action,
    ) -> Result<Change, SelectionError> {
        self.check(revision)?;
        // Validate before changing cursor, anchor, ranges or gesture state.
        let target = match action {
            Action::Click { index, .. }
            | Action::Move {
                movement: Movement::To(index),
                ..
            } => {
                self.check_index(index)?;
                Some(index)
            }
            Action::Move { movement, .. } if self.len > 0 => Some(match movement {
                Movement::First => 0,
                Movement::Last => self.len - 1,
                Movement::Offset(offset) => self.cursor.map_or(0, |cursor| {
                    cursor.saturating_add_signed(offset).min(self.len - 1)
                }),
                Movement::To(_) => unreachable!("validated above"),
            }),
            _ => None,
        };
        let old_cursor = self.cursor;
        let selection_changed = match action {
            Action::SelectAll => {
                self.extension = None;
                self.selected.replace(0..self.len)
            }
            Action::Clear => {
                self.extension = None;
                self.anchor = self.cursor;
                self.selected.replace(0..0)
            }
            Action::ToggleCursor => {
                self.extension = None;
                self.anchor = self.cursor;
                if let Some(cursor) = self.cursor {
                    self.selected.toggle(cursor);
                    true
                } else {
                    false
                }
            }
            Action::Click { modifiers, .. } | Action::Move { modifiers, .. } => {
                if let Some(target) = target {
                    let is_move = matches!(action, Action::Move { .. });
                    let changed = if modifiers.shift {
                        self.extend(target, modifiers.control)
                    } else {
                        self.extension = None;
                        self.anchor = Some(target);
                        if modifiers.control && is_move {
                            false
                        } else if modifiers.control {
                            self.selected.toggle(target);
                            true
                        } else {
                            self.selected.replace(target..target + 1)
                        }
                    };
                    self.cursor = Some(target);
                    changed
                } else {
                    false
                }
            }
        };
        Ok(Change {
            selection_changed,
            cursor_changed: self.cursor != old_cursor,
        })
    }

    fn extend(&mut self, target: usize, additive: bool) -> bool {
        let anchor = *self.anchor.get_or_insert(self.cursor.unwrap_or(target));
        if self
            .extension
            .as_ref()
            .is_none_or(|old| old.additive != additive)
        {
            self.extension = Some(Extension {
                additive,
                baseline: if additive {
                    self.selected.clone()
                } else {
                    Runs::default()
                },
            });
        }
        let range = anchor.min(target)..anchor.max(target) + 1;
        if additive {
            let mut selected = self.extension.as_ref().unwrap().baseline.clone();
            selected.add(range);
            let changed = self.selected != selected;
            self.selected = selected;
            changed
        } else {
            self.selected.replace(range)
        }
    }

    fn check(&self, revision: &ListingRevision) -> Result<(), SelectionError> {
        if self.revision == *revision {
            Ok(())
        } else {
            Err(SelectionError::StaleRevision)
        }
    }
    fn check_index(&self, index: usize) -> Result<(), SelectionError> {
        if index < self.len {
            Ok(())
        } else {
            Err(SelectionError::OutOfBounds {
                index,
                len: self.len,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Modifiers = Modifiers {
        control: false,
        shift: false,
    };
    const CTRL: Modifiers = Modifiers {
        control: true,
        shift: false,
    };
    const SHIFT: Modifiers = Modifiers {
        control: false,
        shift: true,
    };
    const BOTH: Modifiers = Modifiers {
        control: true,
        shift: true,
    };

    fn click(index: usize, modifiers: Modifiers) -> Action {
        Action::Click { index, modifiers }
    }
    fn move_to(index: usize, modifiers: Modifiers) -> Action {
        Action::Move {
            movement: Movement::To(index),
            modifiers,
        }
    }
    fn indices(selection: &Selection, revision: &ListingRevision) -> Vec<usize> {
        selection
            .ranges(revision)
            .unwrap()
            .iter()
            .cloned()
            .flatten()
            .collect()
    }

    #[test]
    fn cursor_is_distinct_and_entering_a_listing_selects_nothing() {
        let rev = ListingRevision::new();
        let mut selection = Selection::new(rev.clone(), 10);
        assert_eq!(selection.count(), 0);
        assert_eq!(selection.cursor(&rev), Ok(None));
        assert_eq!(
            selection.apply(&rev, Action::ToggleCursor),
            Ok(Change::default())
        );
        selection.apply(&rev, move_to(4, CTRL)).unwrap();
        assert_eq!(selection.cursor(&rev), Ok(Some(4)));
        assert_eq!(selection.count(), 0);
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(indices(&selection, &rev), [4]);
        selection.apply(&rev, move_to(5, CTRL)).unwrap();
        assert_eq!(indices(&selection, &rev), [4]);
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(indices(&selection, &rev), [4, 5]);
        selection.apply(&rev, move_to(8, PLAIN)).unwrap();
        assert_eq!(indices(&selection, &rev), [8]);
        selection.apply(&rev, Action::Clear).unwrap();
        assert_eq!(selection.cursor(&rev), Ok(Some(8)));
        assert_eq!(selection.anchor(&rev), Ok(Some(8)));
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn extension_reverses_and_additive_extension_preserves_only_its_baseline() {
        let rev = ListingRevision::new();
        let mut selection = Selection::new(rev.clone(), 12);
        selection.apply(&rev, click(1, PLAIN)).unwrap();
        selection.apply(&rev, click(5, CTRL)).unwrap();
        selection.apply(&rev, move_to(9, BOTH)).unwrap();
        assert_eq!(indices(&selection, &rev), [1, 5, 6, 7, 8, 9]);
        selection.apply(&rev, move_to(3, BOTH)).unwrap();
        assert_eq!(indices(&selection, &rev), [1, 3, 4, 5]);
        selection.apply(&rev, move_to(7, SHIFT)).unwrap();
        assert_eq!(indices(&selection, &rev), [5, 6, 7]);
        selection.apply(&rev, move_to(2, SHIFT)).unwrap();
        assert_eq!(indices(&selection, &rev), [2, 3, 4, 5]);
        assert_eq!(selection.anchor(&rev), Ok(Some(5)));
        selection.apply(&rev, move_to(10, CTRL)).unwrap();
        selection.apply(&rev, click(11, SHIFT)).unwrap();
        assert_eq!(indices(&selection, &rev), [10, 11]);
    }

    #[test]
    fn million_rows_remain_one_lazy_run_and_select_all_does_not_move_cursor() {
        let rev = ListingRevision::new();
        let mut selection = Selection::new(rev.clone(), 1_000_000);
        selection.apply(&rev, click(451_123, PLAIN)).unwrap();
        let change = selection.apply(&rev, Action::SelectAll).unwrap();
        assert_eq!(
            change,
            Change {
                selection_changed: true,
                cursor_changed: false
            }
        );
        assert_eq!(selection.cursor(&rev), Ok(Some(451_123)));
        assert_eq!(selection.anchor(&rev), Ok(Some(451_123)));
        assert_eq!(selection.count(), 1_000_000);
        assert_eq!(
            selection.ranges(&rev).unwrap(),
            std::slice::from_ref(&(0..1_000_000))
        );
        assert!(selection.selected.ranges.capacity() < 16);
        assert_eq!(
            selection.apply(&rev, Action::SelectAll),
            Ok(Change::default())
        );
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(selection.count(), 999_999);
        assert_eq!(
            selection.ranges(&rev).unwrap(),
            &[0..451_123, 451_124..1_000_000]
        );
        // No viewport input exists in the API: resizing cannot change selection.
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(
            selection.ranges(&rev).unwrap(),
            std::slice::from_ref(&(0..1_000_000))
        );
    }

    #[test]
    fn stale_and_invalid_actions_are_atomic_and_same_length_reset_clears_identity() {
        let old = ListingRevision::new();
        let new = ListingRevision::new();
        let mut selection = Selection::new(old.clone(), 8);
        selection.apply(&old, click(2, PLAIN)).unwrap();
        selection.apply(&old, move_to(6, BOTH)).unwrap();
        let before = format!("{selection:?}");
        for action in [
            Action::Clear,
            Action::SelectAll,
            Action::ToggleCursor,
            click(0, PLAIN),
            move_to(1, SHIFT),
        ] {
            assert_eq!(
                selection.apply(&new, action),
                Err(SelectionError::StaleRevision)
            );
        }
        for action in [click(8, PLAIN), move_to(usize::MAX, BOTH)] {
            assert!(matches!(
                selection.apply(&old, action),
                Err(SelectionError::OutOfBounds { .. })
            ));
        }
        assert_eq!(format!("{selection:?}"), before);
        assert_eq!(selection.cursor(&new), Err(SelectionError::StaleRevision));
        assert_eq!(selection.anchor(&new), Err(SelectionError::StaleRevision));
        assert_eq!(
            selection.contains(&new, 2),
            Err(SelectionError::StaleRevision)
        );
        assert_eq!(selection.ranges(&new), Err(SelectionError::StaleRevision));
        selection.reset(new.clone(), 8);
        assert_eq!(selection.count(), 0);
        assert_eq!(selection.cursor(&new), Ok(None));
        assert_eq!(selection.ranges(&old), Err(SelectionError::StaleRevision));
        assert_eq!(
            selection.contains(&new, 8),
            Err(SelectionError::OutOfBounds { index: 8, len: 8 })
        );
    }

    #[test]
    fn empty_and_maximum_length_lists_do_not_overflow() {
        let rev = ListingRevision::new();
        let mut selection = Selection::new(rev.clone(), 0);
        for action in [
            Action::Clear,
            Action::SelectAll,
            Action::ToggleCursor,
            Action::Move {
                movement: Movement::First,
                modifiers: PLAIN,
            },
            Action::Move {
                movement: Movement::Last,
                modifiers: BOTH,
            },
            Action::Move {
                movement: Movement::Offset(isize::MIN),
                modifiers: CTRL,
            },
        ] {
            assert_eq!(selection.apply(&rev, action), Ok(Change::default()));
        }
        assert!(selection.apply(&rev, click(0, PLAIN)).is_err());
        selection.reset(ListingRevision::new(), usize::MAX);
        let rev = selection.revision().clone();
        selection.apply(&rev, Action::SelectAll).unwrap();
        assert_eq!(selection.count(), usize::MAX);
        selection
            .apply(
                &rev,
                Action::Move {
                    movement: Movement::Last,
                    modifiers: CTRL,
                },
            )
            .unwrap();
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(selection.count(), usize::MAX - 1);
        selection.apply(&rev, Action::ToggleCursor).unwrap();
        assert_eq!(selection.count(), usize::MAX);
        selection
            .apply(
                &rev,
                Action::Move {
                    movement: Movement::Offset(isize::MAX),
                    modifiers: PLAIN,
                },
            )
            .unwrap();
        assert_eq!(selection.cursor(&rev), Ok(Some(usize::MAX - 1)));
        selection
            .apply(
                &rev,
                Action::Move {
                    movement: Movement::Offset(isize::MIN),
                    modifiers: SHIFT,
                },
            )
            .unwrap();
        assert_eq!(selection.count(), isize::MIN.unsigned_abs() + 1);
    }

    /// Intentionally simple per-row oracle, independent of the interval storage.
    struct Oracle {
        selected: Vec<bool>,
        cursor: Option<usize>,
        anchor: Option<usize>,
        extension: Option<(bool, Vec<bool>)>,
    }
    impl Oracle {
        fn new(len: usize) -> Self {
            Self {
                selected: vec![false; len],
                cursor: None,
                anchor: None,
                extension: None,
            }
        }
        fn apply(&mut self, action: Action) -> Result<Change, SelectionError> {
            let before = self.selected.clone();
            let old_cursor = self.cursor;
            let len = self.selected.len();
            match action {
                Action::SelectAll => {
                    self.selected.fill(true);
                    self.extension = None;
                }
                Action::Clear => {
                    self.selected.fill(false);
                    self.extension = None;
                    self.anchor = self.cursor;
                }
                Action::ToggleCursor => {
                    self.extension = None;
                    self.anchor = self.cursor;
                    if let Some(index) = self.cursor {
                        self.selected[index] ^= true;
                    }
                }
                Action::Click { .. } | Action::Move { .. } => {
                    let (target, modifiers, moving) = match action {
                        Action::Click { index, modifiers } => (index, modifiers, false),
                        Action::Move {
                            movement,
                            modifiers,
                        } => {
                            let target = match movement {
                                Movement::To(index) => index,
                                _ if len == 0 => return Ok(Change::default()),
                                Movement::First => 0,
                                Movement::Last => len - 1,
                                Movement::Offset(delta) => match self.cursor {
                                    None => 0,
                                    Some(index) if delta < 0 => {
                                        index.saturating_sub(delta.unsigned_abs())
                                    }
                                    Some(index) => {
                                        index.saturating_add(delta as usize).min(len - 1)
                                    }
                                },
                            };
                            (target, modifiers, true)
                        }
                        _ => unreachable!(),
                    };
                    if target >= len {
                        return Err(SelectionError::OutOfBounds { index: target, len });
                    }
                    if modifiers.shift {
                        let anchor = self.anchor.or(self.cursor).unwrap_or(target);
                        self.anchor = Some(anchor);
                        if self.extension.as_ref().map(|(additive, _)| *additive)
                            != Some(modifiers.control)
                        {
                            self.extension = Some((modifiers.control, self.selected.clone()));
                        }
                        for (index, selected) in self.selected.iter_mut().enumerate() {
                            let inside = index >= anchor.min(target) && index <= anchor.max(target);
                            *selected = inside
                                || (modifiers.control && self.extension.as_ref().unwrap().1[index]);
                        }
                    } else {
                        self.extension = None;
                        self.anchor = Some(target);
                        if !modifiers.control {
                            self.selected.fill(false);
                            self.selected[target] = true;
                        } else if !moving {
                            self.selected[target] ^= true;
                        }
                    }
                    self.cursor = Some(target);
                }
            }
            Ok(Change {
                selection_changed: self.selected != before,
                cursor_changed: self.cursor != old_cursor,
            })
        }
    }

    #[test]
    fn randomized_state_machine_matches_boolean_oracle_and_normalized_ranges() {
        let mut random = 0xd313_4ac5_88f1_7219_u64;
        let mut next = || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        for len in 0..32 {
            let rev = ListingRevision::new();
            let mut selection = Selection::new(rev.clone(), len);
            let mut oracle = Oracle::new(len);
            for step in 0..4000 {
                let modifiers = [PLAIN, CTRL, SHIFT, BOTH][(next() % 4) as usize];
                let index = (next() % (len + 3) as u64) as usize;
                let movement = match next() % 6 {
                    0 => Movement::First,
                    1 => Movement::Last,
                    2 => Movement::To(index),
                    3 => Movement::Offset(isize::MIN),
                    4 => Movement::Offset(isize::MAX),
                    _ => Movement::Offset((next() % 17) as isize - 8),
                };
                let action = match next() % 7 {
                    0 => Action::SelectAll,
                    1 => Action::Clear,
                    2 => Action::ToggleCursor,
                    3 | 4 => click(index, modifiers),
                    _ => Action::Move {
                        movement,
                        modifiers,
                    },
                };
                assert_eq!(
                    selection.apply(&rev, action),
                    oracle.apply(action),
                    "len={len} step={step} {action:?}"
                );
                assert_eq!(selection.cursor, oracle.cursor);
                assert_eq!(selection.anchor, oracle.anchor);
                assert_eq!(
                    selection.count(),
                    oracle.selected.iter().filter(|selected| **selected).count()
                );
                for index in 0..len {
                    assert_eq!(selection.contains(&rev, index), Ok(oracle.selected[index]));
                }
                let runs = selection.ranges(&rev).unwrap();
                assert!(runs
                    .iter()
                    .all(|range| range.start < range.end && range.end <= len));
                assert!(runs.windows(2).all(|pair| pair[0].end < pair[1].start));
                assert_eq!(
                    runs.iter()
                        .map(|range| range.end - range.start)
                        .sum::<usize>(),
                    selection.count()
                );
            }
        }
    }
}
