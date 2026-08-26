//! The cmd+p workspace switcher: a query line over a live-filtered list of
//! workspaces, matched by title, description, branch names and PR numbers;
//! up/down move a cursor, Enter jumps, Esc closes, a click on a row jumps.
//!
//! Same shape as the cmd+f search bar (`search.rs`): an egui-Context-free
//! state machine fed every input event before any TerminalView clones the
//! frame's list, so it owns the keyboard the moment it opens and every
//! transition unit-tests with bare `Event` values. Painted by hand rather
//! than built from a `TextEdit`, which nothing here would focus (no code in
//! the app calls `request_focus`); the App hands the keyboard back through
//! the pane-focus sentinel when it closes.
//!
//! cmd+p, not the customary cmd+k: that chord is the iTerm-style
//! clear-screen here and the user wanted it kept.

use egui::{
    Align2, Color32, CornerRadius, Event, FontId, ImeEvent, Key, Modifiers,
    Pos2, Rect, Sense, Vec2,
};

use crate::theme::{self, UiTheme};

/// One workspace as the switcher sees it: the fields it matches on and the
/// ones it paints. Built per frame by the App while the switcher is open;
/// `tab_index` is the raw `App.tabs` index a jump lands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tab_index: usize,
    pub title: String,
    pub description: Option<String>,
    /// The worktree's branch plus every pane's current branch, deduped.
    pub branches: Vec<String>,
    /// PR numbers the workspace has checked out.
    pub prs: Vec<u64>,
    /// The root folder's name.
    pub folder: Option<String>,
    pub archived: bool,
}

/// How good a hit is. A title hit outranks a branch/description/folder
/// hit, which outranks a hit on nothing but a PR number; ties keep the
/// caller's order (the sidebar's), which is why `filter` sorts stably.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Title,
    Meta,
    Pr,
}

/// `#12` or `12` -> "12"; anything else is not a PR token.
fn pr_token(t: &str) -> Option<&str> {
    let digits = t.strip_prefix('#').unwrap_or(t);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(digits)
}

/// Does the entry match, and how well. The query is whitespace-split into
/// tokens and *every* token must hit some field (case-insensitive
/// substring, the branch picker's rule); a digits-only token, `#` or not,
/// also matches a PR number by prefix so `#1` finds #12 and #134. The
/// entry's rank is its best token's. Empty query: everything, unranked.
fn rank(e: &Entry, query: &str) -> Option<Rank> {
    let q = query.trim();
    if q.is_empty() {
        return Some(Rank::Meta);
    }
    let title = e.title.to_lowercase();
    let metas: Vec<String> = e
        .branches
        .iter()
        .chain(e.description.iter())
        .chain(e.folder.iter())
        .map(|s| s.to_lowercase())
        .collect();
    let mut best: Option<Rank> = None;
    for token in q.split_whitespace() {
        let t = token.to_lowercase();
        let r = if title.contains(&t) {
            Rank::Title
        } else if metas.iter().any(|m| m.contains(&t)) {
            Rank::Meta
        } else if pr_token(&t).is_some_and(|d| {
            e.prs.iter().any(|n| n.to_string().starts_with(d))
        }) {
            Rank::Pr
        } else {
            return None;
        };
        best = Some(best.map_or(r, |b| b.min(r)));
    }
    best
}

/// The entries the query keeps, best hits first, ties in the given order.
pub fn filter(entries: Vec<Entry>, query: &str) -> Vec<Entry> {
    let mut hits: Vec<(Rank, Entry)> = entries
        .into_iter()
        .filter_map(|e| rank(&e, query).map(|r| (r, e)))
        .collect();
    // Stable: equal ranks keep the sidebar's order.
    hits.sort_by_key(|(r, _)| *r);
    hits.into_iter().map(|(_, e)| e).collect()
}

