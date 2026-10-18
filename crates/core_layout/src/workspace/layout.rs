use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Computes target rectangles and visibility; negative gaps are treated as zero.
    pub fn compute_placements(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use rounding instead of truncation to prevent sub-pixel jitter
        let viewport_left = self.scroll_offset.round() as i32;

        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Computes non-fullscreen placements with scroll pinned to zero for the workspace overview.
    /// The caller supplies a viewport wide enough for the strip.
    pub fn placements_for_full_strip(&self, viewport: Rect) -> Vec<WindowPlacement> {
        self.compute_non_fullscreen_placements(viewport, 0)
    }

    fn compute_non_fullscreen_placements(
        &self,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        let mut placements = Vec::new();

        let gap = self.gap.max(0);
        let outer_left = self.outer_gap_left.max(0);
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);

        let vis_w = self.visible_width(viewport.width);
        let visible_right = viewport_left.saturating_add(vis_w);

        // Strip starts at 0 — outer gaps are viewport padding
        let mut current_x: i32 = 0;

        for (col_idx, column) in self.columns.iter().enumerate() {
            let eff_width = self.effective_column_width(column);

            let col_strip_x = current_x;
            let col_strip_right = col_strip_x.saturating_add(eff_width);

            // Signed arithmetic preserves negative positions for partially scrolled columns.
            let natural_screen_x = col_strip_x - viewport_left + viewport.x + outer_left;

            let visibility = if col_strip_right <= viewport_left {
                Visibility::OffScreenLeft
            } else if col_strip_x >= visible_right {
                Visibility::OffScreenRight
            } else {
                Visibility::Visible
            };

            // Keep off-screen columns beyond viewport padding when DWM cloaking is delayed or
            // ineffective.
            // The clamp boundary matches the cloak boundary in platform_win32 apply_placements, so
            // the jump is hidden.
            let col_screen_x = match visibility {
                Visibility::OffScreenRight => {
                    natural_screen_x.max(viewport.x.saturating_add(viewport.width))
                }
                Visibility::OffScreenLeft => {
                    natural_screen_x.min(viewport.x.saturating_sub(eff_width))
                }
                Visibility::Visible => natural_screen_x,
            };

            let visible_windows: Vec<(usize, WindowId)> = match column.mode() {
                crate::ColumnMode::Vertical => column
                    .windows()
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| !self.minimized_windows.contains(w))
                    .map(|(i, &w)| (i, w))
                    .collect(),
                crate::ColumnMode::Tabbed { .. } => {
                    // Rendering and the tab-strip overlay must select the same visible tab.
                    column
                        .effective_visible_tab(|w| self.minimized_windows.contains(&w))
                        .and_then(|i| column.windows().get(i).map(|&w| (i, w)))
                        .into_iter()
                        .collect()
                }
            };

            if !self.is_column_active(column) {
                continue;
            }

            // Fallback when cloaking fails: park hidden tabs a viewport width left, since SWP_NOSIZE
            // keeps their size.
            if column.is_tabbed() {
                let on_screen_idx = visible_windows.first().map(|(i, _)| *i);
                let offscreen_x = viewport.x.saturating_sub(viewport.width.max(1));
                for (i, &wid) in column.windows().iter().enumerate() {
                    if Some(i) == on_screen_idx {
                        continue;
                    }
                    if self.minimized_windows.contains(&wid) {
                        continue;
                    }
                    placements.push(WindowPlacement {
                        window_id: wid,
                        rect: Rect::new(offscreen_x, viewport.y, 0, 0),
                        visibility: Visibility::OffScreenLeft,
                        column_index: col_idx,
                    });
                }
            }

            // Reserve overlay space inside the column so the tab strip cannot land above the work
            // area.
            let column_top_reserve = if column.is_tabbed() {
                self.tab_strip_reserve_px.max(0)
            } else {
                0
            };

            let usable_height = viewport
                .height
                .saturating_sub(outer_top)
                .saturating_sub(outer_bottom)
                .saturating_sub(column_top_reserve)
                .max(0);
            let window_count = visible_windows.len() as i32;
            let window_gaps = if window_count > 1 {
                gap.saturating_mul(window_count - 1)
            } else {
                0
            };
            let available_height = (usable_height - window_gaps).max(0);

            // Recorded minima take priority; flexible windows share the remaining height by weight.
            // The last window absorbs leftover space.
            let visible_weights: Vec<f64> = if column.height_weights.len() == column.windows().len()
            {
                visible_windows
                    .iter()
                    .map(|(i, _)| column.height_weights[*i])
                    .collect()
            } else {
                vec![1.0; visible_windows.len()]
            };

            let min_heights: Vec<i32> = visible_windows
                .iter()
                .map(|(_, wid)| self.window_min_heights.get(wid).copied().unwrap_or(0))
                .collect();
            let total_min: i32 = min_heights.iter().sum();
            let flex_height = (available_height - total_min).max(0);

            let flex_weight_sum: f64 = visible_weights
                .iter()
                .zip(min_heights.iter())
                .filter(|(_, m)| **m == 0)
                .map(|(w, _)| *w)
                .sum();
            let has_flex = flex_weight_sum > 0.0;

            let mut current_y = viewport.y + outer_top + column_top_reserve;

            for (win_idx, &(_, window_id)) in visible_windows.iter().enumerate() {
                let is_last = win_idx == visible_windows.len() - 1;
                let height = if is_last {
                    // Honor the last window's minimum even when it overflows the viewport.
                    let remainder =
                        (viewport.y + viewport.height - outer_bottom - current_y).max(0);
                    remainder.max(min_heights[win_idx])
                } else if min_heights[win_idx] > 0 {
                    min_heights[win_idx]
                } else if has_flex {
                    let share = visible_weights[win_idx] / flex_weight_sum;
                    (flex_height as f64 * share).round() as i32
                } else {
                    available_height / visible_windows.len().max(1) as i32
                };

                placements.push(WindowPlacement {
                    window_id,
                    rect: Rect::new(col_screen_x, current_y, eff_width, height),
                    visibility,
                    column_index: col_idx,
                });

                current_y = current_y.saturating_add(height).saturating_add(gap);
            }

            current_x = current_x.saturating_add(eff_width).saturating_add(gap);
        }

        for floating in &self.floating_windows {
            if self.minimized_windows.contains(&floating.id) {
                continue;
            }
            placements.push(WindowPlacement {
                window_id: floating.id,
                rect: floating.rect,
                visibility: Visibility::Visible,
                column_index: usize::MAX, // Sentinel for floating windows
            });
        }

        placements
    }

    /// Computes placements using the animated scroll offset when an animation is active.
    pub fn compute_placements_animated(&self, viewport: Rect) -> Vec<WindowPlacement> {
        let viewport_left = self.effective_scroll_offset().round() as i32;

        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    // Hidden windows keep their real rects: they move with SWP_NOSIZE, so a zeroed rect would snap
    // them to the top-left.
    // Pinned floating windows remain visible above fullscreen.
    fn compute_fullscreen_placements(
        &self,
        fs_wid: WindowId,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
            return self.compute_non_fullscreen_placements(viewport, viewport_left);
        }

        let mut placements = self.compute_non_fullscreen_placements(viewport, viewport_left);

        for placement in &mut placements {
            if placement.window_id == fs_wid {
                placement.rect = viewport;
                placement.visibility = Visibility::Visible;
            } else if placement.column_index == usize::MAX
                && self
                    .floating_windows
                    .iter()
                    .any(|f| f.id == placement.window_id && f.pinned)
            {
            } else {
                placement.visibility = Visibility::OffScreenLeft;
            }
        }

        placements
    }
}
