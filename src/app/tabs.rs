//! Tabs: the documents opened in a pane, as a strip under the top bar. Only
//! the active tab's document is open; switching reopens the other one at its
//! saved position. Dragging a tab into the other half of the window opens it
//! there (split view); dragging a pane's last tab away closes that pane.

use super::*;
use crate::library;

#[derive(Clone, Debug)]
pub(super) struct Tab {
    pub uri: String,
    pub name: String,
}

/// What a touch on the tab strip turned into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TabAct {
    Press,
    Scroll,
    Drag,
}

impl App {
    // ----- geometry ---------------------------------------------------------------------

    fn tabs_h(&self) -> f32 {
        self.dp(44.0)
    }

    /// The strip is shown (with the overlay) while reading a document.
    pub(super) fn tabs_shown(&self) -> bool {
        self.pane.mode == Mode::Read && !self.pane.tabs.is_empty() && (self.pane.doc.is_some() || self.pane.pending.is_some())
    }

    /// The strip's rect (viewport coordinates).
    fn tabs_rect(&self) -> [f32; 4] {
        [0.0, self.bar_bottom(), self.pane.vw, self.tabs_h()]
    }

    /// Bottom of the top bar plus the tab strip, if shown.
    pub(super) fn overlay_bottom(&self) -> f32 {
        self.bar_bottom() + if self.tabs_shown() { self.tabs_h() } else { 0.0 }
    }

    /// Where a tab dropped at window point (`wx`, `wy`) would go: the other
    /// pane, or the half of the window a new pane would take. Its window rect.
    fn drop_rect(&self, wx: f32, wy: f32) -> Option<[f32; 4]> {
        if let Some(o) = &self.other {
            return o.contains(wx, wy).then_some([o.x0, o.y0, o.w, o.vh]);
        }
        let (r, _) = self.slot_rect_in(1, true);
        inside(r, wx, wy).then_some(r)
    }

    /// The finger of a tab drag, in window coordinates.
    fn drag_point(&self, x: f32, y: f32) -> (f32, f32) {
        (x + self.pane.x0 + self.pane.ox, y + self.pane.y0)
    }

    // ----- actions ----------------------------------------------------------------------

    /// A document opened in the focused pane: its tab (new, after the current one) becomes active.
    pub(super) fn add_tab(&mut self, uri: &str, name: &str) {
        let pn = &mut self.pane;
        match pn.tabs.iter().position(|t| t.uri == uri) {
            Some(i) => {
                pn.tabs[i].name = name.to_string();
                pn.tab = i;
            }
            None => {
                let i = if pn.tabs.is_empty() { 0 } else { (pn.tab + 1).min(pn.tabs.len()) };
                pn.tabs.insert(i, Tab { uri: uri.to_string(), name: name.to_string() });
                pn.tab = i;
            }
        }
        pn.tab_follow = true;
    }

    /// Shows tab `i` (opens its document).
    fn switch_tab(&mut self, i: usize, p: &mut dyn Platform) {
        let Some(t) = self.pane.tabs.get(i) else { return };
        if self.pane.doc.as_ref().is_some_and(|d| d.uri == t.uri) && self.pane.pending.is_none() {
            self.pane.tab = i;
            return;
        }
        let uri = t.uri.clone();
        self.open_uri(&uri, p);
    }

    /// Closes tab `i`; the neighbour takes its place, and without tabs the pane
    /// closes (split view) or the document does (library / file picker).
    pub(super) fn close_tab(&mut self, i: usize, p: &mut dyn Platform) {
        let pn = &mut self.pane;
        if i >= pn.tabs.len() {
            return;
        }
        let was_active = i == pn.tab;
        pn.tabs.remove(i);
        if pn.tab > i {
            pn.tab -= 1;
        }
        pn.tab = pn.tab.min(pn.tabs.len().saturating_sub(1));
        pn.tab_follow = true;
        if pn.tabs.is_empty() {
            if self.split() {
                let id = self.pane.id;
                return self.close_pane(id, p);
            }
            return self.close_document(p);
        }
        if was_active {
            let uri = self.pane.tabs[self.pane.tab].uri.clone();
            self.open_uri(&uri, p);
        }
    }

