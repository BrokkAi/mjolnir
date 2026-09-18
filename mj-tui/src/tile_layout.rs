//! Binary-space-partition layout tree for tiling conversation panes.
//!
//! A [`TileLayout`] is a binary tree: each leaf is a pane holding one
//! conversation, and each inner node is a split that divides its rectangle
//! into a first and a second half along a ratio. [`TileLayout::panes`] and
//! [`TileLayout::splits`] turn the tree into rectangles for a given area;
//! [`TileLayout::can_split`] keeps a split from producing a pane smaller
//! than a conversation can usefully show.

use std::cmp::Reverse;

use ratatui::layout::{Direction, Rect};

/// Narrowest a pane may be after a split, in columns.
pub const MIN_PANE_WIDTH: u16 = 40;

/// Shortest a pane may be after a split, in rows.
pub const MIN_PANE_HEIGHT: u16 = 8;

/// Identifies one pane within a single [`TileLayout`]. Ids are allocated by
/// that layout and are not reused while the pane exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(u32);

impl PaneId {
    /// The id's raw value, for saving a layout.
    pub fn raw(self) -> u32 {
        self.0
    }

    /// Rebuild a `PaneId` from a value a layout previously saved.
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// One node of the binary-space-partition tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A leaf holding one pane.
    Pane(PaneId),
    /// An inner node dividing `first` and `second` along `direction`.
    Split {
        direction: Direction,
        /// Share of the split's area given to `first`, kept in `0.1..=0.9`.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// A pane's rectangle and focus state, as computed for one area.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub is_focused: bool,
}

/// One split boundary within a laid-out area, for resize.
#[derive(Debug, Clone)]
pub struct SplitBorder {
    /// Column (horizontal split) or row (vertical split) of the divider.
    pub pos: u16,
    pub direction: Direction,
    /// Share of `area` given to the split's first child.
    pub ratio: f32,
    /// The split node's own area, before dividing it.
    pub area: Rect,
    /// Steps from the root to this split: `false` into `first`, `true` into
    /// `second`.
    pub path: Vec<bool>,
}

/// A direction to move focus or grow a pane toward.
#[derive(Debug, Clone, Copy)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A binary-space-partition tree of panes, with a focused leaf.
pub struct TileLayout {
    root: Node,
    focus: PaneId,
    /// The pane focused before `focus`, so `close_focused` can return to it.
    /// Only a real focus change sets this; tree edits that take a target
    /// pane directly (`split_pane`, `close_pane`) leave it alone.
    prev_focus: Option<PaneId>,
    /// The next id `split_pane` will hand out.
    next_id: u32,
}

impl TileLayout {
    /// Start a layout with a single pane, id 1. Returns the layout and that
    /// pane's id so the caller can set up what it shows.
    pub fn new() -> (Self, PaneId) {
        let root_id = PaneId(1);
        (
            Self {
                root: Node::Pane(root_id),
                focus: root_id,
                prev_focus: None,
                next_id: 2,
            },
            root_id,
        )
    }

    fn allocate_id(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Move focus, recording the pane being left. Does nothing when `id` is
    /// already focused.
    fn set_focus(&mut self, id: PaneId) {
        if id != self.focus {
            self.prev_focus = Some(self.focus);
            self.focus = id;
        }
    }

    pub fn focused(&self) -> PaneId {
        self.focus
    }

    pub fn pane_count(&self) -> usize {
        count_panes(&self.root)
    }

    /// Compute every pane's rectangle within `area`.
    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut result = Vec::new();
        collect_panes(&self.root, area, self.focus, &mut result);
        result
    }

    /// Compute every split boundary within `area`, for drag or key resize.
    pub fn splits(&self, area: Rect) -> Vec<SplitBorder> {
        let mut result = Vec::new();
        collect_splits(&self.root, area, Vec::new(), &mut result);
        result
    }

