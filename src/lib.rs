//! prowl — watch a repo's open PRs, merge queue, and recently merged PRs.
//!
//! The crate is split into a small library (this file plus its modules) and a
//! thin binary so the parsing/rendering/change-detection logic can be exercised
//! by offline, fixture-based tests under `tests/`.

#![warn(clippy::pedantic)]
// Pedantic lints that are noise for this small binary crate. Its `pub` items
// exist so the offline fixture tests can reach them, not as a stable public API,
// so most "document/annotate the public surface" lints don't apply.
#![allow(clippy::must_use_candidate)] // internal API; blanket #[must_use] is noise
#![allow(clippy::return_self_not_must_use)] // same, for builder-style methods
#![allow(clippy::missing_errors_doc)] // anyhow Results; the failure modes are self-evident
#![allow(clippy::missing_panics_doc)] // the only panics are non-poisonable mutex locks
#![allow(clippy::struct_excessive_bools)] // clap flag structs are naturally bool-heavy
#![allow(clippy::struct_field_names)] // serde structs mirror GitHub's JSON field names
#![allow(clippy::implicit_hasher)] // internal HashSet params use the one default hasher
#![allow(clippy::needless_pass_by_value)] // by-value serde_json::Value is the ergonomic form
#![allow(clippy::needless_raw_string_hashes)]
// `r#"…"#` is the convention for query blocks
// The few numeric casts are bounded/guarded (surface rows, non-negative display
// seconds); the one size-sensitive calc — the duration parser — uses checked_mul.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::duration_suboptimal_units)] // tests spell durations in seconds on purpose

pub mod auth;
pub mod cache;
pub mod changes;
pub mod cli;
pub mod commits;
pub mod github;
pub mod merged;
pub mod model;
pub mod nav;
pub mod open;
pub mod prs;
pub mod queue;
pub mod render;
pub mod reviews;
pub mod status;
pub mod timefmt;

use anyhow::{Context, Result};
use changes::{Changes, Tracker};
use clap::Parser;
use cli::{Cli, View};
use github::{Client, Repo};
use std::io::Write;
use std::time::{Duration, Instant};
use uncurses::buffer::{Bounded, SurfaceMut, TextBuffer};
use uncurses::color::{Color, Profile};
use uncurses::event::{Event, KeyCode, KeyModifiers};
use uncurses::screen::Screen;
use uncurses::style::Style;
use uncurses::terminal::{Stdin, Stdout, Terminal};
use uncurses::text::{Encode, TextSurface};

/// A fetched snapshot of every enabled section (`None` = section disabled).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    /// Queue-level estimate: seconds until a newly added entry would merge.
    queue_next_eta: Option<i64>,
    prs: Option<Vec<prs::PrRow>>,
    commits: Option<commits::CommitStats>,
    /// Reviews view: open PRs awaiting / under my review.
    reviews: Option<Vec<reviews::ReviewRow>>,
    /// Reviews view: merged PRs I reviewed.
    reviewed_merged: Option<Vec<reviews::ReviewedMergedRow>>,
}

impl Sections {
    /// Every section disabled — painted as just the bottom (error/footer/help)
    /// when a fetch fails before any data has arrived.
    const EMPTY: Sections = Sections {
        merged: None,
        queue: None,
        queue_next_eta: None,
        prs: None,
        commits: None,
        reviews: None,
        reviewed_merged: None,
    };
}

/// Fetch the sections for the requested views. `want_mine` covers the Mine view
/// (open PRs, queue, merged, shipments, honoring `--only`); `want_reviews`
/// covers the Reviews view (PRs to review, reviewed-and-merged). In watch mode
/// both are fetched so Tab can switch instantly; `--once` fetches just one.
fn fetch(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &str,
    want_mine: bool,
    want_reviews: bool,
) -> Result<Sections> {
    // Release data powers both the "My Shipments" counts and the merged
    // "RELEASE" column, so fetch it once when either section is shown.
    // Best-effort: a failure (no releases, empty repo, ...) degrades to an
    // "unavailable" shipments line and blank release cells rather than taking
    // down the whole dashboard.
    let (commit_stats, release_map) = if want_mine && (cli.show_shipments() || cli.show_merged()) {
        commits::fetch(client, repo, me, default_branch, cli.include_pre_releases).ok()
    } else {
        None
    }
    .unwrap_or_else(|| {
        (
            commits::CommitStats::unavailable(),
            commits::ReleaseMap::new(),
        )
    });

    let merged = if want_mine && cli.show_merged() {
        let since = timefmt::since_date(&cli.merged_window);
        let nodes = model::fetch_merged(client, repo, me, &since, cli.merged_limit)?;
        Some(merged::build_rows(nodes, cli.merged_limit, &release_map))
    } else {
        None
    };
    let (queue, queue_next_eta) = if want_mine && cli.show_queue() {
        let (nodes, eta) = model::fetch_queue(client, repo)?;
        (Some(queue::build_rows(nodes, me)), eta)
    } else {
        (None, None)
    };
    let prs = if want_mine && cli.show_mine() {
        let rows = prs::build_rows(model::fetch_my_prs(client, repo, me)?);
        // A queued PR is shown in the Merge Queue section; drop it from the
        // open-PRs list so it isn't listed twice. Keep it when the queue is
        // hidden (e.g. `--only mine`) so it doesn't disappear entirely.
        let rows = if cli.show_queue() {
            prs::without_queued(rows)
        } else {
            rows
        };
        Some(rows)
    } else {
        None
    };
    let commits = (want_mine && cli.show_shipments()).then_some(commit_stats);

    // Reviews view: PRs awaiting / under my review, plus merged PRs I reviewed.
    let (reviews, reviewed_merged) = if want_reviews {
        let data = model::fetch_reviews(client, repo, me, cli.review_scope.qualifier())?;
        let open = reviews::build_open_rows(data);
        let since = timefmt::since_date(&cli.merged_window);
        let merged_nodes =
            model::fetch_reviewed_merged(client, repo, me, &since, cli.merged_limit)?;
        let merged_reviews = reviews::build_merged_rows(merged_nodes, cli.merged_limit);
        (Some(open), Some(merged_reviews))
    } else {
        (None, None)
    };

    Ok(Sections {
        merged,
        queue,
        queue_next_eta,
        prs,
        commits,
        reviews,
        reviewed_merged,
    })
}

