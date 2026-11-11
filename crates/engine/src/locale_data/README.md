These tables are derived from the MIT-licensed Moment.js 2.30.1 as bundled
with the desktop app. `LICENSE` retains Moment's copyright and license. The product
formatter is independently implemented in Rust and does not execute JavaScript.

Pinned bundled `lib/moment.min.js` SHA-256:
`b19e17770b1673aca44065e7dc2c393d306559871b99bfe885a0d10d4f2798cf`.
The public upstream sources are https://github.com/moment/moment/tree/2.30.1
and the official token reference is https://momentjs.com/docs/#/displaying/format/.

The reproduction script is `crates/engine/tools/export-moment-locales.cjs`;
run it with `TZ=UTC`, passing the pinned bundled library path and an output JSON
path. No bundled JavaScript library is included in the repository. It enumerates all 138 locales, contextual month/weekday
forms, week rules, localized presets, meridiem minute transitions, digit/comma
postformat mappings and ordinal prefix/suffix cycles (domain 0..9999, preserving
exceptional literal results). Regex rules and era boundaries are retained.
These are locale data, not a port of proprietary application code.

`tests/fixtures/reference_moment_clock.json` retains 1008 actual application
outputs across UTC/Los Angeles, 14 languages and year/leap/DST boundaries.
`reference_moment_locales.json` retains 44712 actual application outputs across
all 138 locales, 13 calendar/era/grammar/escape instants and 23 meridiem hours.
Its string table only deduplicates exact observed bytes.

Supported scope is built-in local Moment date formatting for daily/templates.
Moment parsing/arithmetic, UTC-mode timezone labels, dynamically injected
JavaScript locales, moment locale-array negotiation and JavaScript Date's full
+/-275760 range are separate requirements. Engine locale is an internal explicit
setting; automatic import of the desktop app's global language preference remains
pending. All locale availability does not prove every possible token/date/input
combination; the differential fixtures state the verified boundary.
