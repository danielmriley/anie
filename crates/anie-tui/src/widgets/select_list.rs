//! Reusable scrollable-filtered list primitive.
//!
//! Used by the inline slash-command autocomplete popup (plan 12)
//! and designed so the existing `ModelPickerPane` and future
//! session/extension pickers can migrate onto it. This widget
//! owns **only** state: selection index, scroll offset, filtered
//! view. Rendering stays with the caller so each overlay keeps
//! its own visual style.
//!
//! Pi's equivalent is `packages/tui/src/components/select-list.ts`
//! (see docs/arch/pi_summary.md). The anie port keeps the same
//! mental model — items + predicate filter + wrap navigation —
//! but stays synchronous and generic over the item type.

/// Scrollable list of `T` with a filterable view.
///
/// State layout:
/// - `items` — the immutable backing store set by `new`/`set_items`.
/// - `filtered` — indices into `items` currently visible.
/// - `selected` — index into `filtered`.
/// - `scroll` — first visible row (in `filtered` space).
/// - `max_visible` — viewport height cap.
///
/// Selection is empty (`None` from `selected()`) iff the filtered
/// view is empty; when items are present, `selected` always
/// points at a valid row so callers don't have to nil-check on
/// every keypress.
#[derive(Debug, Clone)]
pub(crate) struct SelectList<T> {
    items: Vec<T>,
    filtered: Vec<usize>,
    selected: usize,
    scroll: usize,
    max_visible: usize,
}

