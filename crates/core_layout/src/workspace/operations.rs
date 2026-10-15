use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    fn column_x(&self, column_index: usize) -> i32 {
        self.column_x_with_minimized_handling(column_index, true)
    }

    /// Compute the X position of a column, optionally skipping minimized columns.
    fn column_x_with_minimized_handling(&self, column_index: usize, skip_minimized: bool) -> i32 {
        // Defensively clamp gaps to >= 0
        let gap = self.gap.max(0);

        // Strip coordinates start at 0 — outer gaps are viewport padding,
        // not part of the scrollable strip.
        let mut x = 0;
        for (i, col) in self.columns.iter().enumerate() {
            if i == column_index {
                return x;
            }
            // Skip fully-minimized columns when requested
            if skip_minimized && !self.is_column_active(col) {
                continue;
            }
            x = x
                .saturating_add(self.effective_column_width(col))
                .saturating_add(gap);
        }
        x
    }

    /// Get the x-coordinate and width of the focused column.
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

    /// Whether the focused column should be centered under the current
    /// centering mode. `OnOverflow` centers only when the column is wider
    /// than the visible area (it cannot fit otherwise).
    fn should_center(&self, col_width: i32, vis_w: i32) -> bool {
        match self.centering_mode {
            CenteringMode::Center => true,
            CenteringMode::JustInView => false,
            CenteringMode::OnOverflow => col_width > vis_w,
        }
    }

    /// Ensure the focused column is visible in the viewport.
    /// Adjusts scroll_offset according to the centering mode.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        // Outer gaps are viewport padding — visible strip area is smaller.
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

    /// Resize the focused column by a delta amount.
    pub fn resize_focused_column(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.maximized_column = None;
        if let Some(column) = self.columns.get(self.focused_column) {
            let new_width = self.effective_column_width(column).saturating_add(delta);
            let column = &mut self.columns[self.focused_column];
            column.set_width(new_width);
            // Clear cached min-widths and min-heights so constraints will be
            // re-detected from the actual window size on the next apply cycle.
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

    /// Move the focused column to the start (leftmost) of the strip.
    pub fn move_column_to_start(&mut self) {
        self.reorder_column(self.focused_column, 0);
    }

    /// Move the focused column to the end (rightmost) of the strip.
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

        // Update focused_column to track correctly after the shift.
        if self.focused_column == from {
            self.focused_column = to;
        } else if from < to {
            // Column moved forward: indices in (from, to] shifted left by 1
            if self.focused_column > from && self.focused_column <= to {
                self.focused_column -= 1;
            }
        } else {
            // Column moved backward: indices in [to, from) shifted right by 1
            if self.focused_column >= to && self.focused_column < from {
                self.focused_column += 1;
            }
        }
        self.clamp_focus_indices();
    }

    /// Remove an entire column and return it. Used for cross-monitor drag.
    /// Returns `None` if index is out of bounds.
    pub fn remove_column(&mut self, index: usize) -> Option<Column> {
        if index >= self.columns.len() {
            return None;
        }
        let col = self.columns.remove(index);
        for wid in col.windows() {
            self.minimized_windows.remove(wid);
            // Clear fullscreen if the removed column contained the fullscreen window
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
        Some(col)
    }

    /// Insert a column at the given index. Used for cross-monitor drag.
    /// Index is clamped to `columns.len()`. Empty columns are rejected to
    /// preserve the invariant that all columns contain at least one window.
    pub fn insert_column_at(&mut self, column: Column, index: usize) {
        if column.is_empty() {
            return;
        }
        // Reject if any window already exists in this workspace
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

    /// Join the nearest active column to the left, skipping minimized-only columns.
    /// If none exists, unstack to the left (no-op for a lone window).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    /// In a Tabbed receiver, the moved window becomes the new active tab
    /// (consistent with the "user-initiated keyboard move" intent).
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
        // Clamp focus in old column (column.remove_at_index already adjusted
        // active_idx for Tabbed; here we clamp focused_window_in_column).
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
        // Focus the new column (it took the current index)
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
        // If the focused column is tabbed, make the consumed window the
        // active (visible) tab rather than leaving the old tab showing.
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one above in the same column.
    /// In a Tabbed column, `swap_windows` keeps `active_idx` tracking the
    /// same window (handled inside `Column::swap_windows`).
    pub fn move_window_up_in_column(&mut self) {
        if self.focused_window_in_column == 0 {
            return;
        }
        self.columns[self.focused_column].swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column - 1,
        );
        self.focused_window_in_column -= 1;
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one below in the same column.
    pub fn move_window_down_in_column(&mut self) {
        if self.focused_window_in_column + 1 >= self.columns[self.focused_column].len() {
            return;
        }
        self.columns[self.focused_column].swap_windows(
            self.focused_window_in_column,
            self.focused_window_in_column + 1,
        );
        self.focused_window_in_column += 1;
        self.sync_active_tab_to_focus();
    }

    /// Scroll the viewport by a pixel delta.
    ///
    /// Cancels any active scroll animation so the manual scroll takes effect
    /// immediately. Special float values (NaN, Infinity) are treated as zero.
    pub fn scroll_by(&mut self, delta: f64, viewport_width: i32) {
        // Cancel any in-flight animation so manual scroll is not overridden
        self.cancel_animation();
        // Treat NaN and Infinity as zero for safety
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

    // ========================================================================
    // Animation Methods
    // ========================================================================

    /// Check if a scroll animation is currently active.
    pub fn is_animating(&self) -> bool {
        self.active_animation.is_some()
    }

    /// Get the current effective scroll offset.
    /// Returns the animated offset if an animation is active, otherwise the base offset.
    pub fn effective_scroll_offset(&self) -> f64 {
        match &self.active_animation {
            Some(anim) => anim.current_offset(),
            None => self.scroll_offset,
        }
    }

    /// Start an animated scroll to a target offset.
    /// If an animation is already active, it will be cancelled and a new one started.
    pub fn start_scroll_animation(
        &mut self,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        // Clamp target to valid range (visible area = viewport minus outer padding)
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        let clamped_target =
            self.clamp_scroll_offset(target, viewport_width, 0.0, max_scroll as f64);
        self.animate_scroll_to(clamped_target, duration_ms, easing);
    }

    fn animate_scroll_to(&mut self, target: f64, duration_ms: Option<u64>, easing: Option<Easing>) {
        // Use current effective position as start (handles interrupting animations)
        let start = self.effective_scroll_offset();

        // If already at target, no animation needed
        if (start - target).abs() < 0.5 {
            self.scroll_offset = target;
            self.active_animation = None;
            return;
        }

        // Explicit args win; otherwise use this workspace's configured
        // scroll params (set by the daemon from `[animation]`).
        let duration = duration_ms.unwrap_or(self.scroll_duration_ms);
        let ease = easing.unwrap_or(self.scroll_easing);

        self.active_animation = Some(ScrollAnimation::new(start, target, duration, ease));
    }

    /// Advance the active animation by the given delta time in milliseconds.
    /// Returns true if an animation is still active, false if complete or no animation.
    pub fn tick_animation(&mut self, delta_ms: u64) -> bool {
        let Some(anim) = &mut self.active_animation else {
            return false;
        };

        let still_running = anim.tick(delta_ms);

        if !still_running {
            // Animation complete - finalize scroll offset and clear animation
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

    /// Ensure the focused column is visible with animation.
    /// Like `ensure_focused_visible` but animates the scroll instead of jumping.
    /// Snaps instantly when `reduce_motion` is set.
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

    /// Center the focused column in the viewport, regardless of centering mode.
    /// When `center_past_edges` is true, first/last columns truly center with
    /// empty space; otherwise the scroll is clamped to content boundaries.
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
            // Unclamped — allow negative offsets and scrolling past the end
            if self.reduce_motion {
                self.stop_animation();
                self.scroll_offset = target;
                return;
            }

            self.animate_scroll_to(target, None, None);
        } else {
            // Clamped — use the standard scroll animation path
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
