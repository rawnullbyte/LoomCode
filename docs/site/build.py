#!/usr/bin/env python3
"""Build the Loom documentation site for GitHub Pages.

Deliberately dependency-free: the repository has no Node or Python toolchain
requirement, so the site is generated with the standard library only. The
Markdown subset implemented here is exactly what the repository's own documents
use (headings, tables, fenced code, lists, links, images, rules, blockquotes).

Usage:
    python3 docs/site/build.py            # write docs/site/_out
    python3 docs/site/build.py --check    # build, then verify links and anchors
"""

from __future__ import annotations

import argparse
import html
import re
import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SITE = ROOT / "docs" / "site"
OUT = SITE / "_out"


# ── project metadata ─────────────────────────────────────────────────────────

def project_version() -> str:
    """The workspace version, so the site cannot drift from the crates."""
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^\[workspace\.package\](?:(?!\n\[).)*?version\s*=\s*"([^"]+)"',
                      text, re.S | re.M)
    return match.group(1) if match else "0.0.0"


REPO_URL = "https://github.com/rawnullbyte/LoomCode"


# ── Markdown ─────────────────────────────────────────────────────────────────

def escape(text: str) -> str:
    return html.escape(text, quote=False)


def slugify(text: str) -> str:
    """Stable, readable anchors. Keeps unicode letters, drops punctuation."""
    text = re.sub(r"`", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)  # links -> their label
    text = text.strip().lower()
    text = re.sub(r"[^\w\s-]", "", text, flags=re.UNICODE)
    text = re.sub(r"[\s_]+", "-", text.strip())
    return re.sub(r"-{2,}", "-", text).strip("-") or "section"


class Inline:
    """Inline formatting for one block of text.

    Escaping happens first, then code spans are stashed so their contents are
    never reinterpreted as emphasis or links. That ordering is what keeps
    `--flag` and `a_b` intact.
    """

    def __init__(self, link_map: dict[str, str]):
        self.link_map = link_map

    def __call__(self, text: str) -> str:
        stashed: list[str] = []

        def stash(fragment: str) -> str:
            stashed.append(fragment)
            return f"\x00{len(stashed) - 1}\x00"

        out = escape(text)

        # Code spans first: their contents are literal.
        out = re.sub(r"`([^`]+)`", lambda m: stash(f"<code>{m.group(1)}</code>"), out)

        def image(m: re.Match[str]) -> str:
            alt, src = m.group(1), self.resolve(m.group(2))
            return stash(f'<img src="{src}" alt="{alt}" loading="lazy">')

        def link(m: re.Match[str]) -> str:
            label, href = m.group(1), m.group(2)
            target = self.resolve(href)
            external = target.startswith(("http://", "https://", "mailto:"))
            attrs = ' rel="noopener"' if external else ""
            return stash(f'<a href="{target}"{attrs}>{label}</a>')

        out = re.sub(r"!\[([^\]]*)\]\(([^)\s]+)\)", image, out)
        out = re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", link, out)

        # Emphasis. Bold before italic so `**x**` is not eaten as two italics.
        out = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", out)
        out = re.sub(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])", r"<em>\1</em>", out)

        # Restore stashed fragments (which may themselves nest once).
        for _ in range(3):
            if "\x00" not in out:
                break
            out = re.sub(r"\x00(\d+)\x00", lambda m: stashed[int(m.group(1))], out)
        return out

    def resolve(self, href: str) -> str:
        """Rewrite repository-relative links for the generated site."""
        href = href.strip()
        if href in self.link_map:
            return self.link_map[href]
        if href.startswith(("http://", "https://", "mailto:", "#")):
            return href
        base, _, anchor = href.partition("#")
        base = base.lstrip("./")
        mapped = self.link_map.get(base)
        if mapped is not None and not mapped.startswith(("http://", "https://")) and anchor:
            return f"{mapped}#{anchor}" if "#" not in mapped else mapped
        if mapped is None:
            mapped = self.link_map.get(str(Path(base)), base)
        if mapped is None:
            return href
        return f"{mapped}#{anchor}" if anchor else mapped


@dataclass
class Heading:
    level: int
    text: str
    anchor: str


@dataclass
class Rendered:
    html: str
    headings: list[Heading] = field(default_factory=list)
    title: str = ""


def rewrite_html_src(block: str, link_map: dict[str, str]) -> str:
    """Rewrite repository-relative src/href attributes inside passed-through HTML."""
    def sub(m: re.Match[str]) -> str:
        attr, value = m.group(1), m.group(2)
        if value.startswith(("http://", "https://", "mailto:", "#", "data:")):
            return m.group(0)
        mapped = link_map.get(value.lstrip("./"))
        if mapped is None:
            return m.group(0)
        return f'{attr}="{mapped}"'

    return re.sub(r'\b(src|href)="([^"]+)"', sub, block)


