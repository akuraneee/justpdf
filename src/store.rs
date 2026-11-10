//! Remembers the reading position (plus name and page count, for the library's
//! recent list) per document URI in a small text file, and the settings.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub page: u32,
    /// Offset of the viewport top inside `page`, as a fraction of its height.
    pub frac: f64,
    pub zoom: f64,
    /// Horizontal scroll as a fraction of the page width.
    pub x: f64,
}

const MAX_ENTRIES: usize = 500;

/// A document opened before, most recent first in `Store::recent`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub uri: String,
    pub pos: Position,
    /// Display name ("" if unknown: written by an older version).
    pub name: String,
    /// Page count (0 if unknown).
    pub pages: u32,
}

pub struct Store {
    path: Option<PathBuf>,
    settings: Option<PathBuf>,
    spread: bool,
    caffeine: bool,
    pin_chapters: bool,
    /// Docked contents panel width in dp; 0 = automatic.
    chapters_w: u32,
    /// Library tab last shown: "" = recent, else a folder's tree URI.
    shelf: String,
    /// Most recent first.
    entries: Vec<Entry>,
    dirty: bool,
}

impl Store {
    pub fn load(dir: Option<PathBuf>) -> Store {
        let settings = dir.as_ref().map(|d| d.join("settings.txt"));
        let text = settings.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let flag = |key: &str| text.lines().filter_map(|l| l.trim().split_once('=')).any(|(k, v)| k == key && v == "1");
        let (spread, caffeine, pin_chapters) = (flag("spread"), flag("caffeine"), flag("pin_chapters"));
        let value = |key: &str| text.lines().filter_map(|l| l.trim().split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v);
        let chapters_w = value("chapters_w").and_then(|v| v.parse().ok()).unwrap_or(0);
        let shelf = value("shelf").unwrap_or("").to_string();
        let path = dir.map(|d| d.join("positions.tsv"));
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().filter_map(parse_line).collect())
            .unwrap_or_default();
        Store { path, settings, spread, caffeine, pin_chapters, chapters_w, shelf, entries, dirty: false }
    }

    /// Two-page spread mode (a global preference).
    pub fn spread(&self) -> bool {
        self.spread
    }

    pub fn set_spread(&mut self, on: bool) {
        self.spread = on;
        self.save_settings();
    }

    /// Keep the screen on while reading (a global preference).
    pub fn caffeine(&self) -> bool {
        self.caffeine
    }

    pub fn set_caffeine(&mut self, on: bool) {
        self.caffeine = on;
        self.save_settings();
    }

    /// Keep the contents panel docked at the side (a global preference).
    pub fn pin_chapters(&self) -> bool {
        self.pin_chapters
    }

    pub fn set_pin_chapters(&mut self, on: bool) {
        self.pin_chapters = on;
        self.save_settings();
    }

    /// Width of the docked contents panel in dp (0: automatic).
    pub fn chapters_w(&self) -> u32 {
        self.chapters_w
    }

    pub fn set_chapters_w(&mut self, dp: u32) {
        self.chapters_w = dp;
        self.save_settings();
    }

    /// Library tab last shown: "" = recent, else a folder's tree URI.
    pub fn shelf(&self) -> &str {
        &self.shelf
    }

    pub fn set_shelf(&mut self, tab: &str) {
        if self.shelf != tab {
            self.shelf = tab.replace(['\n', '\r'], "");
            self.save_settings();
        }
    }

    fn save_settings(&self) {
        if let Some(p) = &self.settings {
            let s = format!(
                "spread={}\ncaffeine={}\npin_chapters={}\nchapters_w={}\nshelf={}\n",
                self.spread as u8, self.caffeine as u8, self.pin_chapters as u8, self.chapters_w, self.shelf
            );
            let _ = std::fs::write(p, s);
        }
    }

    pub fn get(&self, uri: &str) -> Option<Position> {
        self.entry(uri).map(|e| e.pos)
    }

    pub fn entry(&self, uri: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.uri == uri)
    }

    /// Documents opened before, most recent first.
    pub fn recent(&self) -> &[Entry] {
        &self.entries
    }

    /// Moves `uri` to the front, creating it if new; returns its entry.
    fn front(&mut self, uri: &str) -> &mut Entry {
        match self.entries.iter().position(|e| e.uri == uri) {
            Some(0) => {}
            Some(i) => {
                let e = self.entries.remove(i);
                self.entries.insert(0, e);
            }
            None => {
                let pos = Position { page: 0, frac: 0.0, zoom: 1.0, x: 0.0 };
                self.entries.insert(0, Entry { uri: uri.to_string(), pos, name: String::new(), pages: 0 });
                self.entries.truncate(MAX_ENTRIES);
            }
        }
        self.dirty = true;
        &mut self.entries[0]
    }

    pub fn set(&mut self, uri: &str, pos: Position) {
        if self.entries.first().is_some_and(|e| e.uri == uri && e.pos == pos) {
            return;
        }
        self.front(uri).pos = pos;
    }

    /// Records that `uri` was just opened: it becomes the most recent document.
    pub fn opened(&mut self, uri: &str, name: &str, pages: u32) {
        let e = self.front(uri);
        e.name = name.replace(['\t', '\n', '\r'], " ");
        e.pages = pages;
    }

    /// Forgets a document (it is no longer listed as recent).
    pub fn remove(&mut self, uri: &str) {
        let n = self.entries.len();
        self.entries.retain(|e| e.uri != uri);
        self.dirty |= self.entries.len() != n;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        for e in &self.entries {
            let p = e.pos;
            out.push_str(&format!("{}\t{:.6}\t{:.4}\t{:.6}\t{}\t{}\t{}\n", p.page, p.frac, p.zoom, p.x, e.uri, e.name, e.pages));
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, path)).is_ok() {
            self.dirty = false;
        }
    }
}

