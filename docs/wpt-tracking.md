# WPT tracking — Legatus engine line

How our score is defined: **upstream daily score + our fixed subtests**.
Upstream runs the full suite daily (wpt.servo.org); we inherit every
upstream point by rebasing `legatus` onto their `main`, then add our own
fixes on top. Rebase cadence: weekly (or when upstream lands something big).

## Baselines (upstream `scores.json`, "All WPT tests")

| Date | Upstream score | Upstream subtests | Our delta | Our line |
|------|---------------|-------------------|-----------|----------|
| 2026-09-17 | 66.43% (38600/58110) | 93.43% (2021567/2163669) | +11 subtests, 4 files | `legatus` @ 061fe8d6 on upstream 0f4d68e0 |

## Our fixes (each verified live via `probe` before commit)

1. Fragment `:target` before `load` — `url/data-uri-fragment.html` (+1)
2. Unlabeled docs → windows-1252 — `encoding/sniffing.html` (+1)
3. JSON docs → UTF-8 w/o charset — `encoding/json-document-utf8.html` (+3)
4. Opaque-path trailing space → `%20` (rust-url fork) — `url/urlsearchparams-delete.any.js` (+4 html; worker variant same code path)

rust-url fork: 13 fixture expectations cleared, suite green.

## Target ranking (failing subtests, 2026-09-17)

editing 23,879 · css-grid 8,154 · IndexedDB 2,033 · css-text 1,989 ·
flexbox 1,685 · css-sizing 1,288 · css-align 1,089 · CSP 869 · cssom 648

## Upstream velocity (for the race)

25.82% (2023-04) → 48.86% (2025-09) → 60.81% (2026-06) → 66.43% (now).
~1.5pp/month. Our line must out-fix that delta to gain ground.