def render_markdown(source: str, link_map: dict[str, str]) -> Rendered:
    inline = Inline(link_map)
    lines = source.replace("\r\n", "\n").split("\n")
    out: list[str] = []
    headings: list[Heading] = []
    title = ""
    seen: dict[str, int] = {}
    i = 0

    def anchor_for(text: str) -> str:
        base = slugify(text)
        count = seen.get(base, 0)
        seen[base] = count + 1
        return base if count == 0 else f"{base}-{count}"

    while i < len(lines):
        line = lines[i]

        # Centred wrapper: GitHub renders the Markdown inside <div align="center">,
        # so the site does too — the wrapper is re-emitted around the rendered block.
        if re.match(r'^\s*<div\s+align="center"\s*>\s*$', line):
            i += 1
            inner: list[str] = []
            while i < len(lines) and not re.match(r"^\s*</div>\s*$", lines[i]):
                inner.append(lines[i])
                i += 1
            i += 1
            block = render_markdown("\n".join(inner), link_map)
            out.append(
                f'<div class="center-block">\n'
                f'{rewrite_html_src(block.html, link_map)}\n</div>'
            )
            continue

        # Any other block-level HTML is passed through untouched.
        if re.match(r"^\s*<(div|p|table|section|details|figure|picture|video)\b", line, re.I):
            raw: list[str] = []
            while i < len(lines) and lines[i].strip():
                raw.append(lines[i])
                i += 1
            out.append(rewrite_html_src("\n".join(raw), link_map))
            continue

        # fenced code
        fence = re.match(r"^(\s*)```(\S*)\s*$", line)
        if fence:
            indent, lang = len(fence.group(1)), fence.group(2)
            i += 1
            body: list[str] = []
            while i < len(lines) and not re.match(r"^\s*```\s*$", lines[i]):
                raw = lines[i]
                body.append(raw[indent:] if raw[:indent].strip() == "" else raw)
                i += 1
            i += 1
            cls = f' class="language-{lang}"' if lang else ""
            out.append(f"<pre><code{cls}>{escape(chr(10).join(body))}</code></pre>")
            continue

        # blank
        if not line.strip():
            i += 1
            continue

        # Horizontal rules, but not a `#`-fenced heading line: comment-only lines
        # such as `# api_key_env = ...` (and their trailing context) are safe to ignore
        # at the top level -- GitHub renders them inside <pre> only, and here they live
        # inside real fenced blocks, so a `/^#/` test inside a SHA-style TOML aside
        # would be a false positive. Keep the generic check but require that the line
        # genuinely opens a heading (hash, space, text).
        if re.match(r"^\s*(?:---+|\*\*\*+|___+)\s*$", line):
            out.append("<hr>")
            i += 1
            continue

        # heading
        head = re.match(r"^(#{1,6})\s+(.*)$", line)
        if head:
            level = len(head.group(1))
            text = head.group(2).strip()
            anchor = anchor_for(text)
            headings.append(Heading(level, re.sub(r"[`*]", "", text), anchor))
            if level == 1 and not title:
                title = re.sub(r"[`*]", "", text)
            out.append(f'<h{level} id="{anchor}">{inline(text)}</h{level}>')
            i += 1
            continue

        # table: header row + separator row
        if line.lstrip().startswith("|") and i + 1 < len(lines) and \
                re.match(r"^\s*\|?[\s:|-]+\|[\s:|-]*$", lines[i + 1]):
            header = split_row(line)
            aligns = []
            for cell in split_row(lines[i + 1]):
                cell = cell.strip()
                if cell.startswith(":") and cell.endswith(":"):
                    aligns.append("center")
                elif cell.endswith(":"):
                    aligns.append("right")
                else:
                    aligns.append("left")
            i += 2
            rows: list[list[str]] = []
            while i < len(lines) and lines[i].lstrip().startswith("|"):
                rows.append(split_row(lines[i]))
                i += 1
            out.append("<table>")
            out.append("<thead><tr>")
            for n, cell in enumerate(header):
                style = f' style="text-align:{aligns[n]}"' if n < len(aligns) else ""
                out.append(f"<th{style}>{inline(cell)}</th>")
            out.append("</tr></thead><tbody>")
            for row in rows:
                out.append("<tr>")
                for n in range(len(header)):
                    cell = row[n] if n < len(row) else ""
                    style = f' style="text-align:{aligns[n]}"' if n < len(aligns) else ""
                    out.append(f"<td{style}>{inline(cell)}</td>")
                out.append("</tr>")
            out.append("</tbody></table>")
            continue

        # blockquote
        if line.lstrip().startswith(">"):
            body = []
            while i < len(lines) and lines[i].lstrip().startswith(">"):
                body.append(re.sub(r"^\s*>\s?", "", lines[i]))
                i += 1
            out.append(f"<blockquote><p>{inline(' '.join(x.strip() for x in body))}</p></blockquote>")
            continue

        # lists
        if re.match(r"^\s*([-*+]|\d+\.)\s+", line):
            html_list, i = render_list(lines, i, inline)
            out.append(html_list)
            continue

        # paragraph: consume until a blank line or a block start
        body = [line.strip()]
        i += 1
        while i < len(lines):
            nxt = lines[i]
            if not nxt.strip():
                break
            if re.match(r"^\s*(#{1,6}\s|```|>|\|)", nxt) or \
                    re.match(r"^\s*([-*+]|\d+\.)\s+", nxt) or \
                    re.match(r"^\s*(?:---+|\*\*\*+|___+)\s*$", nxt):
                break
            body.append(nxt.strip())
            i += 1
        out.append(f"<p>{inline(' '.join(body))}</p>")

    return Rendered("\n".join(out), headings, title)


