# AGENTS.md

Orientation for AI agents (and humans) working on **prowl**. Keep this file up
to date in the same change whenever the architecture, modules, queries, data
model, or workflow change.

## What prowl is

A small terminal dashboard that watches a GitHub repo and re-renders on an
interval. It has two **views**, toggled with **Tab** (and chosen for one-shot
output with `--view`):

- **Mine** (default): **My open PRs → Merge Queue → My merged PRs → My
  Shipments**.
- **Reviews**: **Reviews** (open PRs awaiting / under my review, each with a
  per-row review-state glyph) **→ Reviewed & merged** (merged PRs I reviewed).

Below the active view is a `r refresh (every 5m) - tab switch view - enter open -
/ search - ? help` footer (which also shows the refresh interval, and reads `r
refreshing` while a fetch is in flight), an optional search prompt, and an
optional help legend last at the bottom. While watching, the very top shows a
`my PRs / reviews` tab strip with the active view accented. It rings the terminal
bell when one of your PRs merges or an open PR's status changes, and flags the
changed rows (the bell and change markers track the Mine view only). The
interactive watch runs on the
[**uncurses**](https://github.com/aymanbagabas/uncurses) toolkit with an event
loop: it shows an *inline* `Loading...` frame, then enters the **alternate
screen** [`Screen`] once the first fetch lands (or immediately when there's a
cache to paint), where that bottom block is **pinned** to the last rows and the
sections scroll under it. Interactive `--once` uses an *inline* `Screen` instead:
a `Loading...` frame while the fetch runs (abortable with `q`), then the dashboard
is left in the terminal. Piped/non-TTY output is plain text printed straight to
stdout, so the dashboard stays pipe-friendly and URLs can be OSC-8 hyperlinks.

## Golden rules

- **Transport is the native GitHub API over HTTP** (`ureq` + rustls), not the
  `gh` CLI. `github::Client` sends a Bearer token with a User-Agent +
  `X-GitHub-Api-Version`. GraphQL is a `POST /graphql` with `{query, variables}`;
  parse the full `{"data":...}` envelope (`github::parse_graphql`, surfacing
  GraphQL `errors`). REST is `GET /<path>`.
- **Auth** lives in `auth.rs`: token resolution is `PROWL_TOKEN` → `GITHUB_TOKEN`
  → OS keyring / chmod-600 file → OAuth **device flow** (interactive). The OAuth
  App client id is public and embedded. `--login` forces the device flow.
- **The terminal toolkit is `uncurses`** (the author's own low-level library):
  its `style::Style` carries SGR + the OSC-8 link, the `Screen` facade owns raw
  mode / the alternate screen / input / teardown, and `text` provides width math.
  Don't reach for a higher-level TUI framework (ratatui, etc.): the watch is a
  full-repaint dashboard, and one-shot output must degrade to plain piped text.
- **Styling:** built on `uncurses::style::Style` (SGR incl. 24-bit truecolor;
  OSC-8 links ride in the style). There is **one painter**: the dashboard is
  drawn straight onto an `uncurses` surface with `set_str`. Plain-vs-styled is
  not a code branch — the surface's color `Profile` downsamples at encode/render
  time, and `Profile::Disabled` (non-TTY/piped) drops SGR and hyperlinks, so
  piped output is plain automatically. Glyph-vs-letter and bar-vs-parens are the
  one content choice, driven by an `ascii` flag (`--ascii`, or a `Disabled`
  profile).
- **One status palette.** Colors and glyphs live only in `status.rs` (Catppuccin
  Mocha + Nerd Font), as `uncurses::color::Color` constants. Don't redefine them.

## Layout (lib + thin bin)

`src/main.rs` is a thin binary calling `prowl::run()`. `src/lib.rs` orchestrates
(painting the dashboard onto a surface, encoding the one-shot frame, and the
watch event loop); everything else is testable modules:

- `cli.rs` — clap derive CLI, `Section` enum, `View` (Mine/Reviews, `--view`,
  `.toggle()`), `ReviewScope` (Direct/All, `--review-scope`, `.qualifier()`),
  duration parser (`s/m/h/d/w`), and the `WATCH_KEYS` `after_help` block
  documenting the interactive watch-mode keys.
- `github.rs` — `Client` (HTTP `graphql()`/`get()`), `Repo`, `me()`,
  `default_branch()`, `detect_repo()` (parses the git `origin` remote),
  `parse_graphql()`.
- `auth.rs` — device-flow login + token storage (keyring/file).
- `model.rs` — serde structs + `fetch_*` for the queries; query strings. Covers
  the three Mine queries plus the Reviews view: `REVIEWS_QUERY` (one POST with
  two aliased searches, `requested:` + `reviewed:`) and `fetch_reviewed_merged`
  (reuses `merged_query`, now carrying `author`).
- `status.rs` — **the** palette: `Status`, `status_style` (returns a glyph +
  `Color`), glyphs/ASCII, `derive_status` (precedence), `fail_count`; the
  `mergeStateStatus` helpers `state_style`, `state_label` (DIRTY → CONFLICTS),
  `state_glyph`, `state_meaning`; and the Reviews-view `ReviewState` (Awaiting/
  ReReview/Updated/Reviewed) with `review_style`/`review_glyph`/`review_ascii`/
  `review_meaning` and `REVIEW_ORDER`. `fg(Color)` builds the foreground `Style`.
- `render.rs` — the surface painters: `paint_table`/`paint_header`/`paint_dim`/
  `paint_footer`/`paint_tabs`/`paint_search_prompt`/`paint_help` write onto any
  `&mut impl TextSurface` using the surface's own `str_width` (no in-house width
  math) and `set_str` (column gaps are implicit — unpainted cells stay blank, so
  no padding is emitted). `Cell` (text + `Style`, the OSC-8 link folded into the
  style) / `Table`, `truncate` (uncurses' width-aware truncator), and
  `title_width` (cap/align the shared `TITLE` column so every table lines up and
  the whole view stays within `MAX_WIDTH` = 120). Headers (with an optional dim
  count badge and trailing note — the queue ETA), the `tabs` view-switcher strip,
  the leading-column markers (`change_marker`, and the `select_marker` navigation
  caret that overrides it on the selected row), the key-hint footer (carrying the
  refresh interval and the `enter open` / `/ search` hints), the search prompt
  line (the `/` query + match count; it paints no cursor and instead *returns*
  the caret cell, so the watch can park the terminal's real one there), and the
  help legend
  (`paint_help(view, …)` — a movement-keys line then, contextual: status glyphs +
  every `STATE` value for Mine, review glyphs + the merged glyph for Reviews)
  live here too, plus `render_table` (paint one table to a string, for tests).
  It also owns the watch frame's geometry: `compose(screen, body, bottom, rows,
  caret)` fills exactly `rows` rows — as much of the body as fits at the top,
  blank padding, then the bottom block glued to the last rows — and returns the
  row that block starts on. When the body is taller than the space left over it
  scrolls, keeping `caret` (the row a view reported painting its selection caret
  on) centered; when the bottom block alone overflows it keeps its head, so the
  footer survives and the help legend is what gets cut. The body is drawn through
  a `uncurses::buffer::View`, which clips without translating, so blitting it maps
  the first visible body row onto the top of the screen.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
  Each row's PR number is the OSC-8 link (no separate URL column); the queue
  columns are `# PR TITLE AUTHOR WAIT BUILD` (author truncated to
  `AUTHOR_WIDTH`), where `WAIT` is how long the entry has been queued (now −
  `enqueuedAt`) and `BUILD` is how long its speculative merge commit has been
  building — now − the earliest check-run `startedAt` in the commit's
  `statusCheckRollup.contexts` (`QueueEntryNode::build_started_at`), or `—` until
  a check actually starts running (still queued, or no speculative commit /
  checks). The rollup is a single flat connection (cheap, and front-loads the
  real check runs, unlike `checkSuites` whose first entries are app
  integrations). The `Merge Queue` header also carries the queue-level ETA
  (`~11m to merge`, from `mergeQueue.nextEntryEstimatedTimeToMerge`) as a dim
  note. The
  merged columns are `# PR TITLE RELEASE MERGED`, where `RELEASE` is the release
  that shipped the PR (a link to its release page) or `—` if not yet shipped,
  looked up from the `commits::ReleaseMap`.
- `reviews.rs` — the Reviews view's rows/tables. `ReviewRow` (open: `glyph PR
  TITLE AUTHOR UPDATED`, glyph = the `ReviewState`) via `build_open_rows`
  (de-dupes the two searches, derives the state, sorts by state rank then
  `updatedAt`) + `open_to_table`; `ReviewedMergedRow` (`glyph PR TITLE AUTHOR
  MERGED`) via `build_merged_rows` + `merged_to_table`.
- `commits.rs` — "commits by me" counts for the next (unreleased) version and
  the last 4 stable releases (GitHub releases + compare REST APIs); best-effort,
  never fatal. `fetch` returns both the `CommitStats` (rendered as the "My
  Shipments" section: one left-aligned labelled count per bucket, each label a
  link — `upcoming` to the compare log (last tag → default branch), each release
  tag to its release page, with each shipped release's relative publish age in a
  trailing dim column) and a `ReleaseMap` (PR number → the release that
  shipped it, parsed from each commit subject's trailing `(#NNN)`, the squash /
  merge-commit convention) that annotates the merged section's `RELEASE` column.
  `--include-pre-releases` also counts prereleases (drafts are always skipped).
- `changes.rs` — `Tracker`/`Changes`: bell + highlight detection (Mine view).
- `nav.rs` — watch-mode row navigation + search: `targets(view, &Sections,
  query)` is the open URL of every navigable row matching `query` in render
  order (PR rows → the PR; shipments → the release / compare log; url-less rows
  skipped), `filter(&Sections, query)` clones the matching rows for rendering
  (same per-row haystack — number/title/author/tag — so rows and targets stay in
  lockstep), `moved` advances the selection cursor by a `nav::Move` (the
  input-agnostic movement type — `lib.rs::classify` maps keys onto it; lazy:
  `None` until the first move, `Bottom` enters at the last row), and `clamp`
  keeps it in range after a refresh.
- `open.rs` — `open::url` opens a URL in the default browser via the platform
  opener (`open` / `xdg-open` / `cmd /C start`), spawned detached; rejects
  non-`http(s)` URLs; no new dep.
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `timefmt.rs` — `chrono` helpers (local clock, `mergedAt` ages, since-date).

`run()` first creates a `uncurses::terminal::Terminal::stdio()`; interactivity is
its `is_terminal().1` (output a TTY?). When the output is **not** a TTY (piped,
redirected), `render_once` paints the dashboard onto an offscreen `TextBuffer`
sized to its content (a generous `height_bound` + `bottom_bound`, then cropped to
the painted height), and `encode_with`s it to the terminal's output (`Terminal::output`)
using the **detected** color `Profile` (`Profile::detect_from`), so it's colored on
a TTY and plain when piped. Interactive `--once` instead runs `run_once_interactive`:
an *inline* `Screen` (raw mode, hidden cursor) shows a `Loading...` frame while the
fetch runs on a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C`
aborts mid-fetch; on success the dashboard replaces the frame and is left inline
(`Screen::finish` doesn't wipe an inline surface). Otherwise the same `Terminal` is
moved into `App::start` → `Screen::new(terminal)`. The watch redraw and the inline
one-shot frame share `render_dashboard`, which has two layouts: **pinned** (the
watch, in the alternate screen) fills the terminal, scrolls the body under a
bottom block glued to the last rows, and **unpinned** (the inline one-shot) sizes
the surface to the content and crops to the painted height.

The interactive watch is `lib.rs::App`, following the uncurses example **`App`
pattern**: the struct owns the `uncurses::Screen` plus all dashboard state, and
`run()` does `let mut app = App::start(terminal, ...)?; let result = app.run();
app.stop()?; result`. `start` builds the screen from the `Terminal` and brings it
up (raw mode, hidden cursor, keeping the terminal's detected color profile), then
paints the startup frame: a cached dashboard if one exists (entering the alt screen
straight away), otherwise an **inline** `Loading...` frame. `run` resolves
`me`/default branch then loops fetch → paint → wait, returning `Ok(())` on a quit
key. The first live paint calls `enter_alt` (once), which drops the inline frame to
zero rows and switches to the alt screen — so loading looks like ordinary command
output before the dashboard takes over the screen. `stop` consumes the app and calls
**`Screen::finish`** (the idiomatic teardown: exit alt-screen, show cursor, leave
raw mode). Because the caller always runs `stop`, the terminal is restored on
every path — a clean quit, a `?`-operator error, or a failed first paint (`start`
calls `stop` itself before bailing). Each frame is painted by `redraw` →
`render_dashboard`, which pins the frame once `in_alt` is set: `autoresize` fits
the managed area to the whole terminal, `paint_body` and `paint_bottom` each paint
into their own `TextBuffer`, and `render::compose` places them. The loop uses `poll_event` with
the interval as the timeout. Keys are classified into an `Action` (or, while the
search prompt is open, a `SearchAction`) with `Key::matches`, which is
**case-sensitive** — bindings must list both cases (`["r", "R"]`). `r`/`R`
refresh now, `Tab` switches view, `?` toggles help, `/` opens search, `Enter`
opens the selected row, the movement keys drive the cursor, `q`/`Q`/`Ctrl-C`
quit (`Esc` clears the filter, or quits when there is none), `Ctrl-Z`
suspends/resumes, `Resize` repaints. All watch UI state lives in one `Ui` struct
(view, help, selection, search).

## Key behaviors

- **Status precedence:** `merged > conflicts > fail > pending > pass > none`.
  Check suites with **zero check runs** (`checkRuns.totalCount == 0`) are
  phantom and ignored for both the glyph and the `FAIL` count, matching GitHub's
  rollup (so a `CLEAN` PR stays green).
- **Sorting:** open PRs by `updatedAt` desc, merged PRs by `mergedAt` desc;
  queue by `position` asc. Reviews by review-state rank (Awaiting → ReReview →
  Updated → Reviewed) then `updatedAt` desc; reviewed-and-merged by `mergedAt` desc.
- **Queue dedup:** a PR of mine that's in the merge queue is shown only in the
  Merge Queue section, not the open-PRs list (`prs::without_queued`, applied when
  the queue section is shown so `--only mine` still lists it).
- **Views / Tab:** two views, `Mine` (default) and `Reviews`, selected for
  one-shot output with `--view` and toggled live with `Tab`. While watching,
  prowl fetches **both** views every refresh so Tab switches instantly from
  `last_good` (no refetch); `--once`/piped fetches only the selected view. A
  top tab strip marks the active view.
- **Review state:** each open review row is `Awaiting` (requested, not yet
  reviewed by me), `ReReview` (requested again after I reviewed), `Updated` (I
  reviewed; last commit `committedDate` > my latest review `submittedAt`), or
  `Reviewed`. `--review-scope` picks the requested search: `all` →
  `review-requested:<me>` (me + my teams, default), `direct` →
  `user-review-requested:<me>` (only me). Both review searches exclude my own
  PRs (`-author:<me>`).
- **Bell:** rings once per refresh when a PR of mine merges or an open PR's
  status changes (keyed by PR number, so re-sorting / new PRs / title edits do
  not ring). The first refresh is silent. Changed rows get a `▸` marker. Bell
  and change markers track the **Mine** view only (the Reviews view conveys
  state through its per-row glyph instead).
- **Resilience:** a failed API call keeps the last good data, shows a dim error
  line, and does not ring.
- **Navigation / open:** a lazy selection cursor (`nav`, watch only) — `None`
  until the first movement key, then a `select_marker` caret on the chosen row
  (it overrides the change marker, and works in the custom shipments painter
  too). `j`/`k` (or the arrows) move one row, `g`/`G` jump to first/last,
  `Ctrl-D`/`Ctrl-U` half a page (sized from the screen's `window_cells`); Enter
  opens the selected row — the PR, or a shipments release / the upcoming compare
  log — via `open::url`. Every row across all sections of the active view is one
  target (`nav::targets`, in render order); switching views drops the cursor and
  a refresh `clamp`s it. `--once`/piped output has no selection.
- **Search / filter:** `/` opens a search prompt (`Ui.searching`); typing filters
  the rows live (case-insensitive substring over number/title/author/release
  tag), Enter applies the filter and returns to the list, Esc (or a lone Esc from
  the list) clears it — and with no filter to clear, Esc quits. While the prompt
  is open every keystroke is text (`classify_search`), else keys are normal-mode
  actions (`classify`). `nav::filter` produces the rendered rows and
  `nav::targets(…, query)` the navigable ones from the **same** predicate, so the
  caret/open track the visible matches; the selection resets on each edit. The
  prompt uses the **terminal's own cursor**: `paint_search_prompt` returns the
  caret cell, `paint_dashboard` passes it up only while `searching`, and
  `render_dashboard` stages it with `Screen::set_cursor_position` (declarative —
  `render` re-applies it every frame) or `clear_cursor_position`. `App` tracks
  `cursor_shown` and only calls `show_cursor`/`hide_cursor` on a transition,
  since both always emit DECTCEM. Watch mode only.
- **Cache:** on a watch start, prowl paints the cached `Sections` immediately
  (entering the alt screen straight away), seeds change-detection from it
  so the first live refresh highlights what changed while prowl wasn't running,
  but stays silent (no startup bell). With no cache it shows an inline
  `Loading...` frame and enters the alt screen only once the first fetch lands.
  `--no-cache` skips both read and write.
- **Terminal:** the watch runs on a `uncurses::Screen` in the alternate screen
  with the cursor hidden (it reappears only in the search prompt); raw mode means stray keystrokes never garble the
  dashboard or spill into the shell. `r`/`R` forces a refresh now; `Tab` switches
  view; `?` toggles the help legend (contextual to the active view — status
  glyphs + `STATE` values for Mine, review glyphs for Reviews — hidden by
  default, rendered last at the very bottom; `--no-help` only affects
  one-shot/piped output). The movement keys (`j`/`k`, arrows, `g`/`G`,
  `Ctrl-D`/`Ctrl-U`) drive the selection cursor, Enter opens it, and `/` filters.
  `q`/`Q`/`Ctrl-C` quit (as does `Esc` with no filter applied) and `Ctrl-Z`
  suspends/resumes. The bottom block — search prompt, error line, footer, help
  legend — is **pinned** to the last rows of the screen (`render::compose`), and
  the sections scroll under it, following the selection. The only persistent
  bottom line is the footer
  (`r refresh (every 5m) - tab switch view - enter open - / search - ? help`),
  which carries the refresh interval; a failed refresh adds a dim `error: …` line
  above it. While a fetch is in flight the footer reads `r refreshing` with the
  `r` glyph dimmed. Every fetch (and the one-time `me`/default-branch resolution)
  runs on a **detached background thread** and returns over a channel; the main
  thread only polls input and paints, so network I/O never blocks the UI —
  navigation, search, `Tab`, `?`, resize and suspend stay live mid-refresh and
  **quit is instant** (a quit abandons the in-flight request, which is reaped at
  process exit). The terminal is restored on every exit path by `App::stop`
  (`Screen::finish`), which the caller always runs after `App::run`.
- **Interactive `--once`:** `run_once_interactive` brings up an *inline* `Screen`
  (raw mode, hidden cursor) and paints a `Loading...` frame while the fetch runs on
  a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C` aborts the
  fetch instantly. On success the dashboard replaces the frame and is left inline in
  the terminal; on abort the frame is wiped. `Screen::finish` restores the terminal
  on every path. Piped/non-TTY output keeps the plain `render_once` encode path.

## The GraphQL queries + REST (see `model.rs` / `commits.rs`)

- Merge queue: `repository.mergeQueue.entries` (vars `owner`, `name`), each
  entry carrying `enqueuedAt` (WAIT) and `headCommit.statusCheckRollup.contexts`
  check-run `startedAt` timestamps (BUILD = now − the earliest), plus the
  queue-level `nextEntryEstimatedTimeToMerge` (the header ETA).
- Open PRs: `search(is:pr is:open author:<me>)` with `mergeable`,
  `mergeStateStatus`, `mergeQueueEntry`, last commit `checkSuites { conclusion
  checkRuns { totalCount } }`, `updatedAt`.
- Merged: `search(is:pr is:merged author:<me> merged:>=<since>)` with `mergedAt`
  (fetched `sort:updated-desc`, since search can't sort by merge time, then
  re-sorted by `mergedAt` for display). Now also fetches `author` (used by the
  reviewed-and-merged section; the Mine merged section ignores it).
- Reviews (one POST, two aliased searches): `requested: search(is:pr is:open
  <scope>:<me> -author:<me>)` and `reviewed: search(is:pr is:open
  reviewed-by:<me> -author:<me>)`, each node carrying `author`, last commit
  `committedDate`, and `reviews(author:<me>)` `submittedAt`s. Re-review = a PR
  in both result sets.
- Reviewed & merged: `search(is:pr is:merged reviewed-by:<me> -author:<me>
  merged:>=<since>)` (reuses the merged query/limit).
- Commits section: REST `GET /repos/.../releases`, `/compare/a...b`, `/commits`.

## Build / test / lint

```sh
cargo build                                  # must be warning-free
cargo clippy --all-targets -- -D warnings    # must be clean
cargo fmt --all --check                      # must be formatted
cargo test                                   # offline, fixture-based
```

`lib.rs` opts the crate into `#![warn(clippy::pedantic)]` with a curated block of
`#![allow(...)]`s (each justified) for the lints that are noise for a small
bin-plus-test-lib — so `clippy -D warnings` still runs pedantic and new pedantic
findings fail CI.

CI (`.github/workflows/build.yml`) runs fmt/clippy/build/test (the `build` job)
and `cargo audit` for dependency advisories (the `audit` job) on push and PRs.

## Releases

Tag `vX.Y.Z` → `.github/workflows/release.yml` runs **GoReleaser Pro**
(`.goreleaser.yaml`). The config `includes:` shared snippets from
[`caarlos0/goreleaserfiles`](https://github.com/caarlos0/goreleaserfiles)
(changelog/release, notarization, packaging) and publishes: archives, nfpm/nix/
homebrew-cask packages, the npm package `@caarlos0/prowl`, SBOMs, and a
cosign-signed checksum. `snapshot.yml` builds a snapshot on pushes/same-repo PRs.
Required secrets: `GORELEASER_KEY`, `GH_PAT` (repo scope, for tap/nur pushes),
`NPM_TOKEN`; `MACOS_*` enable optional macOS notarization.

Tests are offline: JSON fixtures under `tests/fixtures/` (real captures + a
crafted queue) drive parsing → rows → render in `tests/parsing.rs`, plus
per-module unit tests. No network in tests.

## Conventions

Conventional Commits with scope, one logical change per commit, signed off
(`git commit -s`). Merge (never rebase) when integrating `main`. Keep it simple;
verify before fixing. See `CONTRIBUTING.md`.
