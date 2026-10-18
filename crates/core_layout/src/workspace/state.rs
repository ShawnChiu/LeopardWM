use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Excludes a managed window from placements without removing it and exits fullscreen if it was
    /// the target.
    /// Returns whether the window was newly added to the minimized set.
    pub fn mark_minimized(&mut self, window_id: WindowId) -> bool {
        let is_tiled = self.find_window_location(window_id).is_some();
        let is_floating = self.is_floating(window_id);
        if is_tiled || is_floating {
            if self.fullscreen_window == Some(window_id) {
                self.fullscreen_window = None;
            }
            // Composition changes invalidate measured minima for this window and its column
            // siblings.
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            if is_tiled {
                if let Some((col_idx, _)) = self.find_window_location(window_id) {
                    if let Some(col) = self.columns.get(col_idx) {
                        for &sibling in col.windows() {
                            if sibling != window_id {
                                self.window_min_widths.remove(&sibling);
                                self.window_min_heights.remove(&sibling);
                            }
                        }
                    }
                }
                // Cancel active animation — its target is now stale after minimize
                self.active_animation = None;
            }
            self.minimized_windows.insert(window_id)
        } else {
            false
        }
    }

    /// Restores a window; returns whether it was marked minimized.
    pub fn mark_restored(&mut self, window_id: WindowId) -> bool {
        // Constraints may change while minimized; the next apply re-detects them.
        self.window_min_widths.remove(&window_id);
        self.window_min_heights.remove(&window_id);
        self.minimized_windows.remove(&window_id)
    }

    pub fn is_minimized(&self, window_id: WindowId) -> bool {
        self.minimized_windows.contains(&window_id)
    }

    pub fn minimized_count(&self) -> usize {
        self.minimized_windows.len()
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen_window.is_some()
    }

    pub fn fullscreen_window_id(&self) -> Option<WindowId> {
        self.fullscreen_window
    }

    /// Clears fullscreen only for the matching target; returns whether it was cleared.
    pub fn clear_fullscreen_if_window(&mut self, window_id: WindowId) -> bool {
        if self.fullscreen_window == Some(window_id) {
            self.fullscreen_window = None;
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            true
        } else {
            false
        }
    }

    /// Toggle fullscreen mode for the focused window.
    /// Returns true if entering fullscreen, false if exiting.
    pub fn toggle_fullscreen(&mut self) -> bool {
        if let Some(fs_wid) = self.fullscreen_window {
            // Fullscreen measurements reflect the viewport, not genuine window minima.
            self.window_min_widths.remove(&fs_wid);
            self.window_min_heights.remove(&fs_wid);

            if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
                self.fullscreen_window = None;
            } else {
                self.fullscreen_window = None;
                return false;
            }
        }

        if let Some(wid) = self.focused_visible_window() {
            self.fullscreen_window = Some(wid);
            true
        } else {
            self.fullscreen_window = None;
            false
        }
    }

    /// Retargets active fullscreen to the focused visible window and clears the previous target's
    /// viewport-inflated minima.
    /// Returns whether the target changed.
    pub fn fullscreen_follow_focus(&mut self) -> bool {
        let Some(prev) = self.fullscreen_window else {
            return false;
        };
        let Some(wid) = self.focused_visible_window() else {
            return false;
        };
        if wid == prev {
            return false;
        }
        self.window_min_widths.remove(&prev);
        self.window_min_heights.remove(&prev);
        self.fullscreen_window = Some(wid);
        true
    }

    /// Moves the focused tiled window to a centered floating rectangle; column focus does not
    /// target floating windows.
    /// Returns the moved window ID, if any.
    pub fn toggle_floating(&mut self, viewport: Rect) -> Option<WindowId> {
        let wid = self.focused_window()?;

        self.clear_fullscreen_if_window(wid);

        // The left neighbor anchors restoration even if column indices change.
        let origin = self.find_window_location(wid).map(|(col_idx, _)| {
            let left_neighbor = if col_idx > 0 {
                self.columns[col_idx - 1].windows.first().copied()
            } else {
                None
            };
            (left_neighbor, col_idx)
        });

        let _ = self.remove_window(wid);

        if let Some(origin) = origin {
            self.float_origin_column.insert(wid, origin);
        }

        let float_w = 800.min(viewport.width - 40);
        let float_h = 600.min(viewport.height - 40);
        let float_x = viewport.x + (viewport.width - float_w) / 2;
        let float_y = viewport.y + (viewport.height - float_h) / 2;
        let rect = Rect::new(float_x, float_y, float_w, float_h);

        let _ = self.add_floating(wid, rect);
        Some(wid)
    }

    /// Returns a floating window to its saved column position when available.
    /// Returns whether the window was unfloated.
    pub fn unfloat_window(&mut self, window_id: WindowId) -> bool {
        // Read origin before remove_floating (which clears it)
        let origin = self.float_origin_column.remove(&window_id);
        if self.remove_floating(window_id) {
            if let Some((left_neighbor, fallback_idx)) = origin {
                let target = if let Some(neighbor_id) = left_neighbor {
                    self.find_window_location(neighbor_id)
                        .map(|(col_idx, _)| col_idx + 1)
                        .unwrap_or_else(|| fallback_idx.min(self.columns.len()))
                } else {
                    0
                };
                let column_width = self.default_column_width.max(crate::MIN_COLUMN_WIDTH);
                let column = Column::new(window_id, column_width);
                let target = target.min(self.columns.len());
                self.insert_column_at(column, target);
                self.focused_column = target;
                self.focused_window_in_column = 0;
            } else {
                let _ = self.insert_window(window_id, None);
            }
            true
        } else {
            false
        }
    }
}
