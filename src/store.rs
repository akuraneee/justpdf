//! Remembers the reading position per document URI in a small text file.

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

pub struct Store {
    path: Option<PathBuf>,
    settings: Option<PathBuf>,
    spread: bool,
    caffeine: bool,
    pin_chapters: bool,
    /// Most recent first.
    entries: Vec<(String, Position)>,
    dirty: bool,
}

impl Store {
    pub fn load(dir: Option<PathBuf>) -> Store {
        let settings = dir.as_ref().map(|d| d.join("settings.txt"));
        let text = settings.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let flag = |key: &str| text.lines().filter_map(|l| l.trim().split_once('=')).any(|(k, v)| k == key && v == "1");
        let (spread, caffeine, pin_chapters) = (flag("spread"), flag("caffeine"), flag("pin_chapters"));
        let path = dir.map(|d| d.join("positions.tsv"));
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().filter_map(parse_line).collect())
            .unwrap_or_default();
        Store { path, settings, spread, caffeine, pin_chapters, entries, dirty: false }
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

    fn save_settings(&self) {
        if let Some(p) = &self.settings {
            let s = format!("spread={}\ncaffeine={}\npin_chapters={}\n", self.spread as u8, self.caffeine as u8, self.pin_chapters as u8);
            let _ = std::fs::write(p, s);
        }
    }

    pub fn get(&self, uri: &str) -> Option<Position> {
        self.entries.iter().find(|(u, _)| u == uri).map(|(_, p)| *p)
    }

    pub fn set(&mut self, uri: &str, pos: Position) {
        if self.entries.first().is_some_and(|(u, p)| u == uri && *p == pos) {
            return;
        }
        self.entries.retain(|(u, _)| u != uri);
        self.entries.insert(0, (uri.to_string(), pos));
        self.entries.truncate(MAX_ENTRIES);
        self.dirty = true;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        for (uri, p) in &self.entries {
            out.push_str(&format!("{}\t{:.6}\t{:.4}\t{:.6}\t{}\n", p.page, p.frac, p.zoom, p.x, uri));
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, path)).is_ok() {
            self.dirty = false;
        }
    }
}

fn parse_line(line: &str) -> Option<(String, Position)> {
    let mut it = line.splitn(5, '\t');
    let page = it.next()?.parse().ok()?;
    let frac: f64 = it.next()?.parse().ok()?;
    let zoom: f64 = it.next()?.parse().ok()?;
    let x: f64 = it.next()?.parse().ok()?;
    let uri = it.next()?.to_string();
    (frac.is_finite() && zoom.is_finite() && x.is_finite() && !uri.is_empty()).then_some((uri, Position { page, frac, zoom, x }))
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
        assert_eq!(s2.entries[0].0, "content://other");
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
        std::fs::remove_dir_all(dir).unwrap();
    }
}
