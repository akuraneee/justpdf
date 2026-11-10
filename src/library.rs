//! The library: folders the user added (the platform scans them for PDFs) and
//! the grouping/sorting of their books. Persisted in `files/folders.tsv`.

use std::cmp::Ordering;
use std::ops::Range;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Book {
    pub uri: String,
    /// File name (with extension).
    pub name: String,
    /// Path of the containing folder relative to the library folder ("" = its root),
    /// components separated by '/'.
    pub dir: String,
}

#[derive(Clone, Debug)]
pub struct Folder {
    /// Tree URI with a persisted read permission.
    pub uri: String,
    pub name: String,
    /// Sorted by folder, then name (see `sort`).
    pub books: Vec<Book>,
}

pub struct Library {
    path: Option<PathBuf>,
    pub folders: Vec<Folder>,
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

impl Library {
    pub fn load(dir: Option<PathBuf>) -> Library {
        let path = dir.map(|d| d.join("folders.tsv"));
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let mut folders: Vec<Folder> = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["F", uri, name] if !uri.is_empty() => {
                    folders.push(Folder { uri: uri.to_string(), name: name.to_string(), books: Vec::new() })
                }
                ["B", dir, name, uri] if !uri.is_empty() => {
                    if let Some(folder) = folders.last_mut() {
                        folder.books.push(Book { uri: uri.to_string(), name: name.to_string(), dir: dir.to_string() });
                    }
                }
                _ => {}
            }
        }
        for f in &mut folders {
            sort(&mut f.books);
        }
        Library { path, folders }
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        for f in &self.folders {
            out.push_str(&format!("F\t{}\t{}\n", clean(&f.uri), clean(&f.name)));
            for b in &f.books {
                out.push_str(&format!("B\t{}\t{}\t{}\n", clean(&b.dir), clean(&b.name), clean(&b.uri)));
            }
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn find(&self, uri: &str) -> Option<usize> {
        self.folders.iter().position(|f| f.uri == uri)
    }

    /// Adds a folder (or renames it if it is already there); returns its index.
    pub fn add(&mut self, uri: &str, name: &str) -> usize {
        let name = if name.trim().is_empty() { "Folder" } else { name.trim() };
        let i = match self.find(uri) {
            Some(i) => {
                self.folders[i].name = name.to_string();
                i
            }
            None => {
                self.folders.push(Folder { uri: uri.to_string(), name: name.to_string(), books: Vec::new() });
                self.folders.len() - 1
            }
        };
        self.save();
        i
    }

    pub fn remove(&mut self, i: usize) -> Option<Folder> {
        if i >= self.folders.len() {
            return None;
        }
        let f = self.folders.remove(i);
        self.save();
        Some(f)
    }

    /// Replaces the books of folder `uri` with a fresh scan.
    pub fn set_books(&mut self, uri: &str, mut books: Vec<Book>) -> Option<usize> {
        let i = self.find(uri)?;
        sort(&mut books);
        if self.folders[i].books != books {
            self.folders[i].books = books;
            self.save();
        }
        Some(i)
    }
}

/// Sorts books by folder (the library root first), then by name, both in
/// natural order ("Vol 2" before "Vol 10").
pub fn sort(books: &mut [Book]) {
    books.sort_by(|a, b| natural_cmp(&a.dir, &b.dir).then_with(|| natural_cmp(&a.name, &b.name)));
}

/// Consecutive runs of books in the same folder: (folder path, index range).
pub fn sections(books: &[Book]) -> Vec<(String, Range<usize>)> {
    let mut out: Vec<(String, Range<usize>)> = Vec::new();
    for (i, b) in books.iter().enumerate() {
        match out.last_mut() {
            Some((dir, r)) if *dir == b.dir => r.end = i + 1,
            _ => out.push((b.dir.clone(), i..i + 1)),
        }
    }
    out
}

/// Display title of a file name: without a ".pdf" extension.
pub fn title(name: &str) -> &str {
    let n = name.len();
    if n > 4 && name.is_char_boundary(n - 4) && name[n - 4..].eq_ignore_ascii_case(".pdf") {
        &name[..n - 4]
    } else {
        name
    }
}

/// Case-insensitive comparison where runs of digits compare as numbers.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = x.next_if(char::is_ascii_digit) {
                    na.push(c);
                }
                let mut nb = String::new();
                while let Some(d) = y.next_if(char::is_ascii_digit) {
                    nb.push(d);
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                let ord = c.to_lowercase().cmp(d.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                x.next();
                y.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(dir: &str, name: &str) -> Book {
        Book { uri: format!("content://t/{dir}/{name}"), name: name.into(), dir: dir.into() }
    }

    #[test]
    fn natural() {
        let mut v = vec!["Vol 10.pdf", "vol 2.pdf", "Vol 1.pdf", "Apple", "vol 02b", "Zeta"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["Apple", "Vol 1.pdf", "vol 2.pdf", "vol 02b", "Vol 10.pdf", "Zeta"]);
        assert_eq!(natural_cmp("a", "A"), "a".cmp("A"));
        assert_eq!(title("Book.PDF"), "Book");
        assert_eq!(title(".pdf"), ".pdf");
        assert_eq!(title("Книга.pdf"), "Книга");
    }

    #[test]
    fn grouping() {
        let mut books =
            vec![book("Manga/B", "2.pdf"), book("", "z.pdf"), book("Manga/B", "10.pdf"), book("Manga", "x.pdf"), book("", "a.pdf")];
        sort(&mut books);
        let s = sections(&books);
        let dirs: Vec<&str> = s.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(dirs, ["", "Manga", "Manga/B"]);
        assert_eq!(s[0].1, 0..2);
        assert_eq!(books[3].name, "2.pdf");
        assert_eq!(books[4].name, "10.pdf");
    }

    #[test]
    fn persistence() {
        let dir = std::env::temp_dir().join(format!("justpdf-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut lib = Library::load(Some(dir.clone()));
        assert!(lib.folders.is_empty());
        let i = lib.add("content://tree/a", "Books");
        assert_eq!(lib.add("content://tree/b", " "), 1);
        lib.set_books("content://tree/a", vec![book("Sub", "b.pdf"), book("", "a\tb.pdf")]);
        assert_eq!(lib.add("content://tree/a", "Books"), i);
        let lib2 = Library::load(Some(dir.clone()));
        assert_eq!(lib2.folders.len(), 2);
        assert_eq!(lib2.folders[1].name, "Folder");
        assert_eq!(lib2.folders[0].books.len(), 2);
        assert_eq!(lib2.folders[0].books[0].name, "a b.pdf");
        assert_eq!(lib2.folders[0].books[1].dir, "Sub");
        let mut lib3 = Library::load(Some(dir.clone()));
        lib3.remove(0);
        assert_eq!(Library::load(Some(dir.clone())).folders[0].uri, "content://tree/b");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