def split_row(line: str) -> list[str]:
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|"):
        line = line[:-1]
    # Split on pipes that are not inside a code span.
    cells, current, in_code = [], [], False
    for ch in line:
        if ch == "`":
            in_code = not in_code
        if ch == "|" and not in_code:
            cells.append("".join(current).strip())
            current = []
            continue
        current.append(ch)
    cells.append("".join(current).strip())
    return cells


def render_list(lines: list[str], start: int, inline: Inline) -> tuple[str, int]:
    """Render a (possibly nested) list. Returns (html, next_index)."""
    first = re.match(r"^(\s*)([-*+]|\d+\.)\s+(.*)$", lines[start])
    assert first is not None
    ordered = first.group(2)[0].isdigit()
    base_indent = len(first.group(1))
    tag = "ol" if ordered else "ul"

    items: list[str] = []
    i = start
    while i < len(lines):
        m = re.match(r"^(\s*)([-*+]|\d+\.)\s+(.*)$", lines[i])
        if not m or len(m.group(1)) < base_indent:
            break
        if len(m.group(1)) > base_indent:
            # Nested list: attach to the previous item.
            nested, i = render_list(lines, i, inline)
            if items:
                items[-1] = items[-1].rstrip() + nested
            continue
        if m.group(2)[0].isdigit() != ordered:
            break
        text = m.group(3).strip()
        i += 1
        # Continuation lines (indented, not a new block) belong to this item.
        while i < len(lines):
            nxt = lines[i]
            if not nxt.strip():
                # A blank line only continues the list if the next line is indented.
                if i + 1 < len(lines) and re.match(r"^\s{2,}\S", lines[i + 1]) and \
                        not re.match(r"^\s*([-*+]|\d+\.)\s+", lines[i + 1]):
                    i += 1
                    continue
                break
            if re.match(r"^\s*([-*+]|\d+\.)\s+", nxt):
                break
            if len(nxt) - len(nxt.lstrip()) <= base_indent and nxt.lstrip().startswith(("#", "```", "|", ">")):
                break
            text += " " + nxt.strip()
            i += 1
        items.append(f"<li>{inline(text)}</li>")

    body = "\n".join(items)
    return f"<{tag}>\n{body}\n</{tag}>", i


# ── page shell ───────────────────────────────────────────────────────────────

PAGES = [
    ("index.html", "Overview", None),
    ("guide.html", "Guide", "README.md"),
    ("harness.html", "Harness", "docs/HARNESS.md"),
]

# Repository path -> generated page. Both directions are needed: documents link
# to each other by repository path, and the site must keep those links working.
LINK_MAP = {
    "README.md": "guide.html",
    "docs/HARNESS.md": "harness.html",
    "LICENSE": f"{REPO_URL}/blob/main/LICENSE",
    "crates/loon-core/skills/": f"{REPO_URL}/tree/main/crates/loon-core/skills",
    "docs/site/assets/logo.svg": "assets/logo.svg",
    "Cargo.toml": f"{REPO_URL}/blob/main/Cargo.toml",
    "docs/previews/appearance.svg": "index.html#showcase",
    "docs/previews/configuration.svg": "index.html#showcase",
    "docs/previews/context7.svg": "index.html#showcase",
    "docs/previews/keybindings.svg": "index.html#showcase",
    "docs/previews/modes.svg": "index.html#showcase",
    "docs/previews/palette.svg": "index.html#showcase",
    "docs/previews/models.svg": "index.html#showcase",
    "docs/previews/providers.svg": "index.html#showcase",
    "docs/previews/settings.svg": "index.html#showcase",
    "docs/previews/thinking.svg": "index.html#showcase",
    "docs/previews/workspace.svg": "index.html#showcase",
    "docs/previews/": "index.html#showcase",
}