    /// Whether splitting `target` along `direction` would leave both halves
    /// at least [`MIN_PANE_WIDTH`] columns wide and [`MIN_PANE_HEIGHT`] rows
    /// tall. False when `target` is not in the layout.
    pub fn can_split(&self, target: PaneId, direction: Direction, area: Rect) -> bool {
        let Some(target_rect) = self
            .panes(area)
            .into_iter()
            .find_map(|pane| (pane.id == target).then_some(pane.rect))
        else {
            return false;
        };
        let (first, second) = split_rect(target_rect, direction, 0.5);
        [first, second]
            .into_iter()
            .all(|half| half.width >= MIN_PANE_WIDTH && half.height >= MIN_PANE_HEIGHT)
    }

    /// Split `target` without moving focus. Returns the new pane's id, or
    /// `None` when `target` is not in the layout or [`Self::can_split`]
    /// refuses the split.
    pub fn split_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
        ratio: f32,
        area: Rect,
    ) -> Option<PaneId> {
        if !self.can_split(target, direction, area) {
            return None;
        }
        let new_id = self.allocate_id();
        let placeholder = PaneId(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, target, direction, new_id, valid_split_ratio(ratio));
        Some(new_id)
    }

    /// Split the focused pane with an even ratio. Test-only shorthand for
    /// `split_pane` followed by moving focus to the new pane.
    #[cfg(test)]
    pub fn split_focused(&mut self, direction: Direction, area: Rect) -> PaneId {
        self.split_focused_with_ratio(direction, 0.5, area)
    }

    /// Split the focused pane with a chosen first-child ratio, then focus
    /// the new pane.
    #[cfg(test)]
    pub fn split_focused_with_ratio(
        &mut self,
        direction: Direction,
        ratio: f32,
        area: Rect,
    ) -> PaneId {
        let new_id = self
            .split_pane(self.focus, direction, ratio, area)
            .expect("test area accepts the split");
        self.set_focus(new_id);
        new_id
    }

    /// Close the focused pane, moving focus to the pane it was opened from
    /// when that pane still exists, or to a tree-order neighbor otherwise.
    /// Returns false when it is the last pane.
    pub fn close_focused(&mut self) -> bool {
        if self.pane_count() <= 1 {
            return false;
        }
        let target = self.focus;
        let ids = self.pane_ids();
        let position = ids
            .iter()
            .position(|id| *id == target)
            .expect("focused pane is in the layout");
        let neighbor = if position + 1 < ids.len() {
            ids[position + 1]
        } else {
            ids[position - 1]
        };
        let new_focus = match self.prev_focus {
            Some(prev) if prev != target && ids.contains(&prev) => prev,
            _ => neighbor,
        };
        let placeholder = PaneId(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        let Some(new_root) = remove_pane(old, target) else {
            return false;
        };
        self.root = new_root;
        self.focus = new_focus;
        self.prev_focus = None;
        true
    }

    /// Close any pane. Focus and its history are untouched unless `id` was
    /// the focused pane, in which case this behaves like `close_focused`.
    pub fn close_pane(&mut self, id: PaneId) -> bool {
        if self.focus == id {
            return self.close_focused();
        }
        if self.pane_count() <= 1 || !self.pane_ids().contains(&id) {
            return false;
        }
        let placeholder = PaneId(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        let Some(new_root) = remove_pane(old, id) else {
            return false;
        };
        self.root = new_root;
        if self.prev_focus == Some(id) {
            self.prev_focus = None;
        }
        true
    }

    pub fn focus_pane(&mut self, id: PaneId) {
        if self.pane_ids().contains(&id) {
            self.set_focus(id);
        }
    }

    /// Set the ratio of the split at `path`, clamped to `0.1..=0.9`.
    pub fn set_ratio_at(&mut self, path: &[bool], ratio: f32) -> bool {
        set_ratio_at(&mut self.root, path, ratio.clamp(0.1, 0.9))
    }

    /// Grow or shrink the focused pane toward `nav` by `delta`, adjusting
    /// the nearest matching split.
    pub fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        let panes = self.panes(area);
        let Some(focused) = panes.iter().find(|pane| pane.is_focused) else {
            return;
        };
        let focused_rect = focused.rect;
        let splits = self.splits(area);

        let target_direction = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);

        let chosen =
            nearest_resize_split(&splits, target_direction, focused_rect, nav).or_else(|| {
                nearest_resize_split(
                    &splits,
                    target_direction,
                    focused_rect,
                    opposite_direction(nav),
                )
            });

        if let Some(split) = chosen {
            let path = split.path.clone();
            let current_ratio = get_ratio_at(&self.root, &path).unwrap_or(0.5);
            let adjustment = if grows { delta } else { -delta };
            self.set_ratio_at(&path, current_ratio + adjustment);
        }
    }