/// Synthetic dashboard data for `--demo` (screenshots): no auth, repo, or
/// network. Times are relative to now so the ages look fresh. Temporary.
#[cfg(feature = "demo")]
fn demo_sections() -> Sections {
    use chrono::{SecondsFormat, Utc};
    let ago = |secs: i64| {
        (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
    };
    let pr = |number, is_draft, title: &str, status, merge_state: &str, fail, secs| prs::PrRow {
        number,
        is_draft,
        title: title.to_string(),
        status,
        merge_state: Some(merge_state.to_string()),
        queue: None,
        fail,
        url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
        updated_at: Some(ago(secs)),
    };
    use status::Status::*;
    let prs = vec![
        pr(
            128,
            false,
            "feat(render): truecolor status palette",
            Some(Pass),
            "CLEAN",
            0,
            300,
        ),
        pr(
            127,
            false,
            "fix(term): restore cursor on SIGTSTP",
            Some(Fail),
            "BLOCKED",
            2,
            1080,
        ),
        pr(
            125,
            false,
            "perf(cache): paint from disk on startup",
            Some(Pending),
            "UNSTABLE",
            0,
            2400,
        ),
        pr(
            124,
            true,
            "wip: nix flake + home-manager module",
            None,
            "DRAFT",
            0,
            7200,
        ),
        pr(
            120,
            false,
            "chore(deps): bump ureq to 3.x",
            Some(Conflicts),
            "DIRTY",
            0,
            10800,
        ),
    ];

    let qrow =
        |position, number, author: &str, title: &str, mine, wait_secs, build_secs: Option<i64>| {
            queue::QueueRow {
                position,
                number,
                author: author.to_string(),
                title: title.to_string(),
                url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
                mine,
                enqueued_at: Some(ago(wait_secs)),
                build_started_at: build_secs.map(|s| ago(s)),
            }
        };
    let queue = vec![
        qrow(
            1,
            118,
            "caarlos0",
            "feat(queue): inline merge-queue position",
            true,
            720,
            Some(480),
        ),
        qrow(
            2,
            131,
            "dependabot[bot]",
            "build(deps): bump anstyle to 1.1",
            false,
            480,
            Some(180),
        ),
        qrow(
            3,
            117,
            "octocat",
            "docs: clarify the --only flag",
            false,
            300,
            None,
        ),
    ];

    let base = "https://github.com/caarlos0/prowl";
    let rel = |tag: &str| {
        Some(commits::ReleaseRef {
            tag: tag.to_string(),
            url: format!("{base}/releases/tag/{tag}"),
        })
    };
    let mrow = |number, title: &str, secs, release| merged::MergedRow {
        number,
        title: title.to_string(),
        url: format!("{base}/pull/{number}"),
        release,
        merged_at: Some(ago(secs)),
    };
    // Recent merges aren't shipped yet (None); older ones map to a release.
    let merged = vec![
        mrow(119, "feat(status): ignore phantom check suites", 720, None),
        mrow(116, "fix(github): exact-match the remote host", 7200, None),
        mrow(
            112,
            "ci: build a snapshot on pull requests",
            86_400,
            rel("v0.4.0"),
        ),
        mrow(
            108,
            "feat(render): OSC-8 hyperlinks for URLs",
            259_200,
            rel("v0.3.0"),
        ),
    ];

    let bucket = |mine, capped, url: String| commits::Bucket {
        count: commits::Count { mine, capped },
        url,
    };
    let release = |tag: &str, mine, capped, secs| commits::Release {
        tag: tag.to_string(),
        bucket: bucket(mine, capped, format!("{base}/releases/tag/{tag}")),
        published_at: Some(ago(secs)),
    };
    let commits = commits::CommitStats {
        available: true,
        upcoming: Some(bucket(7, false, format!("{base}/compare/v0.4.0...main"))),
        releases: vec![
            release("v0.4.0", 12, false, 432_000),
            release("v0.3.0", 9, false, 1_728_000),
            release("v0.2.0", 31, true, 3_456_000),
            release("v0.1.0", 18, false, 6_048_000),
        ],
    };

    // Reviews-view demo data (shown with `--demo --view reviews`).
    use status::ReviewState;
    let rrow = |number, author: &str, title: &str, state, secs| reviews::ReviewRow {
        number,
        is_draft: false,
        title: title.to_string(),
        author: author.to_string(),
        url: format!("{base}/pull/{number}"),
        state,
        updated_at: Some(ago(secs)),
    };
    let reviews = vec![
        rrow(
            142,
            "octocat",
            "feat(api): paginate the search endpoint",
            ReviewState::Awaiting,
            420,
        ),
        rrow(
            139,
            "hubot",
            "fix(auth): refresh tokens before expiry",
            ReviewState::ReReview,
            1500,
        ),
        rrow(
            133,
            "dependabot[bot]",
            "build(deps): bump rustls to 0.24",
            ReviewState::Updated,
            5400,
        ),
        rrow(
            130,
            "octocat",
            "docs: expand the troubleshooting guide",
            ReviewState::Reviewed,
            9000,
        ),
    ];
    let mrow_rev = |number, author: &str, title: &str, secs| reviews::ReviewedMergedRow {
        number,
        title: title.to_string(),
        author: author.to_string(),
        url: format!("{base}/pull/{number}"),
        merged_at: Some(ago(secs)),
    };
    let reviewed_merged = vec![
        mrow_rev(
            126,
            "hubot",
            "refactor(store): drop the legacy cache path",
            64_800,
        ),
        mrow_rev(
            122,
            "octocat",
            "test(queue): cover the empty queue",
            172_800,
        ),
    ];

    Sections {
        merged: Some(merged),
        queue: Some(queue),
        queue_next_eta: Some(11 * 60),
        prs: Some(prs),
        commits: Some(commits),
        reviews: Some(reviews),
        reviewed_merged: Some(reviewed_merged),
    }
}

/// Paint one PR section onto `s` at row `top`: a counted header (with an optional
/// dim note), then either its table or, when empty, a dim placeholder, then a
/// trailing blank row. Returns the next free row.
#[allow(clippy::too_many_arguments)]
fn paint_section(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: usize,
    note: Option<&str>,
    empty_msg: &str,
    table: Option<&render::Table>,
    title_w: usize,
    ascii: bool,
    top: u16,
) -> u16 {
    let y = render::paint_header(s, title, accent, Some(&count.to_string()), note, ascii, top);
    let y = match table {
        Some(table) => render::paint_table(s, table, title_w, ascii, y),
        None => render::paint_dim(s, empty_msg, y),
    };
    y + 1
}

/// Overwrite the leading marker cell of `table`'s `local`-th row with the
/// navigation caret (a no-op when there's no such table or row).
fn place_caret(table: Option<&mut render::Table>, local: usize, ascii: bool) {
    if let Some(row) = table.and_then(|t| t.rows.get_mut(local))
        && let Some(first) = row.first_mut()
    {
        *first = render::select_marker(ascii);
    }
}

/// The Mine view: My open PRs, Merge Queue, My merged PRs, then My Shipments.
/// Each section always shows its header (with a count); an empty section follows
/// it with a dim placeholder. Returns the next free row.
fn paint_mine(
    s: &mut impl TextSurface,
    sections: &Sections,
    changes: &Changes,
    selected: Option<usize>,
    ascii: bool,
    top: u16,
) -> u16 {
    let mut prs_table = sections
        .prs
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| prs::to_table(rows, ascii, &changes.status_changed));
    let mut queue_table = sections
        .queue
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| queue::to_table(rows, ascii));
    let mut merged_table = sections
        .merged
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| merged::to_table(rows, ascii, &changes.newly_merged));

    // Place the navigation caret: map the global selection onto the section it
    // falls in. Every PR/queue/merged row is navigable, so the local index is
    // the offset past the earlier sections; any remainder indexes the shipments'
    // navigable rows (handled by `paint_commits`).
    let mut ship_sel = None;
    if let Some(sel) = selected {
        let np = sections.prs.as_ref().map_or(0, Vec::len);
        let nq = sections.queue.as_ref().map_or(0, Vec::len);
        let nm = sections.merged.as_ref().map_or(0, Vec::len);
        if sel < np {
            place_caret(prs_table.as_mut(), sel, ascii);
        } else if sel < np + nq {
            place_caret(queue_table.as_mut(), sel - np, ascii);
        } else if sel < np + nq + nm {
            place_caret(merged_table.as_mut(), sel - np - nq, ascii);
        } else {
            ship_sel = Some(sel - np - nq - nm);
        }
    }

    // The shared TITLE width keeps the tables aligned and the view within
    // MAX_WIDTH; pass it to every section so the columns line up.
    let title_w = title_width(s, [&prs_table, &queue_table, &merged_table]);

    let mut y = top;
    if let Some(rows) = &sections.prs {
        y = paint_section(
            s,
            "My open PRs",
            status::LAVENDER,
            rows.len(),
            None,
            "No open PRs.",
            prs_table.as_ref(),
            title_w,
            ascii,
            y,
        );
    }
    if let Some(rows) = &sections.queue {
        // The queue-level ETA (time until a newly added entry would merge) rides
        // alongside the header as a dim note.
        let eta = sections.queue_next_eta.map(|secs| {
            format!(
                "~{} to merge",
                timefmt::eta(Duration::from_secs(secs.max(0) as u64))
            )
        });
        y = paint_section(
            s,
            "Merge Queue",
            status::BLUE,
            rows.len(),
            eta.as_deref(),
            "No merge queue.",
            queue_table.as_ref(),
            title_w,
            ascii,
            y,
        );
    }
    if let Some(rows) = &sections.merged {
        y = paint_section(
            s,
            "My merged PRs",
            status::MAUVE,
            rows.len(),
            None,
            "No recent merged PRs.",
            merged_table.as_ref(),
            title_w,
            ascii,
            y,
        );
    }
    if let Some(stats) = &sections.commits {
        y = paint_commits(s, stats, ship_sel, ascii, y) + 1;
    }
    y
}