# Preview images are copied next to the pages so every page can reference them
# with one relative path, independent of the GitHub Pages base path.
PREVIEW_DIR = "assets/previews"


def nav_html(current: str) -> str:
    links = []
    for href, label, _ in PAGES:
        attr = ' aria-current="page"' if href == current else ""
        links.append(f'<a href="{href}"{attr}>{label}</a>')
    links.append(f'<a class="gh" href="{REPO_URL}" rel="noopener">GitHub</a>')
    return "\n        ".join(links)


def shell(*, title: str, description: str, current: str, body: str,
          version: str, body_class: str = "") -> str:
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{html.escape(title)}</title>
<meta name="description" content="{html.escape(description)}">
<meta name="theme-color" content="#0e1c28">
<meta property="og:title" content="{html.escape(title)}">
<meta property="og:description" content="{html.escape(description)}">
<meta property="og:type" content="website">
<meta property="og:image" content="assets/logo.svg">
<link rel="icon" href="assets/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="assets/site.css">
</head>
<body{f' class="{body_class}"' if body_class else ""}>
<a class="skip" href="#main">Skip to content</a>
<header class="site-header">
  <div class="wrap">
    <a class="brand" href="index.html">
      <svg viewBox="0 0 64 64" aria-hidden="true"><title>Loom</title>
      <rect width="64" height="64" rx="14" fill="#0e1c28" stroke="rgba(120,188,225,.16)"/>
      <g fill="none" stroke="#5bdac8" stroke-linecap="round">
      <path d="M17 17h30M17 47h30" stroke-width="4"/>
      <path d="M24 21v22M32 21v22M40 21v22" stroke-width="3.2"/>
      <path d="M17 32c5-4.5 10-4.5 15 0s10 4.5 15 0" stroke-width="4.6"/>
      </g></svg>
      <span class="brand-name">Loom</span>
    </a>
    <nav class="site-nav" aria-label="Site">
        {nav_html(current)}
    </nav>
  </div>
</header>
<main id="main">
{body}
</main>
<footer class="site-footer">
  <div class="wrap">
    <span>Loom v{version} · MIT licensed · built from the repository's own documents</span>
    <nav aria-label="Footer">
      <a href="guide.html">Guide</a>
      <a href="harness.html">Harness</a>
      <a href="{REPO_URL}" rel="noopener">GitHub</a>
    </nav>
  </div>
</footer>
</body>
</html>
"""


def toc_html(headings: list[Heading]) -> str:
    items = []
    for h in headings:
        if h.level not in (2, 3):
            continue
        items.append(f'<li class="lvl-{h.level}"><a href="#{h.anchor}">{escape(h.text)}</a></li>')
    if not items:
        return ""
    return ('<aside class="toc" aria-label="On this page">\n'
            '<div class="toc-title">On this page</div>\n<ul>\n'
            + "\n".join(items) + "\n</ul>\n</aside>")


def pager_html(current: str) -> str:
    order = [p for p in PAGES if p[0] != "index.html"]
    idx = next((n for n, p in enumerate(order) if p[0] == current), None)
    if idx is None:
        return ""
    parts = []
    if idx > 0:
        href, label, _ = order[idx - 1]
        parts.append(f'<a href="{href}"><span class="k">Previous</span>'
                     f'<span class="v">{label}</span></a>')
    if idx + 1 < len(order):
        href, label, _ = order[idx + 1]
        parts.append(f'<a href="{href}"><span class="k">Next</span>'
                     f'<span class="v">{label}</span></a>')
    return f'<nav class="pager">{"".join(parts)}</nav>' if parts else ""


# ── build ────────────────────────────────────────────────────────────────────

def build() -> list[Path]:
    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "assets").mkdir(parents=True)
    (OUT / PREVIEW_DIR).mkdir(parents=True)

    for asset in (SITE / "assets").iterdir():
        if asset.is_file():
            shutil.copy2(asset, OUT / "assets" / asset.name)

    previews = ROOT / "docs" / "previews"
    if previews.is_dir():
        for img in previews.glob("*.svg"):
            shutil.copy2(img, OUT / PREVIEW_DIR / img.name)

    version = project_version()
    written: list[Path] = []

    # Landing page: prose lives in HTML so it can be art-directed.
    landing = (SITE / "content" / "landing.html").read_text(encoding="utf-8")
    landing = landing.replace("{{VERSION}}", version).replace("{{REPO}}", REPO_URL)
    page = shell(
        title="Loom — a terminal-native coding agent you can inspect",
        description="Loom is a transparent, inspectable coding agent for the terminal: "
                    "any OpenAI-compatible endpoint, durable plans, an independent "
                    "completion supervisor, and a journal you can replay.",
        current="index.html", body=landing, version=version, body_class="landing")
    (OUT / "index.html").write_text(page, encoding="utf-8")
    written.append(OUT / "index.html")

    for href, label, source in PAGES:
        if source is None:
            continue
        path = ROOT / source
        if not path.is_file():
            # A document referenced by PAGES but absent from the worktree must not
            # break the deploy: emit a clearly-labelled placeholder so links keep
            # resolving and the gap is visible instead of silent.
            print(f"  ! {source} is missing; emitting a placeholder page")
            body = f"""<div class="wrap">