    /// Resize `pane_id` as if it were focused, without disturbing the real
    /// focus. Returns whether any ratio changed.
    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        nav: NavDirection,
        delta: f32,
        area: Rect,
    ) -> bool {
        if !self.pane_ids().contains(&pane_id) {
            return false;
        }
        let before = split_ratios(&self.root);
        let previous_focus = self.focus;
        self.focus = pane_id;
        self.resize_focused(nav, delta, area);
        self.focus = previous_focus;
        split_ratios(&self.root) != before
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        collect_ids(&self.root, &mut ids);
        ids
    }

    /// The tree's root, for saving the layout.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Rebuild a layout from a saved tree. Allocates new ids starting one
    /// above the highest id already present, so a later split never
    /// collides with a restored one.
    pub fn from_saved(root: Node, focus: PaneId) -> Self {
        let mut ids = Vec::new();
        collect_ids(&root, &mut ids);
        let next_id = ids.iter().map(|id| id.0).max().unwrap_or(0) + 1;
        Self {
            root,
            focus,
            prev_focus: None,
            next_id,
        }
    }
}

// --- Directional pane navigation ---

/// Find the nearest pane in `direction` from `focused`, preferring the
/// closest edge, then the largest overlap along the shared axis, then
/// layout order.
pub fn find_in_direction(
    focused: &PaneInfo,
    direction: NavDirection,
    panes: &[PaneInfo],
) -> Option<PaneId> {
    let from = focused.rect;

    panes
        .iter()
        .enumerate()
        .filter(|(_, pane)| pane.id != focused.id)
        .filter(|(_, pane)| {
            let r = pane.rect;
            match direction {
                NavDirection::Left => {
                    r.x + r.width <= from.x && ranges_overlap(r.y, r.height, from.y, from.height)
                }
                NavDirection::Right => {
                    r.x >= from.x + from.width && ranges_overlap(r.y, r.height, from.y, from.height)
                }
                NavDirection::Up => {
                    r.y + r.height <= from.y && ranges_overlap(r.x, r.width, from.x, from.width)
                }
                NavDirection::Down => {
                    r.y >= from.y + from.height && ranges_overlap(r.x, r.width, from.x, from.width)
                }
            }
        })
        .min_by_key(|(index, pane)| {
            let r = pane.rect;
            let edge_distance = match direction {
                NavDirection::Left => from.x.saturating_sub(r.x + r.width),
                NavDirection::Right => r.x.saturating_sub(from.x + from.width),
                NavDirection::Up => from.y.saturating_sub(r.y + r.height),
                NavDirection::Down => r.y.saturating_sub(from.y + from.height),
            };
            let overlap = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_overlap_amount(r.y, r.height, from.y, from.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_overlap_amount(r.x, r.width, from.x, from.width)
                }
            };
            let center_distance = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_center_distance(r.y, r.height, from.y, from.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_center_distance(r.x, r.width, from.x, from.width)
                }
            };
            (edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, pane)| pane.id)
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start + b_len && a_start + a_len > b_start
}

fn split_on_requested_edge(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    split_edge_distance(split, focused, nav) <= 1
}

fn split_area_overlaps_focused_pane(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    match nav {
        NavDirection::Left | NavDirection::Right => {
            ranges_overlap(split.area.y, split.area.height, focused.y, focused.height)
        }
        NavDirection::Up | NavDirection::Down => {
            ranges_overlap(split.area.x, split.area.width, focused.x, focused.width)
        }
    }
}

/// The split closest to the edge of `focused` that `nav` points at, among
/// splits running the right way and overlapping the focused pane's other
/// axis.
fn nearest_resize_split(
    splits: &[SplitBorder],
    target_direction: Direction,
    focused: Rect,
    nav: NavDirection,
) -> Option<&SplitBorder> {
    splits
        .iter()
        .filter(|split| split.direction == target_direction)
        .filter(|split| split_area_overlaps_focused_pane(split, focused, nav))
        .filter(|split| split_on_requested_edge(split, focused, nav))
        .min_by_key(|split| split_edge_distance(split, focused, nav))
}