/// What the app must do with the event that was just fed in.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Leave the event in the frame for TerminalView.
    Pass,
    /// Remove the event from the frame.
    Consume,
    Op(Op),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    /// Escape: the machine has already closed; the App has nothing to undo.
    Close,
    /// Enter: jump to the row under the cursor. Carries no index on
    /// purpose - the App resolves the cursor against the rows it builds in
    /// the *same* frame, so a "type + Enter" batch lands on the new
    /// query's first match, not last frame's.
    Jump,
}

#[derive(Debug, Default)]
pub struct Switcher {
    open: bool,
    query: String,
    cursor: usize,
    /// How many rows the current query yields, as last told by `sync`.
    /// Arrows wrap on it and Enter is inert at zero.
    len: usize,
}

impl Switcher {
    pub fn active(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.len = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Tell the machine how many rows the query yields now, clamping the
    /// cursor into range. Called wherever the App rebuilds the rows - before
    /// resolving an Enter and before painting - so a tab that vanished
    /// between frames can't leave the cursor pointing past the end.
    pub fn sync(&mut self, len: usize) {
        self.len = len;
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    fn step(&mut self, delta: isize) {
        if self.len == 0 {
            self.cursor = 0;
            return;
        }
        let len = self.len as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
    }

    fn edit(&mut self, f: impl FnOnce(&mut String)) -> Verdict {
        f(&mut self.query);
        // A new query is a new list; start from its best hit.
        self.cursor = 0;
        Verdict::Consume
    }

    /// Drive one event through the machine.
    pub fn on_event(&mut self, event: &Event) -> Verdict {
        if !self.open {
            return Verdict::Pass;
        }
        match event {
            Event::Text(t) | Event::Ime(ImeEvent::Commit(t)) => {
                self.edit(|q| q.extend(t.chars().filter(|c| !c.is_control())))
            },
            Event::Paste(t) => self.edit(|q| {
                q.extend(
                    t.chars().map(|c| if c.is_control() { ' ' } else { c }),
                )
            }),
            Event::Ime(_) => Verdict::Consume,
            Event::Key {
                key: Key::Backspace,
                pressed: true,
                ..
            } => {
                // Erasing past empty keeps the switcher open; Escape is the
                // explicit close.
                self.edit(|q| {
                    q.pop();
                })
            },
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_down(*key, *modifiers) => {
                self.step(1);
                Verdict::Consume
            },
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_up(*key, *modifiers) => {
                self.step(-1);
                Verdict::Consume
            },
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            } => {
                if self.len > 0 {
                    Verdict::Op(Op::Jump)
                } else {
                    Verdict::Consume
                }
            },
            Event::Key {
                key: Key::Escape,
                pressed: true,
                ..
            } => {
                self.close();
                Verdict::Op(Op::Close)
            },
            // The switcher owns the keyboard; nothing else may leak to the
            // PTY. Copy/Cut stay live, as with the search bar.
            Event::Key { .. } => Verdict::Consume,
            _ => Verdict::Pass,
        }
    }
}

/// Down: the arrow, Tab, or the emacs/readline ctrl+n.
fn is_down(key: Key, m: Modifiers) -> bool {
    key == Key::ArrowDown
        || (key == Key::Tab && !m.shift)
        || (key == Key::N && m.ctrl)
}

/// Up: the arrow, shift+Tab, or ctrl+p.
fn is_up(key: Key, m: Modifiers) -> bool {
    key == Key::ArrowUp
        || (key == Key::Tab && m.shift)
        || (key == Key::P && m.ctrl)
}

/// Row slots the overlay always reserves. Fixed, like the branch picker's,
/// so typing can't resize the anchored window under the pointer; longer
/// lists scroll behind the cursor (`first_visible`).
pub const MAX_ROWS: usize = 8;

/// The first row painted, so the cursor is always on screen: the window
/// slides only once the cursor walks past the last slot.
fn first_visible(cursor: usize, len: usize) -> usize {
    cursor
        .saturating_sub(MAX_ROWS - 1)
        .min(len.saturating_sub(MAX_ROWS))
}

/// What the overlay wants the App to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// A click landed outside the window (on the sidebar, a pane): the
    /// switcher gets out of the way; the click keeps its own effect.
    Close,
    /// A row was clicked: index into the rows passed in.
    Jump(usize),
}

