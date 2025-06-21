# Brand masters

Full-resolution source art. Nothing here is imported by an app, so nothing here is
bundled or shipped — these are the originals the shipped assets are derived from.

| File       | Size         | Derived into                                                                               |
| ---------- | ------------ | ------------------------------------------------------------------------------------------ |
| `mark.png` | 526x768 RGBA | `apps/reader/src/lib/assets/mark.png` and `apps/admin/src/lib/assets/mark.png`, both 66x96 |

## Why the shipped mark is small

The header renders the mark at `height: 0.8em` of a 20px line (reader) and `height: 18px`
(admin) — 16–18 CSS px. Shipping the 526x768 master meant every visitor downloaded 67 KB
to paint a 19x28 device-pixel box; Lighthouse flagged it as 66.9 KB of pure waste, the
single largest non-cover image on the page. 96 px tall is 5x the largest on-screen use, so
it stays crisp on a DPR-3 phone, and costs 5 KB.

Regenerate the shipped copies after editing the master:

```
python3 - <<'PY'
from PIL import Image
src = Image.open('assets/brand/mark.png').convert('RGBA')
w, h = src.size
out = src.resize((max(1, round(w * 96 / h)), 96), Image.LANCZOS)
for p in ('apps/reader/src/lib/assets/mark.png', 'apps/admin/src/lib/assets/mark.png'):
    out.save(p, format='PNG', optimize=True)
PY
```