impl<T> SelectList<T> {
    /// Create a list with every item initially visible.
    pub(crate) fn new(items: Vec<T>, max_visible: usize) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            scroll: 0,
            max_visible: max_visible.max(1),
        }
    }

    /// Replace the backing items and reset filter + selection.
    #[allow(dead_code)]
    pub(crate) fn set_items(&mut self, items: Vec<T>) {
        self.filtered = (0..items.len()).collect();
        self.items = items;
        self.selected = 0;
        self.scroll = 0;
    }

    /// Apply a predicate to narrow the visible set.
    ///
    /// Called whenever the filter text changes. Resets the
    /// selection to the first match so repeated filter changes
    /// don't leave the highlight stuck on a now-invisible row.
    ///
    /// The autocomplete popup currently rebuilds its
    /// `SelectList` each time the provider yields a new set —
    /// this method is the in-place alternative used by the
    /// model picker migration (tracked as a follow-up in plan
    /// 12's "Out of scope").
    #[allow(dead_code)]
    pub(crate) fn apply_filter<F: Fn(&T) -> bool>(&mut self, predicate: F) {
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| predicate(item))
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Move selection by `delta` rows, wrapping at both ends.
    ///
    /// `delta = -1` is previous, `delta = 1` is next. Larger
    /// deltas move proportionally and still wrap, which mirrors
    /// pi's page-up / page-down behavior without needing extra
    /// methods.
    pub(crate) fn move_selection(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let len_i = len as isize;
        let current = self.selected as isize;
        let mut next = (current + delta) % len_i;
        if next < 0 {
            next += len_i;
        }
        self.selected = next as usize;
        self.scroll_to_selected();
    }

    /// Move the selection to the first item for which the
    /// predicate returns true. No-op if no item matches.
    ///
    /// Used by the autocomplete popup to seed the highlight onto
    /// the best prefix match when the popup first opens.
    pub(crate) fn select_first_where<F: Fn(&T) -> bool>(&mut self, predicate: F) {
        for (filtered_index, &original_index) in self.filtered.iter().enumerate() {
            if predicate(&self.items[original_index]) {
                self.selected = filtered_index;
                self.scroll_to_selected();
                return;
            }
        }
    }

    /// Currently-selected item, or `None` when the filtered view is
    /// empty.
    pub(crate) fn selected(&self) -> Option<&T> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    /// Number of rows currently visible after filtering.
    #[allow(dead_code)]
    pub(crate) fn visible_len(&self) -> usize {
        self.filtered.len()
    }

    /// Maximum viewport height.
    #[allow(dead_code)]
    pub(crate) fn max_visible(&self) -> usize {
        self.max_visible
    }

    /// Set a new viewport height; re-clamps scroll so the
    /// selection stays visible.
    #[allow(dead_code)]
    pub(crate) fn set_max_visible(&mut self, max_visible: usize) {
        self.max_visible = max_visible.max(1);
        self.scroll_to_selected();
    }

    /// Actual row count to render: `min(max_visible,
    /// visible_len())`.
    pub(crate) fn height_hint(&self) -> u16 {
        u16::try_from(self.filtered.len().min(self.max_visible)).unwrap_or(u16::MAX)
    }

    /// Iterate over currently-visible rows (at most `max_visible`).
    /// Yields `(original_index, item, is_selected)`.
    pub(crate) fn visible(&self) -> impl Iterator<Item = (usize, &T, bool)> {
        let start = self.scroll;
        let end = (self.scroll + self.max_visible).min(self.filtered.len());
        let selected = self.selected;
        self.filtered[start..end]
            .iter()
            .enumerate()
            .map(move |(offset, &original_index)| {
                let filtered_index = start + offset;
                (
                    original_index,
                    &self.items[original_index],
                    filtered_index == selected,
                )
            })
    }

    fn scroll_to_selected(&mut self) {
        if self.filtered.is_empty() {
            self.scroll = 0;
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else {
            let bottom = self.scroll + self.max_visible;
            if self.selected >= bottom {
                self.scroll = self.selected + 1 - self.max_visible;
            }
        }
        // Clamp in case `max_visible` is larger than the filtered
        // view.
        let max_scroll = self.filtered.len().saturating_sub(self.max_visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<String> {
        ["apple", "apricot", "banana", "blueberry", "cherry"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    #[test]
    fn filter_narrows_visible_set() {
        let mut list = SelectList::new(words(), 10);
        list.apply_filter(|word| word.starts_with('b'));
        let visible: Vec<&str> = list.visible().map(|(_, w, _)| w.as_str()).collect();
        assert_eq!(visible, vec!["banana", "blueberry"]);
    }

    #[test]
    fn move_selection_wraps_at_bounds() {
        let mut list = SelectList::new(words(), 10);
        assert_eq!(list.selected().map(String::as_str), Some("apple"));
        list.move_selection(-1);
        assert_eq!(list.selected().map(String::as_str), Some("cherry"));
        list.move_selection(1);
        assert_eq!(list.selected().map(String::as_str), Some("apple"));
        list.move_selection(2);
        assert_eq!(list.selected().map(String::as_str), Some("banana"));
    }

    #[test]
    fn scroll_keeps_selection_visible_when_max_visible_small() {
        let mut list = SelectList::new(words(), 2);
        // Start at apple (row 0). Visible rows: apple, apricot.
        let visible: Vec<&str> = list.visible().map(|(_, w, _)| w.as_str()).collect();
        assert_eq!(visible, vec!["apple", "apricot"]);

        list.move_selection(2); // banana (row 2)
        let visible: Vec<&str> = list.visible().map(|(_, w, _)| w.as_str()).collect();
        assert_eq!(
            visible,
            vec!["apricot", "banana"],
            "selection should be in the bottom slot after a forward move"
        );

        list.move_selection(1); // blueberry (row 3)
        let visible: Vec<&str> = list.visible().map(|(_, w, _)| w.as_str()).collect();
        assert_eq!(visible, vec!["banana", "blueberry"]);

        // Wrap back to apple — scroll snaps up.
        list.move_selection(2); // cherry (row 4)
        list.move_selection(1); // wrap to apple (row 0)
        let visible: Vec<&str> = list.visible().map(|(_, w, _)| w.as_str()).collect();
        assert!(
            visible.first().copied() == Some("apple"),
            "scroll must follow wrap-around back to the top: {visible:?}"
        );
    }

    #[test]
    fn set_items_resets_selection_to_first_row() {
        let mut list = SelectList::new(words(), 10);
        list.move_selection(3); // blueberry
        list.set_items(vec!["one".to_string(), "two".into(), "three".into()]);
        assert_eq!(list.selected().map(String::as_str), Some("one"));
        assert_eq!(list.visible_len(), 3);
    }

    #[test]
    fn apply_filter_resets_selection_to_first_match() {
        let mut list = SelectList::new(words(), 10);
        list.move_selection(4); // cherry
        list.apply_filter(|w| w.contains('p'));
        assert_eq!(list.selected().map(String::as_str), Some("apple"));
    }

    #[test]
    fn height_hint_clamps_to_max_visible() {
        let list = SelectList::new(words(), 3);
        assert_eq!(list.height_hint(), 3);

        let list = SelectList::new(words(), 99);
        assert_eq!(list.height_hint(), 5, "clamps to the filtered set size");

        let mut list = SelectList::new(words(), 10);
        list.apply_filter(|w| w.starts_with('z'));
        assert_eq!(list.height_hint(), 0);
    }

    #[test]
    fn select_first_where_highlights_first_match() {
        let mut list = SelectList::new(words(), 10);
        list.select_first_where(|w| w.starts_with('b'));
        assert_eq!(list.selected().map(String::as_str), Some("banana"));
    }

    #[test]
    fn select_first_where_noop_when_no_match() {
        let mut list = SelectList::new(words(), 10);
        list.move_selection(2); // banana
        list.select_first_where(|w| w.starts_with('z'));
        assert_eq!(list.selected().map(String::as_str), Some("banana"));
    }

    #[test]
    fn visible_reports_selected_flag() {
        let list = SelectList::new(words(), 10);
        let selected: Vec<&str> = list
            .visible()
            .filter(|(_, _, is_selected)| *is_selected)
            .map(|(_, w, _)| w.as_str())
            .collect();
        assert_eq!(selected, vec!["apple"]);
    }

    #[test]
    fn empty_filtered_view_returns_no_selection() {
        let mut list = SelectList::new(words(), 10);
        list.apply_filter(|_| false);
        assert!(list.selected().is_none());
        assert_eq!(list.visible().count(), 0);
    }
}