/// `page frac zoom x uri [name pages]` (older versions wrote no name/pages).
fn parse_line(line: &str) -> Option<Entry> {
    let mut it = line.split('\t');
    let page = it.next()?.parse().ok()?;
    let frac: f64 = it.next()?.parse().ok()?;
    let zoom: f64 = it.next()?.parse().ok()?;
    let x: f64 = it.next()?.parse().ok()?;
    let uri = it.next()?.to_string();
    let name = it.next().unwrap_or("").to_string();
    let pages = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (frac.is_finite() && zoom.is_finite() && x.is_finite() && !uri.is_empty()).then_some(Entry {
        uri,
        pos: Position { page, frac, zoom, x },
        name,
        pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("justpdf-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Store::load(Some(dir.clone()));
        let p = Position { page: 3, frac: 0.25, zoom: 1.5, x: 0.1 };
        s.set("content://a/b%20c", p);
        s.set("content://other", Position { page: 0, frac: 0.0, zoom: 1.0, x: 0.0 });
        s.save();
        let s2 = Store::load(Some(dir.clone()));
        assert_eq!(s2.get("content://a/b%20c"), Some(p));
        assert_eq!(s2.entries[0].uri, "content://other");
        assert!(!s2.spread());
        let mut s3 = Store::load(Some(dir.clone()));
        s3.set_spread(true);
        assert!(Store::load(Some(dir.clone())).spread());
        assert!(!Store::load(Some(dir.clone())).caffeine());
        s3.set_caffeine(true);
        let s4 = Store::load(Some(dir.clone()));
        assert!(s4.spread() && s4.caffeine());
        s3.set_spread(false);
        let s5 = Store::load(Some(dir.clone()));
        assert!(!s5.spread() && s5.caffeine());
        assert!(!s5.pin_chapters());
        s3.set_pin_chapters(true);
        let s6 = Store::load(Some(dir.clone()));
        assert!(s6.pin_chapters() && s6.caffeine() && !s6.spread());
        assert_eq!(s6.chapters_w(), 0);
        s3.set_chapters_w(287);
        let s7 = Store::load(Some(dir.clone()));
        assert_eq!(s7.chapters_w(), 287);
        assert!(s7.pin_chapters());
        assert_eq!(s7.shelf(), "");
        s3.set_shelf("content://tree/x");
        assert_eq!(Store::load(Some(dir.clone())).shelf(), "content://tree/x");

        // Name and page count for the library; old 5-column lines still load.
        let mut s8 = Store::load(Some(dir.clone()));
        s8.opened("content://a/b%20c", "B c.pdf", 12);
        s8.opened("content://new", "New\tone.pdf", 3);
        s8.save();
        let s9 = Store::load(Some(dir.clone()));
        let r = s9.recent();
        assert_eq!((r[0].uri.as_str(), r[0].name.as_str(), r[0].pages), ("content://new", "New one.pdf", 3));
        assert_eq!((r[1].name.as_str(), r[1].pages, r[1].pos), ("B c.pdf", 12, p));
        assert_eq!(r[2].uri, "content://other");
        std::fs::write(dir.join("positions.tsv"), "2\t0.5\t1.0\t0.0\tcontent://old\n").unwrap();
        let s10 = Store::load(Some(dir.clone()));
        assert_eq!((s10.recent()[0].name.as_str(), s10.recent()[0].pages, s10.recent()[0].pos.page), ("", 0, 2));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