    /// Closes the focused pane's document: the library (or the file picker) shows.
    fn close_document(&mut self, p: &mut dyn Platform) {
        if self.pane.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
        }
        self.save_position();
        self.pane.worker.close();
        let pn = &mut self.pane;
        pn.doc = None;
        pn.pending = None;
        pn.clear_tiles = true;
        pn.sent.clear();
        pn.wanted.clear();
        pn.search = SearchState::default();
        pn.panel = Panel::default();
        pn.anim = None;
        pn.fling = None;
        pn.shelf.open = false;
        pn.shelf.stale = true;
        self.update_dock(self.pane.vh);
        self.invalidate();
        if self.picker_home() {
            self.pick(p);
        }
    }

    /// Tab `i` of the focused pane was dropped on the other half of the window:
    /// it opens there (in a new pane if needed). It moves, unless it is the only
    /// tab of an unsplit window: then the book is shown twice.
    fn drop_tab(&mut self, i: usize, p: &mut dyn Platform) {
        let Some(tab) = self.pane.tabs.get(i).cloned() else { return };
        let keep = !self.split() && self.pane.tabs.len() == 1;
        let dst = self.new_pane();
        let title = library::title(&tab.name).to_string();
        let now = self.now;
        self.with_pane(Some(dst), |a| {
            a.pane.shelf.opening = Some((title, now));
            a.pane.overlay = true;
            a.pane.overlay_until = now + OVERLAY_TIMEOUT;
        });
        self.open_uri_in(dst, &tab.uri, p);
        if !keep {
            self.close_tab(i, p);
        }
        // The book has the focus where it went.
        if self.pane.id != dst && self.other.as_ref().is_some_and(|o| o.id == dst) {
            self.focus_other(p);
        }
    }

    // ----- touch ------------------------------------------------------------------------

    /// A touch went down at viewport point (`x`, `y`): takes it if it is on the strip.
    pub(super) fn tabs_down(&mut self, x: f32, y: f32) -> bool {
        if !self.tabs_shown() || !inside(self.tabs_rect(), x, y) {
            return false;
        }
        let pad = self.dp(4.0);
        let hit = self.pane.chips.iter().position(|(r, _)| inside([r[0], r[1] - pad, r[2], r[3] + 2.0 * pad], x, y));
        let close = hit.is_some_and(|i| {
            let c = self.pane.chips[i].1;
            inside([c[0] - pad, c[1] - pad, c[2] + 2.0 * pad, c[3] + 2.0 * pad], x, y)
        });
        self.pane.gesture = Gesture::Tabs { sx: x, sy: y, x, y, tab: hit, close, act: TabAct::Press };
        true
    }

    pub(super) fn tabs_move(&mut self, cx: f32, cy: f32) {
        let Gesture::Tabs { sx, sy, x, tab, close, act, .. } = self.pane.gesture else { return };
        let act = match act {
            TabAct::Press if (cx - sx).hypot(cy - sy) > self.dp(8.0) => {
                // Sideways along an overflowing strip scrolls it; otherwise the tab comes along.
                let along = (cx - sx).abs() > (cy - sy).abs() && self.pane.tabs_max > 0.0;
                if tab.is_some() && !close && !along {
                    self.pane.overlay_until = f64::MAX;
                    TabAct::Drag
                } else {
                    TabAct::Scroll
                }
            }
            a => a,
        };
        if act == TabAct::Scroll {
            self.pane.tabs_scroll = (self.pane.tabs_scroll - (cx - x)).clamp(0.0, self.pane.tabs_max);
        }
        self.pane.gesture = Gesture::Tabs { sx, sy, x: cx, y: cy, tab, close, act };
        self.dirty = true;
    }

    pub(super) fn tabs_up(&mut self, gesture: Gesture, p: &mut dyn Platform) {
        let Gesture::Tabs { x, y, tab: Some(i), close, act, .. } = gesture else { return };
        self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
        match act {
            TabAct::Press if close => self.close_tab(i, p),
            TabAct::Press => self.switch_tab(i, p),
            TabAct::Drag => {
                let (wx, wy) = self.drag_point(x, y);
                if self.drop_rect(wx, wy).is_some() {
                    self.drop_tab(i, p);
                }
            }
            TabAct::Scroll => {}
        }
    }

    // ----- drawing ----------------------------------------------------------------------

    /// The tab strip under the top bar (viewport coordinates), with overlay alpha `a`.
    pub(super) fn draw_tabs(&mut self, p: &mut Painter, a: f32) {
        let r = self.tabs_rect();
        p.rect(r[0], r[1], r[2], r[3], fade(ui::BAR, a), 0.0);
        p.rect(r[0], r[1] + r[3] - 1.0, r[2], 1.0, fade([255, 255, 255, 18], a), 0.0);
        let (h, gap, padx) = (self.dp(32.0), self.dp(6.0), self.dp(12.0));
        let (tpx, cs) = (self.dp(14.0), self.dp(16.0));
        let y = r[1] + (r[3] - h) / 2.0;
        let x0 = (self.pane.insets[0] - self.pane.ox).max(0.0) + self.dp(8.0);
        let right = r[2] - self.pane.insets[2] - self.dp(8.0);
        let dragged = match self.pane.gesture {
            Gesture::Tabs { tab, act: TabAct::Drag, .. } => tab,
            _ => None,
        };
        let pressed = match self.pane.gesture {
            Gesture::Tabs { tab, act: TabAct::Press, close: false, .. } => tab,
            _ => None,
        };
        // While a tab opens, its chip is already the selected one.
        let active = match &self.pane.pending {
            Some(pd) => self.pane.tabs.iter().position(|t| t.uri == pd.uri).unwrap_or(self.pane.tab),
            None => self.pane.tab,
        };
        let titles: Vec<String> = self.pane.tabs.iter().map(|t| library::title(&t.name).to_string()).collect();
        let max_label = self.dp(160.0).min((right - x0) * 0.7);
        let widths: Vec<f32> = titles.iter().map(|t| 2.0 * padx + self.text.measure(t, tpx).min(max_label) + self.dp(6.0) + cs).collect();
        let total = widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
        self.pane.tabs_max = (total - (right - x0)).max(0.0);
        if std::mem::take(&mut self.pane.tab_follow) {
            // Keep the active tab in view.
            let start: f32 = widths[..active.min(widths.len())].iter().map(|w| w + gap).sum();
            let end = start + widths.get(active).copied().unwrap_or(0.0);
            let s = &mut self.pane.tabs_scroll;
            if start < *s {
                *s = start;
            } else if end > *s + (right - x0) {
                *s = end - (right - x0);
            }
        }
        self.pane.tabs_scroll = self.pane.tabs_scroll.clamp(0.0, self.pane.tabs_max);
        self.pane.chips.clear();
        let mut x = x0 - self.pane.tabs_scroll;
        for (i, (title, &w)) in titles.iter().zip(&widths).enumerate() {
            let k = if dragged == Some(i) { a * 0.35 } else { a };
            let sel = i == active;
            let fill = if sel { [138, 180, 248, 52] } else { ui::FIELD };
            p.rect(x, y, w, h, fade(fill, k), h / 2.0);
            if pressed == Some(i) {
                p.rect(x, y, w, h, fade(ui::PRESS, k), h / 2.0);
            }
            let lw = w - 2.0 * padx - self.dp(6.0) - cs;
            let color = if sel { ui::FG } else { [200, 200, 207, 255] };
            let base = y + h / 2.0 + self.text.cap_height(tpx) / 2.0;
            // Labels stay inside the strip when it scrolls.
            let tx = x + padx;
            if tx + lw > x0 - padx && tx < right {
                self.text.draw(p, title, tx, base, tpx, fade(color, k), lw + 1.0);
            }
            let c = [x + w - padx + self.dp(4.0) - cs, y + (h - cs) / 2.0, cs, cs];
            ui::icon(p, Icon::Close, c[0] + cs / 2.0, c[1] + cs / 2.0, cs, fade(ui::DIM, k));
            self.pane.chips.push(([x, y, w, h], c));
            x += w + gap;
        }
    }

    /// The tab being dragged follows the finger, over the place it would go
    /// (window coordinates, drawn over both panes).
    pub(super) fn draw_tab_drag(&mut self, p: &mut Painter) {
        let Gesture::Tabs { x, y, tab: Some(i), act: TabAct::Drag, .. } = self.pane.gesture else { return };
        let Some(t) = self.pane.tabs.get(i) else { return };
        let title = library::title(&t.name).to_string();
        let (wx, wy) = self.drag_point(x, y);
        if let Some(r) = self.drop_rect(wx, wy) {
            let b = self.dp(3.0);
            p.rect(r[0], r[1], r[2], r[3], [138, 180, 248, 48], 0.0);
            for e in [[r[0], r[1], r[2], b], [r[0], r[1] + r[3] - b, r[2], b], [r[0], r[1], b, r[3]], [r[0] + r[2] - b, r[1], b, r[3]]] {
                p.rect(e[0], e[1], e[2], e[3], ui::ACCENT, 0.0);
            }
            let msg = "Open here";
            let px = self.dp(16.0);
            let w = self.text.measure(msg, px) + self.dp(32.0);
            let h = self.dp(40.0);
            let (bx, by) = (r[0] + (r[2] - w) / 2.0, r[1] + (r[3] - h) / 2.0);
            p.rect(bx, by, w, h, [48, 48, 54, 245], h / 2.0);
            self.text.draw(p, msg, bx + self.dp(16.0), by + h / 2.0 + self.text.cap_height(px) / 2.0, px, ui::FG, w);
        }
        let px = self.dp(14.0);
        let (h, padx) = (self.dp(36.0), self.dp(14.0));
        let lw = self.text.measure(&title, px).min(self.dp(200.0));
        let w = lw + 2.0 * padx;
        let (cx, cy) = (wx - w / 2.0, wy - h / 2.0);
        p.rect(cx - 1.0, cy - 1.0, w + 2.0, h + 2.0, [0, 0, 0, 120], h / 2.0 + 1.0);
        p.rect(cx, cy, w, h, ui::ACCENT, h / 2.0);
        self.text.draw(p, &title, cx + padx, cy + h / 2.0 + self.text.cap_height(px) / 2.0, px, [16, 24, 40, 255], lw + 1.0);
    }
}
