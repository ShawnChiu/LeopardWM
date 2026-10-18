use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    fn column_x(&self, column_index: usize) -> i32 {
        self.column_x_with_minimized_handling(column_index, true)
    }

    fn column_x_with_minimized_handling(&self, column_index: usize, skip_minimized: bool) -> i32 {
        let gap = self.gap.max(0);

        // Strip coordinates start at 0 — outer gaps are viewport padding,
        // not part of the scrollable strip.
        let mut x = 0;
        for (i, col) in self.columns.iter().enumerate() {
            if i == column_index {
                return x;
            }
            if skip_minimized && !self.is_column_active(col) {
                continue;
            }
            x = x
                .saturating_add(self.effective_column_width(col))
                .saturating_add(gap);
        }
        x
    }

    fn focused_column_bounds(&self) -> Option<(i32, i32)> {
        self.columns.get(self.focused_column).map(|col| {
            let x = self.column_x(self.focused_column);
            (x, self.effective_column_width(col))
        })
    }

    /// The width of the visible strip area inside the viewport (viewport minus outer padding).
    pub fn visible_width(&self, viewport_width: i32) -> i32 {
        viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .max(0)
    }

    pub(super) fn scroll_bounds(
        &self,
        viewport_width: i32,
        min_scroll: f64,
        max_scroll: f64,
    ) -> (f64, f64) {
        if self.center_single_column && self.fullscreen_window.is_none() {
            let mut active = self
                .columns
                .iter()
                .filter(|column| self.is_column_active(column));
            if let Some(column) = active.next() {
                let width = self.effective_column_width(column);
                let visible_width = self.visible_width(viewport_width);
                if active.next().is_none() && width <= visible_width {
                    let offset = (-(visible_width - width) / 2) as f64;
                    return (offset, offset);
                }
            }
        }
        (min_scroll, max_scroll)
    }

    pub(super) fn clamp_scroll_offset(
        &self,
        offset: f64,
        viewport_width: i32,
        min_scroll: f64,
        max_scroll: f64,
    ) -> f64 {
        let (min_scroll, max_scroll) = self.scroll_bounds(viewport_width, min_scroll, max_scroll);
        offset.clamp(min_scroll, max_scroll)
    }

    // OnOverflow centers columns too wide to fit in the visible area.
    fn should_center(&self, col_width: i32, vis_w: i32) -> bool {
        match self.centering_mode {
            CenteringMode::Center => true,
            CenteringMode::JustInView => false,
            CenteringMode::OnOverflow => col_width > vis_w,
        }
    }

    /// Reveals the focused column according to the centering mode.
    /// Negative gaps are treated as zero.
    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);

        if self.should_center(col_width, vis_w) {
            let col_center = col_x.saturating_add(col_width / 2);
            self.scroll_offset = (col_center.saturating_sub(vis_w / 2)) as f64;
        } else {
            let scroll_left = self.scroll_offset.round() as i32;
            let scroll_right = scroll_left.saturating_add(vis_w);
            let col_right = col_x.saturating_add(col_width);

            if col_x < scroll_left {
                self.scroll_offset = col_x as f64;
            } else if col_right > scroll_right {
                self.scroll_offset = col_right.saturating_sub(vis_w) as f64;
            }
        }

        let max_scroll = (self.total_width() - vis_w).max(0) as f64;
        let min_scroll = if self.center_past_edges {
            f64::NEG_INFINITY
        } else {
            0.0
        };
        self.scroll_offset =
            self.clamp_scroll_offset(self.scroll_offset, viewport_width, min_scroll, max_scroll);
    }

    pub fn resize_focused_column(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.maximized_column = None;
        if let Some(column) = self.columns.get(self.focused_column) {
            let new_width = self.effective_column_width(column).saturating_add(delta);
            let column = &mut self.columns[self.focused_column];
            column.set_width(new_width);
            // Resize invalidates measured minima; the next apply re-detects them.
            for wid in column.windows() {
                self.window_min_widths.remove(wid);
                self.window_min_heights.remove(wid);
            }
        }
    }

    fn nearest_active_column_left(&self, column_index: usize) -> Option<usize> {
        (0..column_index)
            .rev()
            .find(|&index| self.is_column_active(&self.columns[index]))
    }

    fn nearest_active_column_right(&self, column_index: usize) -> Option<usize> {
        (column_index + 1..self.columns.len())
            .find(|&index| self.is_column_active(&self.columns[index]))
    }

    /// Swap the focused column with the nearest active column to its left,
    /// skipping minimized-only columns without moving them.
    pub fn move_column_left(&mut self) {
        if let Some(left) = self.nearest_active_column_left(self.focused_column) {
            self.columns.swap(self.focused_column, left);
            self.focused_column = left;
        }
    }

    /// Swap the focused column with the nearest active column to its right,
    /// skipping minimized-only columns without moving them.
    pub fn move_column_right(&mut self) {
        if let Some(right) = self.nearest_active_column_right(self.focused_column) {
            self.columns.swap(self.focused_column, right);
            self.focused_column = right;
        }
    }

    pub fn move_column_to_start(&mut self) {
        self.reorder_column(self.focused_column, 0);
    }

    pub fn move_column_to_end(&mut self) {
        if !self.columns.is_empty() {
            self.reorder_column(self.focused_column, self.columns.len() - 1);
        }
    }

    /// Move a column from one index to another, shifting intermediate columns.
    /// No-op if indices are equal or out of bounds.
    pub fn reorder_column(&mut self, from: usize, to: usize) {
        if from == to || from >= self.columns.len() || to >= self.columns.len() {
            return;
        }
        let column = self.columns.remove(from);
        self.columns.insert(to, column);

        if self.focused_column == from {
            self.focused_column = to;
        } else if from < to {
            if self.focused_column > from && self.focused_column <= to {
                self.focused_column -= 1;
            }
        } else {
            if self.focused_column >= to && self.focused_column < from {
                self.focused_column += 1;
            }
        }
        self.clamp_focus_indices();
    }

    /// Removes and returns a column; out-of-bounds indices return `None`.
    pub fn remove_column(&mut self, index: usize) -> Option<Column> {
        if index >= self.columns.len() {
            return None;
        }
        let col = self.columns.remove(index);
        for wid in col.windows() {
            self.minimized_windows.remove(wid);
            if self.fullscreen_window == Some(*wid) {
                self.fullscreen_window = None;
            }
        }
        if self.columns.is_empty() {
            self.focused_column = 0;
            self.focused_window_in_column = 0;
            self.scroll_offset = 0.0;
        } else {
            if self.focused_column > index {
                self.focused_column -= 1;
            }
            // Outside fullscreen, centering needs a viewport; defer bounds repair
            // to reveal/reconcile so removal preserves the animation's start.
            if !self.center_single_column || self.fullscreen_window.is_some() {
                let max_scroll = self.total_width().max(0) as f64;
                self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll);
            }
        }
        self.clamp_focus_indices();
        self.sync_active_tab_to_focus();
        Some(col)
    }

    /// Inserts a column at an index clamped to the column count.
    /// Rejects empty columns and columns containing a window already in this workspace.
    pub fn insert_column_at(&mut self, column: Column, index: usize) {
        if column.is_empty() {
            return;
        }
        if column
            .windows()
            .iter()
            .any(|wid| self.contains_window(*wid))
        {
            return;
        }
        let clamped = index.min(self.columns.len());
        let was_empty = self.columns.is_empty();
        self.columns.insert(clamped, column);
        if !was_empty && self.focused_column >= clamped {
            self.focused_column += 1;
        }
        self.clamp_focus_indices();
    }

    /// Moves the focused window into the nearest active left column, or unstacks left if none
    /// exists.
    /// Focus follows the window, including the receiver's active tab; empty source columns are
    /// removed.
    pub fn move_window_left(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let Some(target_idx) = self.nearest_active_column_left(self.focused_column) else {
            self.expel_to_left();
            return;
        };
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
        }
        self.columns[target_idx].add_window(wid);
        self.focused_column = target_idx;
        self.focused_window_in_column = self.columns[target_idx].len() - 1;
        self.sync_active_tab_to_focus();
    }

    /// Join the nearest active column to the right, skipping minimized-only columns.
    /// If none exists, unstack to the right (no-op for a lone window).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    /// In a Tabbed receiver, the moved window becomes the new active tab.
    pub fn move_window_right(&mut self) {
        let Some(mut target_idx) = self.nearest_active_column_right(self.focused_column) else {
            self.expel_to_right();
            return;
        };
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
            target_idx -= 1;
        }
        self.columns[target_idx].add_window(wid);
        self.focused_column = target_idx;
        self.focused_window_in_column = self.columns[target_idx].len() - 1;
        self.sync_active_tab_to_focus();
    }

    /// Push the focused window out to a new column on the left.
    /// The new column is always Vertical (single-window).
    pub fn expel_to_left(&mut self) {
        if self.columns.is_empty() || self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let mut new_col = Column::new(wid, width);
        new_col.width_fraction_cache = self.columns[self.focused_column]
            .width_fraction_cache
            .clone();
        self.columns.insert(self.focused_column, new_col);
        self.focused_window_in_column = 0;
        // sync not needed: new column is Vertical.
    }

    /// Push the focused window out to a new column on the right.
    /// The new column is always Vertical (single-window).
    pub fn expel_to_right(&mut self) {
        if self.columns.is_empty() || self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) =
            self.columns[self.focused_column].remove_at_index(self.focused_window_in_column)
        else {
            return;
        };
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let mut new_col = Column::new(wid, width);
        new_col.width_fraction_cache = self.columns[self.focused_column]
            .width_fraction_cache
            .clone();
        self.columns.insert(self.focused_column + 1, new_col);
        self.focused_column += 1;
        self.focused_window_in_column = 0;
    }

    /// Append and focus the first non-minimized window from the nearest
    /// active column to the right, skipping minimized-only columns.
    /// Remove the source column if it empties.
    /// No-op if there is no non-minimized window to the right.
    pub fn consume_from_right(&mut self) {
        if let Some(right) = self.nearest_active_column_right(self.focused_column) {
            self.consume_from_column(right);
        }
    }

    /// Append and focus the first non-minimized window from the nearest
    /// active column to the left, skipping minimized-only columns.
    /// Remove the source column if it empties.
    /// No-op if there is no non-minimized window to the left.
    pub fn consume_from_left(&mut self) {
        if let Some(left) = self.nearest_active_column_left(self.focused_column) {
            self.consume_from_column(left);
        }
    }

    fn consume_from_column(&mut self, source: usize) {
        let Some(window_index) = self.columns[source]
            .windows()
            .iter()
            .position(|wid| !self.minimized_windows.contains(wid))
        else {
            return;
        };
        let Some(wid) = self.columns[source].remove_at_index(window_index) else {
            return;
        };
        if self.columns[source].is_empty() {
            self.columns.remove(source);
            if source < self.focused_column {
                self.focused_column -= 1;
            }
        }
        self.columns[self.focused_column].add_window(wid);
        self.focused_window_in_column = self.columns[self.focused_column].len().saturating_sub(1);
        self.sync_active_tab_to_focus();
    }

    /// Swaps the focused window upward while preserving its active-tab identity.
    /// No-op without a valid focused tiled window or at the top.
    pub fn move_window_up_in_column(&mut self) {
        let Some(column) = self.columns.get_mut(self.focused_column) else {
            return;
        };
        if self.focused_window_in_column == 0 || self.focused_window_in_column >= column.len() {
            return;
        }
        column.swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column - 1,
        );
        self.focused_window_in_column -= 1;
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one below in the same column.
    /// No-op when there is no focused tiled window or it is already at the bottom.
    pub fn move_window_down_in_column(&mut self) {
        let Some(column) = self.columns.get_mut(self.focused_column) else {
            return;
        };
        if self.focused_window_in_column >= column.len().saturating_sub(1) {
            return;
        }
        column.swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column + 1,
        );
        self.focused_window_in_column += 1;
        self.sync_active_tab_to_focus();
    }

    /// Scrolls by a pixel delta and cancels active animation immediately.
    /// Non-finite deltas are treated as zero.
    pub fn scroll_by(&mut self, delta: f64, viewport_width: i32) {
        self.cancel_animation();
        let safe_delta = if delta.is_finite() { delta } else { 0.0 };
        self.scroll_offset += safe_delta;
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        self.scroll_offset =
            self.clamp_scroll_offset(self.scroll_offset, viewport_width, 0.0, max_scroll as f64);
    }

    /// Repair bounds without revealing focus; preserve valid animations and edge centering.
    pub fn reconcile_scroll_bounds(&mut self, viewport_width: i32) {
        let vis_w = self.visible_width(viewport_width);
        let mut min_scroll = 0.0_f64;
        let mut max_scroll = (self.total_width() - vis_w).max(0) as f64;
        if self.center_past_edges {
            let mut active = self
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| self.is_column_active(column));
            if let Some((first_idx, first)) = active.next() {
                let (last_idx, last) = active.next_back().unwrap_or((first_idx, first));
                let first_center = self.effective_column_width(first) / 2;
                let last_center = self
                    .column_x(last_idx)
                    .saturating_add(self.effective_column_width(last) / 2);
                min_scroll = min_scroll.min(first_center.saturating_sub(vis_w / 2) as f64);
                max_scroll = max_scroll.max(last_center.saturating_sub(vis_w / 2) as f64);
            }
        }
        let (min_scroll, max_scroll) = self.scroll_bounds(viewport_width, min_scroll, max_scroll);
        let current = self.effective_scroll_offset();
        let target = self
            .active_animation
            .as_ref()
            .map_or(current, |anim| anim.target());
        let bounds = min_scroll..=max_scroll;
        if bounds.contains(&target)
            && (bounds.contains(&current)
                || (self.center_single_column
                    && self.fullscreen_window.is_none()
                    && self.active_animation.is_some()))
        {
            return;
        }
        self.cancel_animation();
        self.scroll_offset = self.scroll_offset.clamp(min_scroll, max_scroll);
    }

    pub fn is_animating(&self) -> bool {
        self.active_animation.is_some()
    }

    /// Returns the animated offset when active, otherwise the base offset.
    pub fn effective_scroll_offset(&self) -> f64 {
        match &self.active_animation {
            Some(anim) => anim.current_offset(),
            None => self.scroll_offset,
        }
    }

    /// Starts scrolling to a clamped target, replacing any active animation.
    pub fn start_scroll_animation(
        &mut self,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        let clamped_target =
            self.clamp_scroll_offset(target, viewport_width, 0.0, max_scroll as f64);
        self.animate_scroll_to(clamped_target, duration_ms, easing);
    }

    fn animate_scroll_to(&mut self, target: f64, duration_ms: Option<u64>, easing: Option<Easing>) {
        // Use current effective position as start (handles interrupting animations)
        let start = self.effective_scroll_offset();

        if (start - target).abs() < 0.5 {
            self.scroll_offset = target;
            self.active_animation = None;
            return;
        }

        // Omitted arguments use the workspace's configured animation parameters.
        let duration = duration_ms.unwrap_or(self.scroll_duration_ms);
        let ease = easing.unwrap_or(self.scroll_easing);

        self.active_animation = Some(ScrollAnimation::new(start, target, duration, ease));
    }

    /// Advances by milliseconds; returns whether an animation remains active.
    pub fn tick_animation(&mut self, delta_ms: u64) -> bool {
        let Some(anim) = &mut self.active_animation else {
            return false;
        };

        let still_running = anim.tick(delta_ms);

        if !still_running {
            self.scroll_offset = anim.target();
            self.active_animation = None;
            false
        } else {
            true
        }
    }

    /// Stop the current animation and snap to the target position.
    pub fn stop_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = anim.target();
        }
    }

    /// Cancel the current animation and stay at the current position.
    pub fn cancel_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = anim.current_offset();
        }
    }

    /// Animates focus reveal according to centering mode, or snaps when `reduce_motion` is set.
    pub fn ensure_focused_visible_animated(&mut self, viewport_width: i32) {
        if self.reduce_motion {
            self.stop_animation();
            self.ensure_focused_visible(viewport_width);
            return;
        }
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);

        let target_offset = if self.should_center(col_width, vis_w) {
            let col_center = col_x.saturating_add(col_width / 2);
            (col_center.saturating_sub(vis_w / 2)) as f64
        } else {
            // Reveal must remain valid at landing, not only at the interpolated position.
            let current = self
                .active_animation
                .as_ref()
                .map_or(self.scroll_offset, |anim| anim.target());
            let scroll_left = current.round() as i32;
            let scroll_right = scroll_left.saturating_add(vis_w);
            let col_right = col_x.saturating_add(col_width);

            if col_x < scroll_left {
                col_x as f64
            } else if col_right > scroll_right {
                col_right.saturating_sub(vis_w) as f64
            } else {
                let max_scroll = (self.total_width() - vis_w).max(0) as f64;
                if current < -0.5 && !self.center_past_edges {
                    0.0
                } else if current > max_scroll + 0.5 {
                    max_scroll
                } else if self.center_single_column {
                    current
                } else {
                    return;
                }
            }
        };

        let max_scroll = (self.total_width() - vis_w).max(0) as f64;
        let min_scroll = if self.center_past_edges {
            f64::NEG_INFINITY
        } else {
            0.0
        };
        let target =
            self.clamp_scroll_offset(target_offset, viewport_width, min_scroll, max_scroll);
        self.animate_scroll_to(target, None, None);
    }

    /// Centers the focused column regardless of centering mode.
    /// Uses content bounds unless `center_past_edges` is set, honoring single-column centering.
    pub fn center_focused_column_animated(&mut self, viewport_width: i32) {
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);
        let col_center = col_x.saturating_add(col_width / 2);
        let target = self.clamp_scroll_offset(
            (col_center - vis_w / 2) as f64,
            viewport_width,
            f64::NEG_INFINITY,
            f64::INFINITY,
        );

        if self.center_past_edges {
            if self.reduce_motion {
                self.stop_animation();
                self.scroll_offset = target;
                return;
            }

            self.animate_scroll_to(target, None, None);
        } else {
            if self.reduce_motion {
                self.stop_animation();
                let max_scroll = (self.total_width() - vis_w).max(0);
                self.scroll_offset =
                    self.clamp_scroll_offset(target, viewport_width, 0.0, max_scroll as f64);
                return;
            }

            self.start_scroll_animation(target, viewport_width, None, None);
        }
    }
}