<div class="doc-layout">
<article class="doc">
<h1>{label}</h1>
<p class="doc-meta">generated from <code>{source}</code></p>
<div class="callout warn">
  <b>{source} was not present in the repository when this site was built.</b>
  The page is a placeholder so links keep working. Restore the file and rebuild.
</div>
{pager_html(href)}
</article>
</div>
</div>"""
            page = shell(title=f"{label} — Loom", description=f"Loom {label.lower()}.",
                         current=href, body=body, version=version)
            (OUT / href).write_text(page, encoding="utf-8")
            written.append(OUT / href)
            continue
        text = path.read_text(encoding="utf-8")
        rendered = render_markdown(text, LINK_MAP)
        title = f"{rendered.title or label} — Loom"
        first_para = next((ln for ln in text.split("\n\n")
                           if ln.strip() and not ln.startswith(("#", "|", "```", "-"))), "")
        description = re.sub(r"[`*\[\]]|\([^)]*\)", "", first_para)[:180].strip() or \
            f"Loom {label.lower()}."
        body = f"""<div class="wrap">
<div class="doc-layout">
{toc_html(rendered.headings)}
<article class="doc">
<p class="doc-meta">Loom v{version} · generated from <code>{source}</code></p>
{rendered.html}
{pager_html(href)}
</article>
</div>
</div>"""
        page = shell(title=title, description=description, current=href,
                     body=body, version=version)
        (OUT / href).write_text(page, encoding="utf-8")
        written.append(OUT / href)

    (OUT / ".nojekyll").write_text("", encoding="utf-8")
    return written


def check(pages: list[Path]) -> int:
    """Verify internal links, anchors and referenced assets resolve."""
    problems = 0
    anchors: dict[str, set[str]] = {}
    for page in pages:
        text = page.read_text(encoding="utf-8")
        anchors[page.name] = set(re.findall(r'id="([^"]+)"', text))

    for page in pages:
        text = page.read_text(encoding="utf-8")
        for href in re.findall(r'(?:href|src)="([^"]+)"', text):
            if href.startswith(("http://", "https://", "mailto:", "#")):
                if href.startswith("#") and href[1:] not in anchors[page.name]:
                    print(f"  ✗ {page.name}: missing anchor {href}")
                    problems += 1
                continue
            target, _, frag = href.partition("#")
            path = OUT / target
            if not path.exists():
                print(f"  ✗ {page.name}: missing file {target}")
                problems += 1
            elif frag and target.endswith(".html") and frag not in anchors.get(target, set()):
                print(f"  ✗ {page.name}: missing anchor {target}#{frag}")
                problems += 1

        # Every page must be self-describing.
        if "<title>" not in text:
            print(f"  ✗ {page.name}: no <title>")
            problems += 1
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the Loom documentation site.")
    parser.add_argument("--check", action="store_true", help="verify links after building")
    args = parser.parse_args()

    pages = build()
    total = sum(p.stat().st_size for p in OUT.rglob("*") if p.is_file())
    print(f"built {len(pages)} pages into {OUT.relative_to(ROOT)} "
          f"({total / 1024:.0f} KiB total)")

    if args.check:
        problems = check(pages)
        if problems:
            print(f"{problems} link problem(s)")
            return 1
        print("links and anchors verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