/// The Reviews view: PRs to review (with a per-row review-state glyph), then
/// merged PRs I reviewed. Their TITLE columns are aligned together. Returns the
/// next free row.
fn paint_reviews(
    s: &mut impl TextSurface,
    sections: &Sections,
    selected: Option<usize>,
    ascii: bool,
    top: u16,
) -> u16 {
    let mut open_table = sections
        .reviews
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::open_to_table(rows, ascii));
    let mut merged_table = sections
        .reviewed_merged
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::merged_to_table(rows, ascii));

    // The open reviews come first, then the reviewed & merged rows, so a
    // selection index past the open rows indexes the latter.
    if let Some(sel) = selected {
        let nr = sections.reviews.as_ref().map_or(0, Vec::len);
        if sel < nr {
            place_caret(open_table.as_mut(), sel, ascii);
        } else {
            place_caret(merged_table.as_mut(), sel - nr, ascii);
        }
    }

    let title_w = title_width(s, [&open_table, &merged_table]);

    let mut y = top;
    if let Some(rows) = &sections.reviews {
        y = paint_section(
            s,
            "Reviews",
            status::LAVENDER,
            rows.len(),
            None,
            "No PRs to review.",
            open_table.as_ref(),
            title_w,
            ascii,
            y,
        );
    }
    if let Some(rows) = &sections.reviewed_merged {
        y = paint_section(
            s,
            "Reviewed & merged",
            status::MAUVE,
            rows.len(),
            None,
            "No reviewed PRs merged recently.",
            merged_table.as_ref(),
            title_w,
            ascii,
            y,
        );
    }
    y
}

/// The shared TITLE column width across the present tables of one view.
fn title_width<const N: usize>(s: &impl TextSurface, tables: [&Option<render::Table>; N]) -> usize {
    let present: Vec<&render::Table> = tables.into_iter().flatten().collect();
    render::title_width(s, &present)
}