/// The subtitle: branches, then `#pr`s, then the description, dot-joined.
fn subtitle(e: &Entry) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(e.branches.iter().cloned());
    parts.extend(e.prs.iter().map(|n| format!("#{n}")));
    if let Some(d) = &e.description {
        parts.push(d.clone());
    }
    if parts.is_empty() {
        if let Some(f) = &e.folder {
            parts.push(f.clone());
        }
    }
    parts.join(" · ")
}

pub fn show(
    ctx: &egui::Context,
    sw: &Switcher,
    rows: &[Entry],
    font: &FontId,
    t: &UiTheme,
) -> Outcome {
    let mut outcome = Outcome::None;
    let screen = ctx.screen_rect();
    let width = (screen.width() * 0.6).clamp(240.0, 560.0);
    let sub_font = FontId::new(font.size * 0.8, font.family.clone());
    let (line_h, sub_h, char_w) = ctx.fonts(|f| {
        (
            f.row_height(font),
            f.row_height(&sub_font),
            f.glyph_width(font, 'M').max(1.0),
        )
    });
    let query_pad = Vec2::new(8.0, 6.0);
    let query_h = line_h + query_pad.y * 2.0;
    let row_pad = Vec2::new(8.0, 4.0);
    let row_h = line_h + sub_h + row_pad.y * 2.0;
    let row_gap = 2.0;
    let gap = 8.0;
    let note_h = sub_h + 4.0;
    // The whole thing is fixed-size: one query line, MAX_ROWS slots (blank
    // when the list is shorter), one note line.
    let height = query_h
        + gap
        + MAX_ROWS as f32 * (row_h + row_gap)
        + gap
        + note_h;

    let cursor = sw.cursor();
    let first = first_visible(cursor, rows.len());

    let win = egui::Window::new("switcher")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 80.0))
        .fixed_size(Vec2::new(width, height))
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .inner_margin(egui::Margin::same(14))
                .stroke(egui::Stroke::new(1.0_f32, theme::blend(t.bg, t.text, 0.18)))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();

            // The query line: an accent `›`, the text, a caret - or a dim
            // hint while empty. Tail-truncated like the search bar's field,
            // since the caret sits at the end.
            let (qrect, _) = ui.allocate_exact_size(
                Vec2::new(w, query_h),
                Sense::hover(),
            );
            let painter = ui.painter();
            painter.rect_filled(
                qrect,
                CornerRadius::same(4),
                theme::blend(t.bg, t.accent, 0.10),
            );
            let mid = qrect.center().y;
            let prefix =
                painter.layout_no_wrap("› ".into(), font.clone(), t.accent);
            let text_left = qrect.min.x + query_pad.x + prefix.size().x;
            painter.galley(
                Pos2::new(qrect.min.x + query_pad.x, mid - prefix.size().y / 2.0),
                prefix,
                t.accent,
            );
            let query = sw.query();
            if query.is_empty() {
                let hint = painter.layout_no_wrap(
                    "title, description, branch, #pr…".into(),
                    font.clone(),
                    t.text_dim,
                );
                painter.galley(
                    Pos2::new(text_left, mid - hint.size().y / 2.0),
                    hint,
                    t.text_dim,
                );
            } else {
                let avail = (qrect.max.x - query_pad.x - text_left).max(0.0);
                let budget = ((avail / char_w) as usize).saturating_sub(1);
                let chars = query.chars().count();
                let visible: String = if chars > budget {
                    query.chars().skip(chars - budget).collect()
                } else {
                    query.to_string()
                };
                let text = painter.layout_no_wrap(visible, font.clone(), t.text);
                let caret_x = text_left + text.size().x;
                painter.galley(
                    Pos2::new(text_left, mid - text.size().y / 2.0),
                    text,
                    t.text,
                );
                painter.rect_filled(
                    Rect::from_min_size(
                        Pos2::new(caret_x + 1.0, mid - line_h / 2.0),
                        Vec2::new(2.0, line_h),
                    ),
                    CornerRadius::ZERO,
                    t.accent,
                );
            }
            ui.add_space(gap);

            // The rows: allocated, not overlaid, so each owns its clicks
            // (and headless clicks reach them). Blank filler slots keep
            // the height.
            for slot in 0..MAX_ROWS {
                let (rect, resp) = ui.allocate_exact_size(
                    Vec2::new(w, row_h),
                    Sense::click(),
                );
                ui.add_space(row_gap);
                let Some(e) = rows.get(first + slot) else {
                    continue;
                };
                let selected = first + slot == cursor;
                let painter = ui.painter().with_clip_rect(rect);
                if selected {
                    painter.rect_filled(
                        rect,
                        CornerRadius::same(4),
                        theme::blend(t.bg, t.accent, 0.14),
                    );
                } else if resp.hovered() {
                    painter.rect_filled(
                        rect,
                        CornerRadius::same(4),
                        theme::blend(t.bg, t.accent, 0.06),
                    );
                }
                let title_color = if selected { t.text } else { t.text_dim };
                let title = painter.layout_no_wrap(
                    e.title.clone(),
                    font.clone(),
                    title_color,
                );
                let title_pos = rect.min + row_pad;
                painter.galley(title_pos, title.clone(), title_color);
                if e.archived {
                    let tag = painter.layout_no_wrap(
                        " · archived".into(),
                        sub_font.clone(),
                        t.text_dim,
                    );
                    painter.galley(
                        Pos2::new(
                            title_pos.x + title.size().x,
                            title_pos.y + (line_h - sub_h) / 2.0,
                        ),
                        tag,
                        t.text_dim,
                    );
                }
                let sub = subtitle(e);
                if !sub.is_empty() {
                    let sub = painter.layout_no_wrap(
                        sub,
                        sub_font.clone(),
                        t.text_dim,
                    );
                    painter.galley(
                        Pos2::new(title_pos.x + char_w, title_pos.y + line_h),
                        sub,
                        t.text_dim,
                    );
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                {
                    outcome = Outcome::Jump(first + slot);
                }
            }
            ui.add_space(gap);

            // The note line: why the list is empty, where the window is in
            // a long list, else the key hint.
            let note = if rows.is_empty() && !query.trim().is_empty() {
                "no matches".to_string()
            } else if rows.len() > MAX_ROWS {
                format!(
                    "{}–{} of {} · ↑↓ move · ⏎ jump · esc close",
                    first + 1,
                    (first + MAX_ROWS).min(rows.len()),
                    rows.len()
                )
            } else {
                "↑↓ move · ⏎ jump · esc close".to_string()
            };
            let (nrect, _) = ui.allocate_exact_size(
                Vec2::new(w, note_h),
                Sense::hover(),
            );
            ui.painter().text(
                Pos2::new(nrect.min.x + row_pad.x, nrect.center().y),
                Align2::LEFT_CENTER,
                note,
                sub_font.clone(),
                t.text_dim,
            );
        });

    // A press outside the window: the sidebar and the panes aren't
    // covered, and a click there must not leave the switcher up.
    if outcome == Outcome::None {
        let win_rect = win.map(|w| w.response.rect);
        let outside = ctx.input(|i| {
            i.pointer.primary_pressed()
                && i.pointer.interact_pos().is_some_and(|p| {
                    !win_rect.is_some_and(|r| r.contains(p))
                })
        });
        if outside {
            outcome = Outcome::Close;
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(i: usize, title: &str) -> Entry {
        Entry {
            tab_index: i,
            title: title.into(),
            description: None,
            branches: Vec::new(),
            prs: Vec::new(),
            folder: None,
            archived: false,
        }
    }

    fn titles(v: &[Entry]) -> Vec<&str> {
        v.iter().map(|e| e.title.as_str()).collect()
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry {
                branches: vec!["feat/parser".into()],
                prs: vec![12],
                description: Some("Fix the tokenizer".into()),
                folder: Some("muxterm".into()),
                ..entry(0, "Parser fixes")
            },
            Entry {
                branches: vec!["main".into()],
                prs: vec![134],
                ..entry(1, "Docs sweep")
            },
            Entry {
                folder: Some("tinyverse".into()),
                archived: true,
                ..entry(2, "React experimental")
            },
            Entry {
                branches: vec!["snappy-dove".into()],
                description: Some("parser benchmarks".into()),
                ..entry(3, "Bench")
            },
        ]
    }

    /// Every token must hit somewhere; case doesn't matter; the field
    /// that hit decides the order - title first, then branch/description/
    /// folder, then a bare PR number - with ties in the given order.
    #[test]
    fn filter_ranks_title_over_meta_over_pr() {
        let hits = filter(sample(), "PARSER");
        // "Parser fixes" by title; "Bench" only by its description.
        assert_eq!(titles(&hits), vec!["Parser fixes", "Bench"]);

        let hits = filter(sample(), "parser bench");
        assert_eq!(titles(&hits), vec!["Bench"], "both tokens must hit");

        let hits = filter(sample(), "tinyverse");
        assert_eq!(titles(&hits), vec!["React experimental"], "folder counts");

        let hits = filter(sample(), "feat/");
        assert_eq!(titles(&hits), vec!["Parser fixes"], "branch counts");

        assert!(filter(sample(), "zzz").is_empty());
    }

    /// `#12`, `12` and a prefix `1` all find PRs by number, and a title
    /// that spells the number (a PR checkout's "#12 …") still ranks first.
    #[test]
    fn pr_numbers_match_by_prefix_with_or_without_hash() {
        assert_eq!(titles(&filter(sample(), "#12")), vec!["Parser fixes"]);
        assert_eq!(titles(&filter(sample(), "12")), vec!["Parser fixes"]);
        assert_eq!(
            titles(&filter(sample(), "1")),
            vec!["Parser fixes", "Docs sweep"],
            "a digit prefix finds #12 and #134, in order",
        );
        let mut s = sample();
        s.push(Entry { ..entry(4, "#134 hotfix") });
        assert_eq!(
            titles(&filter(s, "134")),
            vec!["#134 hotfix", "Docs sweep"],
            "the title hit outranks the number-only hit",
        );
        assert_eq!(pr_token("#7"), Some("7"));
        assert_eq!(pr_token("7"), Some("7"));
        assert_eq!(pr_token("#"), None);
        assert_eq!(pr_token("v7"), None);
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        let hits = filter(sample(), "   ");
        assert_eq!(
            titles(&hits),
            vec!["Parser fixes", "Docs sweep", "React experimental", "Bench"]
        );
    }

    fn key(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn ctrl_key(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }
    }

    fn text(s: &str) -> Event {
        Event::Text(s.into())
    }

    /// Typing edits the query and resets the cursor; arrows wrap on the
    /// row count the App last synced; Enter jumps only when there is a
    /// row; Escape closes; other keys are swallowed, Copy passes.
    #[test]
    fn machine_owns_the_keyboard_while_open() {
        let mut sw = Switcher::default();
        assert_eq!(sw.on_event(&text("x")), Verdict::Pass, "closed: inert");

        sw.open();
        assert!(sw.active());
        assert_eq!(sw.on_event(&key(Key::Enter)), Verdict::Consume, "no rows yet");
        assert_eq!(sw.on_event(&text("pa")), Verdict::Consume);
        assert_eq!(sw.on_event(&text("r\u{7}")), Verdict::Consume);
        assert_eq!(sw.query(), "par", "control chars dropped");
        assert_eq!(sw.on_event(&Event::Paste("s\ner".into())), Verdict::Consume);
        assert_eq!(sw.query(), "pars er", "pasted control chars become spaces");
        assert_eq!(sw.on_event(&key(Key::Backspace)), Verdict::Consume);
        assert_eq!(sw.query(), "pars e");

        sw.sync(3);
        assert_eq!(sw.on_event(&key(Key::ArrowDown)), Verdict::Consume);
        assert_eq!(sw.on_event(&ctrl_key(Key::N)), Verdict::Consume);
        assert_eq!(sw.cursor(), 2);
        assert_eq!(sw.on_event(&key(Key::Tab)), Verdict::Consume);
        assert_eq!(sw.cursor(), 0, "down wraps");
        assert_eq!(sw.on_event(&key(Key::ArrowUp)), Verdict::Consume);
        assert_eq!(sw.cursor(), 2, "up wraps");
        assert_eq!(sw.on_event(&ctrl_key(Key::P)), Verdict::Consume);
        assert_eq!(sw.cursor(), 1);
        assert_eq!(sw.on_event(&text("x")), Verdict::Consume);
        assert_eq!(sw.cursor(), 0, "an edit starts from the best hit again");

        sw.sync(3);
        sw.on_event(&key(Key::ArrowDown));
        sw.on_event(&key(Key::ArrowDown));
        sw.sync(1);
        assert_eq!(sw.cursor(), 0, "sync clamps a stale cursor");
        assert_eq!(sw.on_event(&key(Key::Enter)), Verdict::Op(Op::Jump));
        assert!(sw.active(), "the App closes after a jump, not the machine");

        assert_eq!(sw.on_event(&key(Key::A)), Verdict::Consume, "unknown keys never reach the PTY");
        assert_eq!(sw.on_event(&Event::Copy), Verdict::Pass);
        assert_eq!(sw.on_event(&key(Key::Escape)), Verdict::Op(Op::Close));
        assert!(!sw.active());
        assert_eq!(sw.on_event(&text("x")), Verdict::Pass);
    }

    #[test]
    fn the_window_slides_only_past_the_last_slot() {
        assert_eq!(first_visible(0, 20), 0);
        assert_eq!(first_visible(MAX_ROWS - 1, 20), 0);
        assert_eq!(first_visible(MAX_ROWS, 20), 1);
        assert_eq!(first_visible(19, 20), 20 - MAX_ROWS);
        assert_eq!(first_visible(3, 4), 0, "short lists never slide");
    }

    fn theme() -> UiTheme {
        let preset = theme::preset("iterm-dark").unwrap();
        theme::build(preset, &HashMap::new(), 0.12).1
    }

    fn base_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        }
    }

    fn texts(out: &egui::FullOutput) -> Vec<(String, Pos2)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, Pos2)>) {
            match shape {
                egui::Shape::Text(t) => {
                    out.push((t.galley.text().to_string(), t.pos))
                },
                egui::Shape::Vec(v) => {
                    for s in v {
                        walk(s, out);
                    }
                },
                _ => {},
            }
        }
        let mut v = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, &mut v);
        }
        v
    }

    /// The overlay paints the query, every matched title and subtitle,
    /// nothing for filtered-out entries, and keeps one height whatever
    /// the list length (blank slots), while a long list scrolls so the
    /// cursor's row is on screen.
    #[test]
    fn overlay_paints_matches_at_a_fixed_height() {
        let ctx = egui::Context::default();
        let th = theme();
        let font = FontId::monospace(14.0);
        let mut sw = Switcher::default();
        sw.open();
        for c in "par".chars() {
            sw.on_event(&text(&c.to_string()));
        }
        let rows = filter(sample(), sw.query());
        sw.sync(rows.len());

        let render = |sw: &Switcher, rows: &[Entry]| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, sw, rows, &font, &th);
            };
            // First frame sizes the window invisibly; the second paints.
            let _ = ctx.run(base_input(), &mut frame);
            ctx.run(base_input(), &mut frame)
        };
        let out = render(&sw, &rows);
        let painted: Vec<String> = texts(&out).into_iter().map(|(s, _)| s).collect();
        let joined = painted.join("\n");
        assert!(joined.contains("par"), "query painted: {painted:?}");
        assert!(joined.contains("Parser fixes"), "{painted:?}");
        assert!(joined.contains("feat/parser · #12 · Fix the tokenizer"), "{painted:?}");
        assert!(joined.contains("Bench"), "{painted:?}");
        assert!(!joined.contains("Docs sweep"), "filtered out: {painted:?}");
        assert!(!joined.contains("React"), "filtered out: {painted:?}");

        // Height: the same for 2 rows and for the full list.
        let win_h = |out: &egui::FullOutput| {
            out.shapes
                .iter()
                .map(|c| c.clip_rect.height())
                .fold(0.0_f32, f32::max)
        };
        let two = win_h(&out);
        let mut all = Switcher::default();
        all.open();
        let rows = filter(sample(), "");
        all.sync(rows.len());
        let out = render(&all, &rows);
        assert_eq!(two, win_h(&out), "blank slots keep the window height");
        let joined: String = texts(&out).into_iter().map(|(s, _)| s).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("React experimental"));
        assert!(joined.contains(" · archived"), "archived rows say so: {joined}");

        // A long list: with the cursor on the last row, that row paints
        // and the first one has scrolled off.
        let long: Vec<Entry> = (0..20).map(|i| entry(i, &format!("ws-{i:02}"))).collect();
        let mut sw = Switcher::default();
        sw.open();
        sw.sync(long.len());
        for _ in 0..19 {
            sw.on_event(&key(Key::ArrowDown));
        }
        let out = render(&sw, &long);
        let joined: String = texts(&out).into_iter().map(|(s, _)| s).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("ws-19"), "{joined}");
        assert!(!joined.contains("ws-00"), "{joined}");
        assert!(joined.contains("13–20 of 20"), "{joined}");
    }

    /// Rows are allocated widgets, so a click on one is a jump (index into
    /// the rows given), and a press outside the window closes it.
    #[test]
    fn a_click_on_a_row_jumps_and_outside_closes() {
        let ctx = egui::Context::default();
        let th = theme();
        let font = FontId::monospace(14.0);
        let mut sw = Switcher::default();
        sw.open();
        let rows = filter(sample(), "");
        sw.sync(rows.len());

        // A RefCell so the frame closure can keep running after each
        // assertion reads the outcome it wrote.
        let outcome = std::cell::RefCell::new(Outcome::None);
        let mut frame = |ctx: &egui::Context| {
            *outcome.borrow_mut() = show(ctx, &sw, &rows, &font, &th);
        };
        let _ = ctx.run(base_input(), &mut frame);
        let out = ctx.run(base_input(), &mut frame);
        let pos = texts(&out)
            .into_iter()
            .find(|(s, _)| s == "Docs sweep")
            .map(|(_, p)| p + Vec2::new(10.0, 6.0))
            .expect("second row painted");

        let press = |pos| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        let release = |pos| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        };
        let at = |events: Vec<Event>| egui::RawInput { events, ..base_input() };
        let _ = ctx.run(at(vec![Event::PointerMoved(pos), press(pos)]), &mut frame);
        let _ = ctx.run(at(vec![release(pos)]), &mut frame);
        assert_eq!(*outcome.borrow(), Outcome::Jump(1));

        // Off in the corner, well outside the window.
        let away = Pos2::new(20.0, 680.0);
        let _ = ctx.run(at(vec![Event::PointerMoved(away)]), &mut frame);
        let _ = ctx.run(at(vec![press(away)]), &mut frame);
        assert_eq!(*outcome.borrow(), Outcome::Close);
    }
}
