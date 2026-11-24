//! How a window's panes split its grid, as a tree of splits.

/// Identifies a pane for as long as xaya runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u64);

impl PaneId {
    /// A pane id not used before.
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Where a new pane goes, beside the one it splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Side by side, the new pane on the right.
    Right,
    /// Stacked, the new pane below.
    Down,
}

/// A rectangle of grid cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub column: usize,
    pub line: usize,
    pub columns: usize,
    pub lines: usize,
}

/// The cells separating two panes, a column or a line wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divider {
    pub direction: SplitDirection,
    pub rect: CellRect,
}

/// The smallest pane, in cells; splits that would make a smaller one are refused.
const MIN_COLUMNS: usize = 10;
const MIN_LINES: usize = 3;

/// A pane, or two parts split in a direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Pane(PaneId),
    Split { direction: SplitDirection, first: Box<Layout>, second: Box<Layout> },
}

impl Layout {
    /// The panes in reading order: left before right, top before bottom.
    pub fn panes(&self) -> Vec<PaneId> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut panes = first.panes();
                panes.extend(second.panes());
                panes
            },
        }
    }

    /// Split `target` in `direction`, putting `new` beside it, if `area` leaves both panes room.
    pub fn split(
        &mut self,
        area: CellRect,
        target: PaneId,
        new: PaneId,
        direction: SplitDirection,
    ) -> bool {
        let Some(rect) = self.rects(area).into_iter().find(|(id, _)| *id == target).map(|(_, r)| r)
        else {
            return false;
        };
        let fits = match direction {
            SplitDirection::Right => rect.columns > 2 * MIN_COLUMNS,
            SplitDirection::Down => rect.lines > 2 * MIN_LINES,
        };
        fits && self.replace(target, &mut |pane| Self::Split {
            direction,
            first: Box::new(pane),
            second: Box::new(Self::Pane(new)),
        })
    }

    /// Remove `target`, its sibling taking its place; the last pane can't be removed.
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                if **first == Self::Pane(target) {
                    *self = (**second).clone();
                    true
                } else if **second == Self::Pane(target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove(target) || second.remove(target)
                }
            },
        }
    }

    /// Where each pane goes in `area`.
    pub fn rects(&self, area: CellRect) -> Vec<(PaneId, CellRect)> {
        let mut rects = Vec::new();
        self.walk(area, &mut |layout, rect| {
            if let Self::Pane(id) = layout {
                rects.push((*id, rect));
            }
        });
        rects
    }

    /// The dividers between panes in `area`.
    pub fn dividers(&self, area: CellRect) -> Vec<Divider> {
        let mut dividers = Vec::new();
        self.walk(area, &mut |layout, rect| {
            if let Self::Split { direction, .. } = layout {
                let (_, divider, _) = divide(rect, *direction);
                dividers.push(Divider { direction: *direction, rect: divider });
            }
        });
        dividers
    }

    fn walk(&self, area: CellRect, visit: &mut impl FnMut(&Self, CellRect)) {
        visit(self, area);
        if let Self::Split { direction, first, second } = self {
            let (first_rect, _, second_rect) = divide(area, *direction);
            first.walk(first_rect, visit);
            second.walk(second_rect, visit);
        }
    }

    /// Replace the pane `target` with what `with` makes of it.
    fn replace(&mut self, target: PaneId, with: &mut impl FnMut(Self) -> Self) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                let pane = std::mem::replace(self, Self::Pane(target));
                *self = with(pane);
                true
            },
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.replace(target, with) || second.replace(target, with)
            },
        }
    }
}

/// `area` split in half in `direction`: the first part, the divider and the second part.
///
/// The divider takes a column or line; an odd remainder goes to the second part.
fn divide(area: CellRect, direction: SplitDirection) -> (CellRect, CellRect, CellRect) {
    match direction {
        SplitDirection::Right => {
            let first = area.columns.saturating_sub(1) / 2;
            let second = area.columns.saturating_sub(first + 1);
            (
                CellRect { columns: first, ..area },
                CellRect { column: area.column + first, columns: 1.min(area.columns), ..area },
                CellRect { column: area.column + first + 1, columns: second, ..area },
            )
        },
        SplitDirection::Down => {
            let first = area.lines.saturating_sub(1) / 2;
            let second = area.lines.saturating_sub(first + 1);
            (
                CellRect { lines: first, ..area },
                CellRect { line: area.line + first, lines: 1.min(area.lines), ..area },
                CellRect { line: area.line + first + 1, lines: second, ..area },
            )
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: CellRect = CellRect { column: 0, line: 0, columns: 81, lines: 24 };

    #[test]
    fn splitting_halves_the_target() {
        let (a, b, c) = (PaneId(1), PaneId(2), PaneId(3));
        let mut layout = Layout::Pane(a);
        assert_eq!(layout.rects(AREA), [(a, AREA)]);
        assert!(layout.dividers(AREA).is_empty());

        assert!(layout.split(AREA, a, b, SplitDirection::Right));
        assert_eq!(layout.rects(AREA), [
            (a, CellRect { column: 0, line: 0, columns: 40, lines: 24 }),
            (b, CellRect { column: 41, line: 0, columns: 40, lines: 24 }),
        ]);
        assert_eq!(layout.dividers(AREA), [Divider {
            direction: SplitDirection::Right,
            rect: CellRect { column: 40, line: 0, columns: 1, lines: 24 },
        }]);

        // Splitting the right pane down leaves the left one alone.
        assert!(layout.split(AREA, b, c, SplitDirection::Down));
        assert_eq!(layout.panes(), [a, b, c]);
        let rects = layout.rects(AREA);
        assert_eq!(rects[0].1.columns, 40);
        assert_eq!(rects[1].1, CellRect { column: 41, line: 0, columns: 40, lines: 11 });
        assert_eq!(rects[2].1, CellRect { column: 41, line: 12, columns: 40, lines: 12 });
        assert_eq!(layout.dividers(AREA).len(), 2);
    }

    #[test]
    fn panes_too_small_to_split_are_refused() {
        let (a, b) = (PaneId(1), PaneId(2));
        let mut layout = Layout::Pane(a);
        let narrow = CellRect { columns: 20, lines: 6, ..AREA };
        assert!(!layout.split(narrow, a, b, SplitDirection::Right));
        assert!(!layout.split(narrow, a, b, SplitDirection::Down));
        assert!(!layout.split(AREA, PaneId(9), b, SplitDirection::Right));
        assert_eq!(layout, Layout::Pane(a));
    }

    #[test]
    fn removing_gives_the_space_to_the_sibling() {
        let (a, b, c) = (PaneId(1), PaneId(2), PaneId(3));
        let mut layout = Layout::Pane(a);
        layout.split(AREA, a, b, SplitDirection::Right);
        layout.split(AREA, b, c, SplitDirection::Down);

        assert!(layout.remove(b));
        assert_eq!(layout.panes(), [a, c]);
        assert_eq!(layout.rects(AREA)[1].1, CellRect {
            column: 41,
            line: 0,
            columns: 40,
            lines: 24
        });

        assert!(layout.remove(a));
        assert_eq!(layout, Layout::Pane(c));
        assert!(!layout.remove(c));
        assert!(!layout.remove(PaneId(9)));
    }

    #[test]
    fn pane_ids_are_unique() {
        assert_ne!(PaneId::next(), PaneId::next());
    }
}