/// Paint the whole dashboard onto `s` starting at row 0: the watch-only tab
/// strip, the active view's sections, then the optional search prompt, `error:`
/// line, footer, and help legend. Rows that changed since the previous refresh
/// (per `changes`) are flagged with a leading marker.
///
/// `footer` is `Some((interval, refreshing))` only while watching — it also
/// gates the tab strip, which is an interactive affordance. `ascii` selects
/// letters/parens over Nerd Font glyphs/bars; colors are written as styles and
/// downsampled by the surface's `Profile` at encode/render time. Returns the
/// number of rows used.
fn paint_dashboard(
    s: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    error: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> u16 {
    let mut y = 0u16;
    if footer.is_some() {
        y = render::paint_tabs(s, ui.view, ascii, y) + 1;
    }
    y = match ui.view {
        View::Mine => paint_mine(s, sections, changes, ui.selected, ascii, y),
        View::Reviews => paint_reviews(s, sections, ui.selected, ascii, y),
    };

    // Bottom: search prompt, error line, footer, help — each separated from the
    // previous part by one blank row (the body's trailing blank serves as the
    // first).
    let mut painted = false;
    if !ui.search.is_empty() || ui.searching {
        let matches = nav::targets(ui.view, sections, &ui.search).len();
        y = render::paint_search_prompt(s, &ui.search, ui.searching, matches, ascii, y);
        painted = true;
    }
    if !error.is_empty() {
        if painted {
            y += 1;
        }
        y = render::paint_dim(s, &format!("error: {error}"), y);
        painted = true;
    }
    if let Some((interval, refreshing)) = footer {
        if painted {
            y += 1;
        }
        y = render::paint_footer(s, interval, refreshing, ascii, y);
        painted = true;
    }
    if ui.show_help {
        if painted {
            y += 1;
        }
        y = render::paint_help(s, ui.view, ascii, y);
    }
    y
}

/// A safe upper bound on the dashboard height, used to size a surface before it
/// is cropped to the painted height.
fn height_bound(s: &Sections, ui: &Ui) -> u16 {
    // Tabs + search + error + footer + slack.
    let mut n = 10usize;
    match ui.view {
        View::Mine => {
            n += s.prs.as_ref().map_or(0, |r| r.len() + 3);
            n += s.queue.as_ref().map_or(0, |r| r.len() + 3);
            n += s.merged.as_ref().map_or(0, |r| r.len() + 3);
            // Header + one label row per bucket (upcoming + each release).
            n += s.commits.as_ref().map_or(0, |c| c.releases.len() + 4);
        }
        View::Reviews => {
            n += s.reviews.as_ref().map_or(0, |r| r.len() + 3);
            n += s.reviewed_merged.as_ref().map_or(0, |r| r.len() + 3);
        }
    }
    if ui.show_help {
        n += render::help_height(ui.view) + 1;
    }
    n as u16
}

/// Paint the one-row `Loading...` startup frame (a single dim line) and render it.
/// Shared by the watch's first paint when there's no cache and by interactive
/// `--once`, so both show the identical loading frame.
fn paint_loading(screen: &mut Screen<Stdin, Stdout>) -> Result<()> {
    screen.resize((screen.width().max(1), 1));
    screen.clear();
    render::paint_dim(screen, "Loading...", 0);
    screen.render()?;
    Ok(())
}

/// Size an inline/alternate `Screen` to the dashboard's content height, paint it,
/// crop to the height actually used, and render. Shared by the watch redraw and
/// the interactive one-shot frame so the sizing dance lives in one place.
fn render_dashboard(
    screen: &mut Screen<Stdin, Stdout>,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    error: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> Result<()> {
    let w = screen.width().max(1);
    // Grow tall enough to paint everything, paint, then shrink to the height
    // actually used so the surface is exactly the dashboard's line count.
    screen.resize((w, height_bound(sections, ui).max(1)));
    screen.clear();
    let used = paint_dashboard(screen, sections, ui, changes, error, footer, ascii);
    screen.resize((w, used.max(1)));
    screen.render()?;
    Ok(())
}

/// Render the dashboard once into an offscreen [`TextBuffer`] sized to its content,
/// then encode it to the terminal's output with the **detected** color profile
/// (plain when piped) and exit. Used by `--once`, non-TTY output, and `--demo`.
fn render_once(
    terminal: &Terminal<Stdin, Stdout>,
    sections: &Sections,
    cli: &Cli,
    changes: &Changes,
    footer: Option<(&str, bool)>,
) -> Result<()> {
    let profile = Profile::detect_from(terminal.env(), terminal.is_terminal().1);
    let ascii = cli.ascii || profile == Profile::Disabled;
    // One-shot output has no interaction: no tabs, no selection, no search; the
    // help legend follows `--no-help` instead of the `?` toggle.
    let ui = Ui::once(cli);

    let w = render::MAX_WIDTH as u16;
    let mut canvas = TextBuffer::new(w, height_bound(sections, &ui));
    let used = paint_dashboard(&mut canvas, sections, &ui, changes, "", footer, ascii);
    canvas.resize(w, used.max(1));

    // A closed downstream pipe (`prowl --once | head`) is a clean exit, not an
    // error worth printing.
    let mut out = terminal.output();
    let write = canvas
        .encode_with(&mut out, profile)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    match write {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Interactive `--once`: bring up an inline `Screen` (raw mode, hidden cursor, no
/// echo) showing a `Loading...` frame while the fetch runs on a background
/// thread, so keystrokes never echo and `q`/`Esc`/`Ctrl-C` can abort mid-fetch.
/// On success the dashboard replaces the loading frame and is left inline in the
/// terminal (like piped `--once`); on abort the frame is wiped and nothing is
/// left behind. `Screen::finish` restores the terminal on every path.
fn run_once_interactive(
    terminal: Terminal<Stdin, Stdout>,
    cli: &Cli,
    client: &Client,
    repo: &Repo,
) -> Result<()> {
    let mut screen = Screen::new(terminal)?;
    screen.init()?;
    screen.hide_cursor()?;

    // Inline loading frame; raw mode swallows keystrokes so nothing echoes into
    // the output while we wait.
    paint_loading(&mut screen)?;

    // Fetch off-thread so `q` stays live during network I/O. `me` and the
    // default branch are resolved here too, so even the first round-trip never
    // blocks the abort key.
    let (cli2, client2, repo2) = (cli.clone(), client.clone(), repo.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fetched = (|| {
            let me = client2.me()?;
            let default_branch = client2
                .default_branch(&repo2)
                .unwrap_or_else(|_| "main".to_string());
            // Only the selected view: `--once` output has no Tab to switch.
            fetch(
                &cli2,
                &client2,
                &repo2,
                &me,
                &default_branch,
                cli2.view == View::Mine,
                cli2.view == View::Reviews,
            )
        })();
        let _ = tx.send(fetched); // ignored if we already aborted (rx dropped)
    });

    // `None` => the user aborted; `Some(result)` => the fetch finished.
    let fetched = loop {
        match rx.try_recv() {
            Ok(result) => break Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                break Some(Err(anyhow::anyhow!("fetch worker stopped unexpectedly")));
            }
        }
        if screen.poll_event(Some(Duration::from_millis(60)))? {
            let mut aborted = false;
            while let Some(ev) = screen.try_read_event() {
                if let Action::Quit = classify(&ev) {
                    aborted = true;
                }
            }
            if aborted {
                break None;
            }
        }
    };

    match fetched {
        Some(Ok(sections)) => {
            // Replace the loading frame with the dashboard, then leave it inline.
            render_dashboard(
                &mut screen,
                &sections,
                &Ui::once(cli),
                &Changes::default(),
                "",
                None,
                cli.ascii,
            )?;
            screen.finish()?;
            if !cli.no_cache {
                cache::save(repo, &sections);
            }
            Ok(())
        }
        Some(Err(e)) => {
            screen.finish()?; // restore the terminal before surfacing the error
            Err(e)
        }
        None => {
            // Aborted: wipe the loading frame so nothing is left behind.
            screen.clear();
            screen.render()?;
            screen.finish()?;
            Ok(())
        }
    }
}

/// Paint the "My Shipments" section onto `s` at row `top`: my commit counts for
/// the next (unreleased) version and the last few stable releases, one labelled
/// row each with the labels right-aligned so the colons and counts line up. Each
/// label links out — the upcoming one to the compare log, each release to its
/// release page — and shipped releases also show how long ago they were
/// published, aligned into a trailing column. Returns the next row.
fn paint_commits(
    s: &mut impl TextSurface,
    stats: &commits::CommitStats,
    selected: Option<usize>,
    ascii: bool,
    top: u16,
) -> u16 {
    if !stats.available {
        return render::paint_dim(s, "Commit stats unavailable.", top);
    }
    let count = |c: &commits::Count| format!("{}{}", c.mine, if c.capped { "+" } else { "" });

    // Total commits by me across everything shown (upcoming + the releases); a
    // `+` if any bucket hit the compare API's window and is a lower bound.
    let (total, capped) = stats
        .upcoming
        .iter()
        .map(|b| &b.count)
        .chain(stats.releases.iter().map(|r| &r.bucket.count))
        .fold((0usize, false), |(n, capped), c| {
            (n + c.mine, capped || c.capped)
        });
    let total = format!("{total}{}", if capped { "+" } else { "" });
    let mut y = render::paint_header(
        s,
        "My Shipments",
        status::TEAL,
        Some(&total),
        None,
        ascii,
        top,
    );

    // Each row: the upcoming (unreleased) version first (no publish age), then
    // the shipped releases newest-first with their relative publish age. A row
    // with a URL renders its label as a link to it.
    let value = |b: Option<&commits::Bucket>| match b {
        Some(b) => count(&b.count),
        None => "\u{2014}".to_string(),
    };
    let mut rows: Vec<(String, Option<String>, String, Option<String>)> = vec![(
        "upcoming".to_string(),
        stats.upcoming.as_ref().map(|b| b.url.clone()),
        value(stats.upcoming.as_ref()),
        None,
    )];
    for r in &stats.releases {
        rows.push((
            r.tag.clone(),
            Some(r.bucket.url.clone()),
            value(Some(&r.bucket)),
            r.published_at.as_deref().map(|p| timefmt::age_of(Some(p))),
        ));
    }

    // Right-align the labels and pad the counts to shared widths, so the colons,
    // counts, and publish ages each line up in a readable column.
    let label_w = rows
        .iter()
        .map(|(l, ..)| s.str_width(l) as usize)
        .max()
        .unwrap_or(0);
    let value_w = rows
        .iter()
        .map(|(.., v, _)| s.str_width(v) as usize)
        .max()
        .unwrap_or(0);

    // The selection index counts only navigable (URL-bearing) rows; the sole
    // url-less row is a commit-less "upcoming", which then shifts the rendered
    // caret row down by one.
    let sel_row = selected.map(|k| if stats.upcoming.is_some() { k } else { k + 1 });

    for (i, (label, url, value, age)) in rows.iter().enumerate() {
        // The first row is the upcoming (unreleased) version; set it apart in
        // italics. The label links to the bucket's log/release page.
        let style = if i == 0 && !ascii {
            Style::new().italic()
        } else {
            Style::new()
        };
        let cell = match url {
            Some(url) => render::Cell::link_styled(label.clone(), url.clone(), style),
            None => render::Cell::styled(label.clone(), style),
        };
        // A 2-column leading gutter holds the caret on the selected row, so the
        // labels stay aligned with or without one.
        if Some(i) == sel_row {
            let caret = render::select_marker(ascii);
            s.set_str((0, y), &caret.text, &caret.style);
        }
        let x = (2 + label_w - s.str_width(label) as usize) as u16;
        let p = s.set_str((x, y), &cell.text, &cell.style);
        let p = s.set_str((p.x, y), &format!(": {value}"), None);
        if let Some(age) = age {
            let x = p.x + (value_w - s.str_width(value) as usize + 3) as u16;
            s.set_str((x, y), age, Style::new().faint());
        }
        y += 1;
    }
    y
}

/// First line of an error, truncated, for the one-line error status.
fn short_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let first = full.lines().next().unwrap_or_default();
    if first.chars().count() > 120 {
        format!("{}\u{2026}", first.chars().take(119).collect::<String>())
    } else {
        first.to_string()
    }
}

/// What a keypress or resize means to the watch loop in normal (non-search) mode.
enum Action {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// `q`/`Ctrl-C`: quit.
    Quit,
    /// `r`/`R`: refresh now.
    Refresh,
    /// `?`: toggle the help legend.
    ToggleHelp,
    /// `Tab`: switch to the other view.
    SwitchView,
    /// `Enter`: open the selected row in the browser.
    Open,
    /// `/`: open the search prompt.
    Search,
    /// `Esc`: clear an applied filter, or quit when there is none.
    Cancel,
    /// A movement key: move the selection cursor.
    Move(nav::Move),
    /// `Ctrl-Z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// A keystroke while the search prompt is open (raw text input, unlike the
/// semantic [`Action`]s of normal mode).
enum SearchAction {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// A printable character to append to the query.
    Char(char),
    /// Backspace: drop the last query character.
    Backspace,
    /// Enter: apply the filter and leave the prompt.
    Enter,
    /// Esc: clear the filter and leave the prompt.
    Esc,
    /// `Ctrl-Z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// Classify an event into a normal-mode [`Action`]. In raw mode the signal keys
/// arrive as ordinary key events, so `ctrl+c`/`ctrl+z` are matched here rather
/// than through signal handlers. `Key::matches` is case-sensitive, so the
/// case-insensitive bindings list both forms.
fn classify(ev: &Event) -> Action {
    match ev {
        Event::KeyPress(k) => {
            if k.matches_any(["q", "Q", "ctrl+c"]) {
                Action::Quit
            } else if k.matches("esc") {
                Action::Cancel
            } else if k.matches_any(["r", "R"]) {
                Action::Refresh
            } else if k.matches("?") {
                Action::ToggleHelp
            } else if k.matches("tab") {
                Action::SwitchView
            } else if k.matches("enter") {
                Action::Open
            } else if k.matches("/") {
                Action::Search
            } else if k.matches("ctrl+z") {
                Action::Suspend
            } else if k.matches_any(["j", "down"]) {
                Action::Move(nav::Move::Down)
            } else if k.matches_any(["k", "up"]) {
                Action::Move(nav::Move::Up)
            } else if k.matches("g") {
                Action::Move(nav::Move::Top)
            } else if k.matches("G") {
                Action::Move(nav::Move::Bottom)
            } else if k.matches("ctrl+d") {
                Action::Move(nav::Move::HalfDown)
            } else if k.matches("ctrl+u") {
                Action::Move(nav::Move::HalfUp)
            } else {
                Action::None
            }
        }
        Event::Resize(ws) => Action::Resize(ws.col, ws.row),
        _ => Action::None,
    }
}

/// Classify an event while the search prompt is open: printable characters
/// extend the query, everything else is an edit/exit key. Quit keys are not
/// bound here — `q` is a searchable character, and Esc closes the prompt.
fn classify_search(ev: &Event) -> SearchAction {
    match ev {
        Event::KeyPress(k) => match k.code {
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CTRL | KeyModifiers::ALT) =>
            {
                SearchAction::Char(c)
            }
            KeyCode::Space => SearchAction::Char(' '),
            KeyCode::Backspace => SearchAction::Backspace,
            KeyCode::Enter => SearchAction::Enter,
            KeyCode::Escape => SearchAction::Esc,
            _ if k.matches("ctrl+z") => SearchAction::Suspend,
            _ => SearchAction::None,
        },
        Event::Resize(ws) => SearchAction::Resize(ws.col, ws.row),
        _ => SearchAction::None,
    }
}

/// What the watch loop should do after handling a batch of input.
enum Flow {
    /// Keep waiting / keep fetching.
    Continue,
    /// `r` was pressed: refresh now.
    Refresh,
    /// A quit key was pressed: leave the loop (the caller tears the screen down).
    Quit,
}

/// The interactive dashboard state threaded through painting and mutated on each
/// keypress. One-shot output uses the inert [`Ui::once`] form.
struct Ui {
    /// Active view; starts at `--view`, toggled with Tab.
    view: View,
    /// Whether the `?` help legend is shown (starts hidden while watching).
    show_help: bool,
    /// Navigation cursor into the active view's (filtered) rows — lazy (`None`
    /// until the user moves it), reset when switching views or changing the
    /// search, clamped when a refresh shrinks the list.
    selected: Option<usize>,
    /// The active search query; empty means no filter is applied.
    search: String,
    /// Whether the search prompt is open and capturing text.
    searching: bool,
}

impl Ui {
    /// The non-interactive form used by `--once` / piped output: the `--view`
    /// sections, the help legend per `--no-help`, no selection and no search.
    fn once(cli: &Cli) -> Ui {
        Ui {
            view: cli.view,
            show_help: !cli.no_help,
            selected: None,
            search: String::new(),
            searching: false,
        }
    }

    /// The sections to paint: `good` filtered by the active search, or `good`
    /// itself when there's no query. `buf` owns the filtered copy if one is made,
    /// so the returned reference stays valid for the caller.
    fn shown<'a>(&self, good: &'a Sections, buf: &'a mut Option<Sections>) -> &'a Sections {
        if self.search.is_empty() {
            good
        } else {
            buf.insert(nav::filter(good, &self.search))
        }
    }
}

/// Entry point: authenticate, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Detect interactivity through uncurses' `Terminal` (is the output half a
    // TTY?) and reuse the very same handle to build the watch `Screen` or to
    // encode the one-shot frame. Auth can drive the interactive device flow
    // whenever there's a terminal.
    let terminal = Terminal::stdio();
    let interactive = terminal.is_terminal().1;

    // `--demo`: render synthetic data once and exit (no auth/repo/network), so
    // the dashboard can be screenshotted. Colored on a TTY, plain when piped.
    #[cfg(feature = "demo")]
    if cli.demo {
        let sections = demo_sections();
        let changes = Changes {
            status_changed: std::collections::HashSet::from([127]),
            newly_merged: std::collections::HashSet::from([119]),
        };
        let next = timefmt::eta(cli.interval.dur);
        return render_once(
            &terminal,
            &sections,
            &cli,
            &changes,
            Some((next.as_str(), false)),
        );
    }

    // Authenticate first (this may run the interactive device flow and print
    // prompts, so it must happen before we enter the alternate screen).
    let token = auth::token(cli.login, interactive)?;
    let client = Client::new(token);

    if cli.login {
        let who = client.me().context("verifying the token")?;
        eprintln!("prowl: authenticated as {who}.");
        return Ok(());
    }

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => github::detect_repo()?,
    };

    // Non-interactive (piped, redirected, not a TTY): a blocking fetch, encode
    // the frame to stdout, and exit. No screen, no loading UI.
    if !interactive {
        let me = client.me()?;
        let default_branch = client
            .default_branch(&repo)
            .unwrap_or_else(|_| "main".to_string());
        // Only the selected view's sections are fetched (you can't Tab in
        // one-shot output).
        let sections = fetch(
            &cli,
            &client,
            &repo,
            &me,
            &default_branch,
            cli.view == View::Mine,
            cli.view == View::Reviews,
        )?;
        if !cli.no_cache {
            cache::save(&repo, &sections);
        }
        return render_once(&terminal, &sections, &cli, &Changes::default(), None);
    }

    // Interactive `--once`: an inline screen shows a `Loading...` frame and
    // swallows input while the fetch runs (abortable with `q`), then leaves the
    // dashboard in the terminal.
    if cli.once {
        return run_once_interactive(terminal, &cli, &client, &repo);
    }

    // Interactive watch, structured as an uncurses `App` (start → run → stop):
    // `stop` always runs, so `Screen::finish` restores the terminal on every
    // path — a clean quit, a `?`-operator error, or a panic-free fall-through.
    let mut app = App::start(terminal, &cli, &client, &repo)?;
    let result = app.run();
    app.stop()?;
    result
}

/// The interactive watch, following the uncurses example `App` pattern: it owns
/// the `Screen` and all dashboard state. `start` brings the terminal up, `run`
/// drives the refresh + event loop (returning `Ok(())` when a quit key is
/// pressed), and `stop` tears it back down with `Screen::finish`. The caller
/// always calls `stop`, so the terminal is restored on every path.
struct App<'a> {
    screen: Screen<Stdin, Stdout>,
    cli: &'a Cli,
    client: &'a Client,
    repo: &'a Repo,
    me: String,
    default_branch: String,
    /// The constant next-refresh ETA shown in the key-hint footer.
    eta: String,
    /// Change-detection baseline and the last successfully fetched sections.
    prev: Option<Tracker>,
    last_good: Option<Sections>,
    /// The interactive dashboard state: view, help visibility, selection, search.
    ui: Ui,
    /// The most recent short error (empty unless a refresh or an open failed),
    /// kept so a `?` toggle or a repaint keeps it on screen.
    last_error: String,
    /// Whether a fetch is in flight, so the footer can say `r refreshing`.
    refreshing: bool,
    /// Whether the bell is armed. The first refresh after a cached start is
    /// silent (it still highlights changes).
    armed: bool,
    /// Whether we've switched from the inline loading frame to the alternate
    /// screen. The watch starts inline and enters the alt screen once the first
    /// fetch lands (or immediately when there's a cache to paint).
    in_alt: bool,
}

