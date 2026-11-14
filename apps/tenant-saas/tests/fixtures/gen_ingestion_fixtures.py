"""Regenerates the small committed fixtures in tests/fixtures/ingestion/."""
import os

out = os.path.join(os.path.dirname(__file__), "ingestion")
os.makedirs(out, exist_ok=True)


def w(name, data):
    with open(os.path.join(out, name), "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode("utf-8"))


w("simple.txt", "Hello world.\nThis is a simple text fixture.\n")
w("crlf.txt", "line one\r\nline two\r\n\r\n\r\n\r\nline three\r\n")
w("bom_control_chars.txt", b"\xef\xbb\xbfVisible\x00 text\x07 here\n")
w(
    "headings.md",
    "Preamble text.\n\n# Title\n\nIntro paragraph.\n\n## Second\n\n"
    "```sh\n# not a heading\necho hi\n```\n\nAfter code.\n\n### Third\n\nEnd.\n",
)


def pdf(pages):
    """Builds a minimal valid PDF. Each page is a content-stream string (may be empty)."""
    objs = {}
    n = len(pages)
    kids = " ".join(f"{4 + 2 * i} 0 R" for i in range(n))
    objs[1] = "<< /Type /Catalog /Pages 2 0 R >>"
    objs[2] = f"<< /Type /Pages /Kids [{kids}] /Count {n} >>"
    objs[3] = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    for i, content in enumerate(pages):
        p, c = 4 + 2 * i, 5 + 2 * i
        objs[p] = (
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 3 0 R >> >> /Contents {c} 0 R >>"
        )
        objs[c] = f"<< /Length {len(content)} >>\nstream\n{content}\nendstream"
    buf = b"%PDF-1.4\n"
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(buf)
        buf += f"{num} 0 obj\n{objs[num]}\nendobj\n".encode()
    xref = len(buf)
    size = max(objs) + 1
    buf += f"xref\n0 {size}\n0000000000 65535 f \n".encode()
    for num in range(1, size):
        buf += f"{offsets[num]:010d} 00000 n \n".encode()
    buf += f"trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return buf


def text_page(s):
    return f"BT /F1 14 Tf 72 700 Td ({s}) Tj ET"


w("two_pages.pdf", pdf([text_page("First page text"), text_page("Second page text")]))
w("empty_text_layer.pdf", pdf([""]))
w("corrupt.pdf", b"%PDF-1.4\n" + bytes(range(1, 200)) * 3)
