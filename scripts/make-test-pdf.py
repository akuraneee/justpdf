#!/usr/bin/env python3
"""Writes a test PDF: text pages, tall 'webtoon' strips, a nested outline.

usage: make-test-pdf.py OUT.pdf [PAGES]
"""
import sys

out = sys.argv[1]
pages = int(sys.argv[2]) if len(sys.argv) > 2 else 60

LOREM = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor "
         "incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud "
         "exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure "
         "dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur.").split()

objs = {}
def obj(n, body):
    objs[n] = body

def esc(s):
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")

def text_page(i):
    w, h = 595, 842
    lines = [f"BT /F2 36 Tf 56 {h-100} Td (Page {i+1}) Tj ET"]
    if i % 10 == 0:
        lines.append(f"BT /F2 20 Tf 56 {h-140} Td (Chapter {i//10 + 1}) Tj ET")
    y = h - 190
    words = LOREM * 3
    k = (i * 7) % len(LOREM)
    words = words[k:] + words[:k]
    line = []
    for wd in words:
        line.append(wd)
        if len(" ".join(line)) > 80:
            lines.append(f"BT /F1 12 Tf 56 {y} Td ({esc(' '.join(line))}) Tj ET")
            line = []
            y -= 17
            if y < 80:
                break
    if i == 3:
        lines.append(f"BT /F1 12 Tf 56 60 Td (The needle is hidden here: haystack-needle.) Tj ET")
    lines.append(f"0.2 0.4 0.8 RG 2 w 40 40 {w-80} {h-80} re S")
    return w, h, "\n".join(lines)

def strip_page(i):
    w, h = 800, 6000
    parts = []
    n = 24
    for j in range(n):
        r = (j * 37 % 255) / 255; g = (j * 91 % 255) / 255; b = (j * 53 % 255) / 255
        y = h - (j + 1) * (h / n)
        parts.append(f"{r:.3f} {g:.3f} {b:.3f} rg 0 {y:.1f} {w} {h/n:.1f} re f")
        parts.append(f"1 1 1 rg 0 0 0 RG 3 w 400 {y + h/n/2:.1f} 120 0 360 arc\n" if False else "")
        parts.append(f"BT /F2 48 Tf 1 1 1 rg 60 {y + h/n/2:.1f} Td (Strip {i+1} panel {j+1}) Tj ET")
    return w, h, "\n".join(parts)

kids = []
n = 10
page_objs = []
for i in range(pages):
    w, h, content = strip_page(i) if i % 10 == 5 else text_page(i)
    pn, cn = n, n + 1
    n += 2
    data = content.encode("latin-1")
    obj(cn, b"<< /Length %d >>\nstream\n" % len(data) + data + b"\nendstream")
    obj(pn, f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {cn} 0 R "
            f"/Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> >>".encode())
    kids.append(pn)
    page_objs.append((pn, h))

obj(3, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>")
obj(4, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>")
obj(2, f"<< /Type /Pages /Kids [{' '.join(f'{k} 0 R' for k in kids)}] /Count {len(kids)} >>".encode())

# Outline: one entry per chapter (every 10 pages) with two sub-entries.
chapters = list(range(0, pages, 10))
outline_root = n; n += 1
entries = []
for ci, p in enumerate(chapters):
    entries.append((n, ci, p)); n += 1
first_children = {}
for (eid, ci, p) in entries:
    subs = []
    for s, off in enumerate((3, 6)):
        if p + off < pages:
            subs.append((n, p + off, f"Section {ci+1}.{s+1}")); n += 1
    first_children[eid] = subs
for idx, (eid, ci, p) in enumerate(entries):
    pn, ph = page_objs[p]
    d = [f"/Title ({esc('Chapter %d: The %s part' % (ci+1, LOREM[ci % len(LOREM)]))})", f"/Parent {outline_root} 0 R",
         f"/Dest [{pn} 0 R /XYZ 0 {ph} 0]"]
    if idx > 0: d.append(f"/Prev {entries[idx-1][0]} 0 R")
    if idx + 1 < len(entries): d.append(f"/Next {entries[idx+1][0]} 0 R")
    subs = first_children[eid]
    if subs:
        d.append(f"/First {subs[0][0]} 0 R /Last {subs[-1][0]} 0 R /Count {len(subs)}")
        for si, (sid, sp, title) in enumerate(subs):
            spn, sph = page_objs[sp]
            sd = [f"/Title ({esc(title)})", f"/Parent {eid} 0 R", f"/Dest [{spn} 0 R /XYZ 0 {sph/2} 0]"]
            if si > 0: sd.append(f"/Prev {subs[si-1][0]} 0 R")
            if si + 1 < len(subs): sd.append(f"/Next {subs[si+1][0]} 0 R")
            obj(sid, ("<< " + " ".join(sd) + " >>").encode())
    obj(eid, ("<< " + " ".join(d) + " >>").encode())
obj(outline_root, f"<< /Type /Outlines /First {entries[0][0]} 0 R /Last {entries[-1][0]} 0 R /Count {len(entries)} >>".encode())
obj(1, f"<< /Type /Catalog /Pages 2 0 R /Outlines {outline_root} 0 R /PageMode /UseOutlines >>".encode())

size = max(objs) + 1
buf = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
offsets = [0] * size
for k in range(1, size):
    if k not in objs:
        continue
    offsets[k] = len(buf)
    buf += b"%d 0 obj\n" % k + objs[k] + b"\nendobj\n"
xref = len(buf)
buf += b"xref\n0 %d\n" % size
buf += b"0000000000 65535 f \n"
for k in range(1, size):
    buf += (b"%010d 00000 n \n" % offsets[k]) if k in objs else b"0000000000 65535 f \n"
buf += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (size, xref)
open(out, "wb").write(buf)
print(f"wrote {out}: {pages} pages, {len(buf)} bytes")