fn opposite_direction(nav: NavDirection) -> NavDirection {
    match nav {
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
    }
}

fn split_edge_distance(split: &SplitBorder, focused: Rect, nav: NavDirection) -> u32 {
    match nav {
        NavDirection::Left => (split.pos as i32 - focused.x as i32).unsigned_abs(),
        NavDirection::Right => {
            (split.pos as i32 - (focused.x + focused.width) as i32).unsigned_abs()
        }
        NavDirection::Up => (split.pos as i32 - focused.y as i32).unsigned_abs(),
        NavDirection::Down => {
            (split.pos as i32 - (focused.y + focused.height) as i32).unsigned_abs()
        }
    }
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_center = a_start.saturating_mul(2).saturating_add(a_len);
    let b_center = b_start.saturating_mul(2).saturating_add(b_len);
    a_center.abs_diff(b_center)
}

// --- Tree operations ---

fn count_panes(node: &Node) -> usize {
    match node {
        Node::Pane(_) => 1,
        Node::Split { first, second, .. } => count_panes(first) + count_panes(second),
    }
}

fn collect_panes(node: &Node, area: Rect, focus: PaneId, result: &mut Vec<PaneInfo>) {
    match node {
        Node::Pane(id) => result.push(PaneInfo {
            id: *id,
            rect: area,
            is_focused: *id == focus,
        }),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *direction, *ratio);
            collect_panes(first, a, focus, result);
            collect_panes(second, b, focus, result);
        }
    }
}

fn collect_splits(node: &Node, area: Rect, path: Vec<bool>, result: &mut Vec<SplitBorder>) {
    if let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *direction, *ratio);
        let pos = match direction {
            Direction::Horizontal => a.x + a.width,
            Direction::Vertical => a.y + a.height,
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: *ratio,
            area,
            path: path.clone(),
        });
        let mut first_path = path.clone();
        first_path.push(false);
        collect_splits(first, a, first_path, result);
        let mut second_path = path;
        second_path.push(true);
        collect_splits(second, b, second_path, result);
    }
}

fn collect_ids(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids(first, ids);
            collect_ids(second, ids);
        }
    }
}

/// Every split's path and ratio, for detecting whether a resize changed
/// anything.
fn split_ratios(node: &Node) -> Vec<(Vec<bool>, f32)> {
    fn walk(node: &Node, path: &mut Vec<bool>, out: &mut Vec<(Vec<bool>, f32)>) {
        if let Node::Split {
            ratio,
            first,
            second,
            ..
        } = node
        {
            out.push((path.clone(), *ratio));
            path.push(false);
            walk(first, path, out);
            path.pop();
            path.push(true);
            walk(second, path, out);
            path.pop();
        }
    }

    let mut out = Vec::new();
    walk(node, &mut Vec::new(), &mut out);
    out
}

fn split_at(node: Node, target: PaneId, direction: Direction, new_id: PaneId, ratio: f32) -> Node {
    match node {
        Node::Pane(id) if id == target => Node::Split {
            direction,
            ratio,
            first: Box::new(Node::Pane(id)),
            second: Box::new(Node::Pane(new_id)),
        },
        Node::Pane(_) => node,
        Node::Split {
            direction: d,
            ratio: r,
            first,
            second,
        } => Node::Split {
            direction: d,
            ratio: r,
            first: Box::new(split_at(*first, target, direction, new_id, ratio)),
            second: Box::new(split_at(*second, target, direction, new_id, ratio)),
        },
    }
}

fn valid_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    }
}