impl<'a> App<'a> {
    /// Bring the terminal up (raw mode, hidden cursor) from the supplied
    /// `Terminal` — the screen keeps the terminal's detected color profile. The
    /// loading frame shows **inline**; the alt screen is entered once the first
    /// fetch lands (or immediately when there's a cache to paint), so loading
    /// looks like ordinary command output before the dashboard takes over.
    fn start(
        terminal: Terminal<Stdin, Stdout>,
        cli: &'a Cli,
        client: &'a Client,
        repo: &'a Repo,
    ) -> Result<Self> {
        let mut screen = Screen::new(terminal)?;
        screen.init()?;
        screen.hide_cursor()?;

        let mut app = App {
            eta: timefmt::eta(cli.interval.dur),
            screen,
            cli,
            client,
            repo,
            me: String::new(),
            default_branch: String::new(),
            prev: None,
            last_good: None,
            ui: Ui {
                view: cli.view,
                show_help: false,
                selected: None,
                search: String::new(),
                searching: false,
            },
            last_error: String::new(),
            refreshing: false,
            armed: false,
            in_alt: false,
        };

        // If the very first paint fails, restore the terminal before bailing
        // (`stop` handles both the inline and alt-screen states).
        if let Err(e) = app.paint_startup() {
            let _ = app.stop();
            return Err(e);
        }
        Ok(app)
    }

    /// The initial cache/loading paint, seeding change-detection from the cache
    /// so the first live refresh highlights what changed while prowl was away.
    fn paint_startup(&mut self) -> Result<()> {
        match (!self.cli.no_cache)
            .then(|| cache::load(self.repo))
            .flatten()
        {
            Some(c) => {
                self.prev = Some(Tracker::build(
                    c.sections.prs.as_deref(),
                    c.sections.merged.as_deref(),
                ));
                self.last_good = Some(c.sections);
                // Cached data is real content, so go straight to the alt screen.
                self.enter_alt()?;
                self.redraw(&Changes::default())?;
            }
            None => paint_loading(&mut self.screen)?,
        }
        Ok(())
    }

    /// Switch from the inline loading frame to the alternate screen, once. The
    /// inline frame is dropped to zero rows and flushed first, so taking over the
    /// screen leaves the terminal as it was before prowl ran.
    fn enter_alt(&mut self) -> Result<()> {
        if !self.in_alt {
            self.screen.resize((self.screen.width().max(1), 0));
            self.screen.render()?;
            self.screen.enter_alt_screen()?;
            self.in_alt = true;
        }
        Ok(())
    }

    /// Paint the current dashboard via [`render_dashboard`], drawing the last
    /// good sections (or an empty frame, so a first-fetch error still shows its
    /// error line + footer) with `changes` highlighted.
    fn redraw(&mut self, changes: &Changes) -> Result<()> {
        let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        let mut buf = None;
        let sections = self.ui.shown(good, &mut buf);
        render_dashboard(
            &mut self.screen,
            sections,
            &self.ui,
            changes,
            &self.last_error,
            Some((self.eta.as_str(), self.refreshing)),
            self.cli.ascii,
        )
    }