fn remove_pane(node: Node, target: PaneId) -> Option<Node> {
    match node {
        Node::Pane(id) if id == target => None,
        Node::Pane(_) => Some(node),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => match (remove_pane(*first, target), remove_pane(*second, target)) {
            (None, Some(kept)) => Some(kept),
            (Some(kept), None) => Some(kept),
            (Some(first), Some(second)) => Some(Node::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (None, None) => None,
        },
    }
}

fn set_ratio_at(node: &mut Node, path: &[bool], new_ratio: f32) -> bool {
    let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    match path.split_first() {
        None => {
            *ratio = new_ratio;
            true
        }
        Some((false, rest)) => set_ratio_at(first, rest, new_ratio),
        Some((true, rest)) => set_ratio_at(second, rest, new_ratio),
    }
}

fn get_ratio_at(node: &Node, path: &[bool]) -> Option<f32> {
    let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    else {
        return None;
    };
    match path.split_first() {
        None => Some(*ratio),
        Some((false, rest)) => get_ratio_at(first, rest),
        Some((true, rest)) => get_ratio_at(second, rest),
    }
}

/// Divide `area` into two rects along `direction`, giving `ratio` of it to
/// the first.
pub fn split_rect(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
    match direction {
        Direction::Horizontal => {
            let first_width = ((area.width as f32) * ratio).round() as u16;
            let second_width = area.width.saturating_sub(first_width);
            (
                Rect::new(area.x, area.y, first_width, area.height),
                Rect::new(area.x + first_width, area.y, second_width, area.height),
            )
        }
        Direction::Vertical => {
            let first_height = ((area.height as f32) * ratio).round() as u16;
            let second_height = area.height.saturating_sub(first_height);
            (
                Rect::new(area.x, area.y, area.width, first_height),
                Rect::new(area.x, area.y + first_height, area.width, second_height),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Area big enough that a split never trips the minimum pane size, even
    /// after several nested splits shrink an individual leaf.
    fn area() -> Rect {
        Rect::new(0, 0, 400, 100)
    }

    fn pane(id: u32) -> PaneId {
        PaneId::from_raw(id)
    }

    fn sample_layout() -> TileLayout {
        TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.3,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.6,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.4,
                        first: Box::new(Node::Pane(pane(3))),
                        second: Box::new(Node::Pane(pane(4))),
                    }),
                }),
            },
            pane(2),
        )
    }

    fn pane_rect(layout: &TileLayout, pane_id: PaneId) -> Rect {
        layout
            .panes(area())
            .into_iter()
            .find_map(|info| (info.id == pane_id).then_some(info.rect))
            .expect("pane should exist")
    }

    fn split_snapshot(layout: &TileLayout) -> Vec<(Direction, f32)> {
        fn walk(node: &Node, out: &mut Vec<(Direction, f32)>) {
            if let Node::Split {
                direction,
                ratio,
                first,
                second,
            } = node
            {
                out.push((*direction, *ratio));
                walk(first, out);
                walk(second, out);
            }
        }

        let mut out = Vec::new();
        walk(layout.root(), &mut out);
        out
    }

    #[test]
    fn split_focused_with_ratio_sets_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();
        layout.focus_pane(root);

        layout.split_focused_with_ratio(Direction::Horizontal, 0.333, area());

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_preserves_focus_and_reports_change() {
        let mut layout = sample_layout();
        let original_focus = layout.focused();

        assert!(layout.resize_pane(pane(1), NavDirection::Right, 0.05, area()));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_focused(Direction::Horizontal, area());
        layout.focus_pane(root);

        assert!(layout.resize_pane(right, NavDirection::Left, 0.05, area()));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        horizontal.split_focused(Direction::Horizontal, area());

        assert!(horizontal.resize_pane(left, NavDirection::Left, 0.05, area()));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut horizontal, _left) = TileLayout::new();
        let right = horizontal.split_focused(Direction::Horizontal, area());

        assert!(horizontal.resize_pane(right, NavDirection::Right, 0.05, area()));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        vertical.split_focused(Direction::Vertical, area());

        assert!(vertical.resize_pane(top, NavDirection::Up, 0.05, area()));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut vertical, _top) = TileLayout::new();
        let bottom = vertical.split_focused(Direction::Vertical, area());

        assert!(vertical.resize_pane(bottom, NavDirection::Down, 0.05, area()));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_outer_edge_falls_back_to_horizontal_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Left, 0.05, area()));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Vertical, 0.5));
    }

    #[test]
    fn resize_outer_edge_falls_back_to_vertical_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Up, 0.05, area()));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
    }

    #[test]
    fn resize_uses_split_in_same_branch_when_borders_share_coordinate() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(3),
        );

        assert!(layout.resize_pane(pane(3), NavDirection::Right, 0.05, area()));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_does_not_disturb_the_close_focus_target() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));
        layout.resize_pane(pane(1), NavDirection::Right, 0.05, area());

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn find_in_direction_tiebreaks_by_larger_overlap_before_layout_order() {
        let focused = PaneInfo {
            id: pane(1),
            rect: Rect::new(10, 10, 10, 10),
            is_focused: true,
        };
        let small_overlap_first = PaneInfo {
            id: pane(2),
            rect: Rect::new(0, 10, 10, 2),
            is_focused: false,
        };
        let larger_overlap_second = PaneInfo {
            id: pane(3),
            rect: Rect::new(0, 10, 10, 8),
            is_focused: false,
        };
        let panes = vec![focused.clone(), small_overlap_first, larger_overlap_second];

        assert_eq!(
            find_in_direction(&focused, NavDirection::Left, &panes),
            Some(pane(3))
        );
    }

    #[test]
    fn close_focused_returns_to_the_pane_focus_came_from() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn close_focused_returns_to_the_pane_that_opened_a_split() {
        let (mut layout, first) = TileLayout::new();
        let second = layout.split_focused(Direction::Horizontal, area());
        let third = layout.split_focused(Direction::Vertical, area());
        assert_eq!(layout.pane_ids().len(), 3);

        layout.focus_pane(first);
        let opened = layout.split_focused(Direction::Horizontal, area());
        assert_eq!(layout.focused(), opened);

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), first);
        assert!(layout.pane_ids().contains(&second));
        assert!(layout.pane_ids().contains(&third));
    }

    #[test]
    fn closing_a_background_pane_keeps_the_focused_pane_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(1)));
        assert_eq!(layout.focused(), pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn closing_the_remembered_pane_drops_the_focus_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(2)));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_uses_tree_order_without_focus_history() {
        let mut layout = sample_layout();

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_does_not_reuse_history_after_it_is_consumed() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn split_pane_leaves_focus_and_history_untouched() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        let new_id = layout
            .split_pane(pane(1), Direction::Horizontal, 0.5, area())
            .expect("target exists");

        assert!(layout.pane_ids().contains(&new_id));
        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_missing_target_changes_nothing() {
        let mut layout = sample_layout();
        let ids = layout.pane_ids();

        assert_eq!(
            layout.split_pane(pane(99), Direction::Horizontal, 0.5, area()),
            None
        );

        assert_eq!(layout.pane_ids(), ids);
    }

    #[test]
    fn split_is_refused_when_a_leaf_would_fall_under_the_minimum() {
        let (mut layout, root) = TileLayout::new();

        // 70 columns split in half gives two 35-column halves, under
        // MIN_PANE_WIDTH.
        let narrow_area = Rect::new(0, 0, 70, 20);
        assert!(!layout.can_split(root, Direction::Horizontal, narrow_area));
        assert_eq!(
            layout.split_pane(root, Direction::Horizontal, 0.5, narrow_area),
            None
        );
        assert_eq!(layout.pane_count(), 1);

        // 16 rows split in half gives two 8-row halves... but 15 rows gives
        // two halves under MIN_PANE_HEIGHT.
        let short_area = Rect::new(0, 0, 100, 15);
        assert!(!layout.can_split(root, Direction::Vertical, short_area));
        assert_eq!(
            layout.split_pane(root, Direction::Vertical, 0.5, short_area),
            None
        );
        assert_eq!(layout.pane_count(), 1);

        // A generous area still allows the split.
        assert!(layout.can_split(root, Direction::Horizontal, area()));
        assert!(
            layout
                .split_pane(root, Direction::Horizontal, 0.5, area())
                .is_some()
        );
    }

    #[test]
    fn restored_layouts_allocate_ids_above_the_saved_ones() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Pane(pane(5))),
            },
            pane(5),
        );

        let new_id = layout
            .split_pane(pane(5), Direction::Vertical, 0.5, area())
            .expect("target pane accepts the split");

        assert_eq!(new_id, pane(6));
    }
}