    /// Drive the watch: loop fetch → paint → wait, returning `Ok(())` when the
    /// user presses a quit key.
    fn run(&mut self) -> Result<()> {
        loop {
            if let Flow::Quit = self.fetch_responsive()? {
                return Ok(());
            }
            if let Flow::Quit = self.wait_interval()? {
                return Ok(());
            }
        }
    }

    /// Tear the terminal back down. The consuming `Screen::finish` is the
    /// idiomatic teardown: it exits the alternate screen, shows the cursor, and
    /// leaves raw mode.
    fn stop(self) -> Result<()> {
        self.screen.finish()?;
        Ok(())
    }

    /// Fetch on a detached background thread while the main thread keeps polling
    /// input, so quit/`?`/resize stay live and no network I/O ever blocks the UI.
    /// The result arrives over a channel; pressing quit returns immediately and
    /// abandons the in-flight request (the thread is reaped at process exit).
    /// `me` and the default branch are resolved here too (once), so even the
    /// first round-trip never freezes input. `r` is ignored — a fetch is already
    /// in flight.
    fn fetch_responsive(&mut self) -> Result<Flow> {
        // The footer says `r refreshing` (with `r` dimmed) for the duration.
        self.refreshing = true;
        self.repaint_last()?;
        let flow = self.fetch_loop();
        self.refreshing = false;
        flow
    }

    /// The fetch + input-poll loop itself; [`Self::fetch_responsive`] wraps it to
    /// keep the `refreshing` footer state balanced on every exit path.
    fn fetch_loop(&mut self) -> Result<Flow> {
        let (cli, client, repo) = (self.cli.clone(), self.client.clone(), self.repo.clone());
        let mut me = self.me.clone();
        let mut default_branch = self.default_branch.clone();
        let resolve = me.is_empty();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let fetched = (|| {
                if resolve {
                    me = client.me()?;
                    default_branch = client
                        .default_branch(&repo)
                        .unwrap_or_else(|_| "main".to_string());
                }
                // Both views every refresh, so Tab switches instantly.
                let sections = fetch(&cli, &client, &repo, &me, &default_branch, true, true)?;
                Ok((me, default_branch, sections))
            })();
            let _ = tx.send(fetched); // ignored if we already quit (rx dropped)
        });

        loop {
            match rx.try_recv() {
                Ok(Ok((me, default_branch, sections))) => {
                    self.me = me;
                    self.default_branch = default_branch;
                    // Cleared before painting, so the result frame already shows
                    // the plain `r refresh` hint again.
                    self.refreshing = false;
                    self.apply(sections)?;
                    return Ok(Flow::Continue);
                }
                Ok(Err(e)) => {
                    self.refreshing = false;
                    self.show_error(e)?;
                    return Ok(Flow::Continue);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(Flow::Continue),
            }
            if self.screen.poll_event(Some(Duration::from_millis(60)))? {
                while let Some(ev) = self.screen.try_read_event() {
                    if let Flow::Quit = self.handle_event(&ev)? {
                        return Ok(Flow::Quit);
                    }
                }
            }
        }
    }

    /// Wait out the refresh interval, staying responsive: `r` refreshes now, `?`
    /// toggles help, quit/suspend/resize are honored, other keys are discarded.
    fn wait_interval(&mut self) -> Result<Flow> {
        let deadline = Instant::now() + self.cli.interval.dur;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if !self.screen.poll_event(Some(remaining))? {
                break; // timed out: scheduled refresh
            }
            while let Some(ev) = self.screen.try_read_event() {
                match self.handle_event(&ev)? {
                    Flow::Quit => return Ok(Flow::Quit),
                    Flow::Refresh => return Ok(Flow::Continue), // refresh now
                    Flow::Continue => {}
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Apply an input event's side effects (navigation, view switch, search,
    /// open, suspend, help toggle, resize repaint) and report the control flow it
    /// implies. While the search prompt is open every keystroke is text, so it is
    /// routed to [`Self::handle_search_event`] instead.
    fn handle_event(&mut self, ev: &Event) -> Result<Flow> {
        if self.ui.searching {
            return self.handle_search_event(ev);
        }
        Ok(match classify(ev) {
            Action::Quit => Flow::Quit,
            Action::Refresh => Flow::Refresh,
            Action::Suspend => {
                self.suspend()?;
                Flow::Continue
            }
            Action::ToggleHelp => {
                self.ui.show_help = !self.ui.show_help;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::SwitchView => {
                // Selection indices don't carry across views, so start fresh.
                self.ui.view = self.ui.view.toggle();
                self.ui.selected = None;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::Search => {
                self.ui.searching = true;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::Cancel => {
                // Esc clears an applied filter; with none to clear, it quits.
                if self.ui.search.is_empty() {
                    Flow::Quit
                } else {
                    self.ui.search.clear();
                    self.ui.selected = None;
                    self.repaint_last()?;
                    Flow::Continue
                }
            }
            Action::Open => {
                self.open_selected()?;
                Flow::Continue
            }
            Action::Move(m) => {
                let len = self.target_count();
                let next = nav::moved(m, self.ui.selected, len, self.half_page());
                if next != self.ui.selected {
                    self.ui.selected = next;
                    self.repaint_last()?;
                }
                Flow::Continue
            }
            Action::Resize(w, h) => {
                self.screen.resize((w, h));
                self.repaint_last()?;
                Flow::Continue
            }
            Action::None => Flow::Continue,
        })
    }

    /// Apply a keystroke while the search prompt is open. Typing filters live
    /// (resetting the cursor), Enter applies the filter and closes the prompt,
    /// Esc clears the filter and closes it.
    fn handle_search_event(&mut self, ev: &Event) -> Result<Flow> {
        match classify_search(ev) {
            SearchAction::Char(c) => {
                self.ui.search.push(c);
                self.ui.selected = None;
            }
            SearchAction::Backspace => {
                self.ui.search.pop();
                self.ui.selected = None;
            }
            SearchAction::Enter => self.ui.searching = false,
            SearchAction::Esc => {
                self.ui.search.clear();
                self.ui.searching = false;
            }
            SearchAction::Suspend => return self.suspend().map(|()| Flow::Continue),
            SearchAction::Resize(w, h) => self.screen.resize((w, h)),
            SearchAction::None => return Ok(Flow::Continue),
        }
        self.repaint_last()?;
        Ok(Flow::Continue)
    }

    /// Suspend to the shell (Ctrl-Z) and repaint on resume — the canvas may not
    /// survive the stop, so don't rely on `resume`'s flush. `SIGTSTP` is Unix
    /// job control, so elsewhere Ctrl-Z just repaints.
    fn suspend(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.screen.suspend()?;
            self.screen.resume()?;
        }
        self.repaint_last()
    }

    /// How many rows the selection cursor can visit in the active view, with the
    /// current filter applied.
    fn target_count(&self) -> usize {
        self.last_good
            .as_ref()
            .map_or(0, |s| nav::targets(self.ui.view, s, &self.ui.search).len())
    }

    /// The half-page movement step: half the terminal window's rows.
    fn half_page(&self) -> usize {
        self.screen
            .window_cells()
            .map_or(10, |s| usize::from(s.height / 2).max(1))
    }

    /// Open the selected row's URL in the browser. A failure becomes the dim
    /// error line; a no-op (no selection, no data) leaves the screen as is.
    fn open_selected(&mut self) -> Result<()> {
        let Some(sel) = self.ui.selected else {
            return Ok(());
        };
        let url = self.last_good.as_ref().and_then(|good| {
            nav::targets(self.ui.view, good, &self.ui.search)
                .get(sel)
                .map(|u| (*u).to_string())
        });
        let Some(url) = url else { return Ok(()) };
        if let Err(e) = open::url(&url) {
            self.last_error = format!("open failed: {e}");
            self.repaint_last()?;
        }
        Ok(())
    }

    /// Render a successful fetch: diff against the previous snapshot, paint, ring
    /// the bell on a change (once armed), and cache the result.
    fn apply(&mut self, sections: Sections) -> Result<()> {
        let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
        let changes = self
            .prev
            .as_ref()
            .map(|p| tracker.diff(p))
            .unwrap_or_default();
        let bell = changes.any();

        self.last_error.clear();
        self.prev = Some(tracker);
        self.last_good = Some(sections);
        // The refreshed (and filtered) list may be shorter than before; keep the
        // cursor in range (or drop it if it emptied).
        self.ui.selected = nav::clamp(self.ui.selected, self.target_count());
        self.enter_alt()?;
        self.redraw(&changes)?;

        if self.armed && bell && !self.cli.no_bell {
            let _ = self.screen.beep();
        }
        self.armed = true;
        if !self.cli.no_cache
            && let Some(good) = &self.last_good
        {
            cache::save(self.repo, good);
        }
        Ok(())
    }

    /// Render a failed fetch: keep the last good data, add a dim error line, and
    /// do not ring. With no data yet, just the error line and footer show.
    fn show_error(&mut self, e: anyhow::Error) -> Result<()> {
        self.last_error = short_error(&e);
        self.enter_alt()?;
        self.redraw(&Changes::default())
    }

    /// Repaint the current frame in place (after a `?` toggle or a resize), once
    /// there is something to show.
    fn repaint_last(&mut self) -> Result<()> {
        if self.last_good.is_some() {
            self.redraw(&Changes::default())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uncurses::text::Encode;

    /// Paint a dashboard onto an offscreen buffer and read it back as plain text.
    fn body(sections: &Sections, ui: &Ui) -> String {
        let mut canvas = TextBuffer::new(render::MAX_WIDTH as u16, 64);
        let used = paint_dashboard(
            &mut canvas,
            sections,
            ui,
            &Changes::default(),
            "",
            None,
            true,
        );
        canvas.resize(render::MAX_WIDTH as u16, used.max(1));
        canvas.display_with(Profile::Disabled).to_string()
    }

    /// A `Ui` for the given view with nothing selected and no filter.
    fn ui(view: View) -> Ui {
        Ui {
            view,
            show_help: false,
            selected: None,
            search: String::new(),
            searching: false,
        }
    }

    #[test]
    fn empty_sections_still_show_their_headers_then_a_placeholder() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Mine));

        // Each section header is present even though it has no rows...
        assert!(body.contains("My open PRs (0)"));
        assert!(body.contains("Merge Queue (0)"));
        assert!(body.contains("My merged PRs (0)"));
        // ...and the placeholder follows the header on the next line.
        let after = |title: &str, msg: &str| {
            let h = body.find(title).expect("header present");
            let p = body.find(msg).expect("placeholder present");
            assert!(p > h, "placeholder for {title} should follow its header");
        };
        after("My open PRs (0)", "No open PRs.");
        after("Merge Queue (0)", "No merge queue.");
        after("My merged PRs (0)", "No recent merged PRs.");
    }

    #[test]
    fn queue_header_shows_next_eta() {
        let sections = Sections {
            queue: Some(vec![]),
            queue_next_eta: Some(11 * 60),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Mine));
        assert!(body.contains("Merge Queue (0)"));
        assert!(body.contains("~11m to merge"));
    }

    #[test]
    fn reviews_view_renders_its_own_sections() {
        let sections = Sections {
            reviews: Some(vec![]),
            reviewed_merged: Some(vec![]),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Reviews));
        // The Reviews view shows its two headers (not the Mine ones).
        assert!(body.contains("Reviews (0)"));
        assert!(body.contains("Reviewed & merged (0)"));
        assert!(!body.contains("My open PRs"));
    }

    #[test]
    fn selection_places_the_caret_on_the_chosen_row() {
        let pr = |n: i64| prs::PrRow {
            number: n,
            is_draft: false,
            title: format!("pr {n}"),
            status: None,
            merge_state: None,
            queue: None,
            fail: 0,
            url: format!("https://pr/{n}"),
            updated_at: None,
        };
        let sections = Sections {
            prs: Some(vec![pr(1), pr(2)]),
            ..Sections::EMPTY
        };
        // No selection -> no caret anywhere (the glanceable default).
        let caret = '>';
        let none = body(&sections, &ui(View::Mine));
        assert!(!none.contains(caret));
        // Selecting the second row draws exactly one caret, on that row.
        let sel = body(
            &sections,
            &Ui {
                selected: Some(1),
                ..ui(View::Mine)
            },
        );
        assert_eq!(sel.matches(caret).count(), 1);
        let caret_at = sel.find(caret).unwrap();
        assert!(caret_at < sel.find("#2").unwrap());
        assert!(caret_at > sel.find("#1").unwrap());
    }
}
