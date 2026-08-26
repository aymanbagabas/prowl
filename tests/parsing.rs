//! Offline, fixture-based tests: real (and a crafted) GitHub GraphQL API
//! responses are parsed through the same path the binary uses, then turned into
//! rows and rendered. No network access.

use prowl::model::{MergedData, MineData, QueueData, ReviewsData};
use prowl::status::{Checks, Mergeable, ReviewState, Status};
use prowl::{github, merged, prs, queue, render, reviews};
use std::collections::HashSet;

fn parse<T: serde::de::DeserializeOwned>(json: &str) -> T {
    github::parse_graphql(json.as_bytes()).expect("fixture should parse")
}

// ---------------------------------------------------------------------------
// Merge queue
// ---------------------------------------------------------------------------

#[test]
fn queue_parses_orders_and_flags_mine() {
    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");

    // Input positions are 2,1,3; rows come out ordered by position ascending.
    assert_eq!(
        rows.iter().map(|r| r.position).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // caarlos0 is at position 1 -> mine; octocat is not.
    assert!(rows[0].mine);
    assert_eq!(rows[0].number, 101);
    assert!(!rows[1].mine);
    // A null author renders as "ghost" and is never mine.
    assert_eq!(rows[2].author, "ghost");
    assert!(!rows[2].mine);
}

#[test]
fn queue_counts_speculative_build_checks() {
    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");

    // #101: 1 failing + 3 running + 6 passing check runs, plus one passing
    // legacy status context folded into the same semaphore.
    assert_eq!(
        rows[0].checks,
        Checks {
            fail: 1,
            running: 3,
            pass: 7,
        }
    );
    // #102 has nothing failing.
    assert_eq!(
        rows[1].checks,
        Checks {
            fail: 0,
            running: 2,
            pass: 8,
        }
    );
    // #103's speculative commit has no checks at all.
    assert_eq!(rows[2].checks, Checks::default());

    let out = render::render_table(&queue::to_table(&rows, true), false);
    assert!(out.contains("FAIL"));
    assert!(out.contains("RUN"));
    assert!(out.contains("PASS"));
}

#[test]
fn queue_null_and_empty_both_yield_no_rows() {
    let null: QueueData = parse(include_str!("fixtures/queue_null.json"));
    let empty: QueueData = parse(include_str!("fixtures/queue_empty.json"));
    assert!(model_queue_nodes(null).is_empty());
    assert!(model_queue_nodes(empty).is_empty());
}

#[test]
fn queue_next_eta_parses_and_defaults_to_none() {
    let populated: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    assert_eq!(prowl::model::queue_next_eta(&populated), Some(660));
    // A null queue or one without the field yields no estimate.
    let null: QueueData = parse(include_str!("fixtures/queue_null.json"));
    let empty: QueueData = parse(include_str!("fixtures/queue_empty.json"));
    assert_eq!(prowl::model::queue_next_eta(&null), None);
    assert_eq!(prowl::model::queue_next_eta(&empty), None);
}

#[test]
fn queue_styled_render_uses_palette_and_links() {
    use uncurses::buffer::TextBuffer;
    use uncurses::color::Profile;
    use uncurses::text::Encode;

    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");
    let table = queue::to_table(&rows, false);

    let mut canvas = TextBuffer::new(render::MAX_WIDTH as u16, 8);
    render::paint_table(&mut canvas, &table, 40, false, 0);
    let mut buf = Vec::new();
    canvas.encode_with(&mut buf, Profile::TrueColor).unwrap();
    let out = String::from_utf8(buf).unwrap();

    // Mine row is highlighted yellow (#f9e2af); others' PR cell is blue (#89b4fa).
    assert!(out.contains("38;2;249;226;175"), "expected mine yellow");
    assert!(out.contains("38;2;137;180;250"), "expected not-mine blue");
    // URLs are OSC-8 hyperlinks carrying a per-URL `id=` param.
    assert!(out.contains("\x1b]8;id="));
    assert!(out.contains(";https://github.com/octo/repo/pull/101\x1b\\"));
    // Wait/build columns are present; the entry whose checks are all still
    // queued (no `startedAt`) shows a dash.
    assert!(out.contains("WAIT"));
    assert!(out.contains("BUILD"));
    assert!(out.contains('\u{2014}'));
}

#[test]
fn queue_build_time_is_earliest_check_run_start() {
    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");

    // #101 (pos 1): earliest check-run start across its suites (ignoring the
    // empty / null / not-yet-started ones).
    assert_eq!(rows[0].number, 101);
    assert_eq!(
        rows[0].build_started_at.as_deref(),
        Some("2026-06-19T11:58:00Z")
    );
    // #102 (pos 2): the earlier of its two suite starts.
    assert_eq!(rows[1].number, 102);
    assert_eq!(
        rows[1].build_started_at.as_deref(),
        Some("2026-06-19T11:59:00Z")
    );
    // #103 (pos 3): its only run hasn't started -> no build time.
    assert_eq!(rows[2].number, 103);
    assert!(rows[2].build_started_at.is_none());
}

// ---------------------------------------------------------------------------
// My open PRs
// ---------------------------------------------------------------------------

#[test]
fn mine_parses_sorts_and_derives_mergeability_and_checks() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);

    // Sorted by last update time (most recent first).
    assert_eq!(
        rows.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![6475, 5323, 6656]
    );
    let upd: Vec<&Option<String>> = rows.iter().map(|r| &r.updated_at).collect();
    assert!(upd.windows(2).all(|w| w[0] >= w[1]), "updatedAt descending");

    // #6475 conflicts (DIRTY), and its rollup has 4 failing runs; NEUTRAL and
    // SUCCESS both count as passed.
    assert_eq!(rows[0].mergeable, Mergeable::Conflicts);
    assert_eq!(
        rows[0].checks,
        Checks {
            fail: 4,
            running: 0,
            pass: 17
        }
    );
    assert_eq!(rows[0].status, Some(Status::Conflicts));
    assert_eq!(rows[0].branch, "goreleaser-install-script");

    // #5323 conflicts and has no checks at all.
    assert_eq!(rows[1].mergeable, Mergeable::Conflicts);
    assert!(rows[1].checks.is_empty());

    // #6656 is BLOCKED (waiting on a review), all checks green.
    assert_eq!(rows[2].mergeable, Mergeable::Blocked);
    assert_eq!(
        rows[2].checks,
        Checks {
            fail: 0,
            running: 0,
            pass: 20
        }
    );
    assert_eq!(rows[2].status, Some(Status::Pass));
}

#[test]
fn mine_ascii_mergeable_letters() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);
    let table = prs::to_table(&rows, true, &HashSet::new(), false); // ascii, no highlights
    // Column 0 is the change marker; column 1 is the mergeability glyph.
    let st: Vec<&str> = table.rows.iter().map(|r| r[1].text.as_str()).collect();
    assert_eq!(st, vec!["!", "!", "n"]); // conflicts, conflicts, blocked
}

#[test]
fn mine_renders_the_check_semaphore_and_thread_count() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);
    let table = prs::to_table(&rows, true, &HashSet::new(), false);
    assert_eq!(
        table.header,
        ["", "", "PR", "TITLE", "FAIL", "RUN", "PASS", "THREADS"]
    );
    // #6475: 4 failing, none running, 17 passed, no unresolved threads.
    let tail: Vec<&str> = table.rows[0][4..].iter().map(|c| c.text.as_str()).collect();
    assert_eq!(tail, ["4", "0", "17", "0"]);
    // #5323 has no checks at all — every lamp reads zero.
    let tail: Vec<&str> = table.rows[1][4..].iter().map(|c| c.text.as_str()).collect();
    assert_eq!(tail, ["0", "0", "0", "0"]);
}

#[test]
fn mine_changed_rows_get_a_marker() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);
    let highlight = HashSet::from([5323]);
    let table = prs::to_table(&rows, true, &highlight, false);
    let marks: Vec<&str> = table.rows.iter().map(|r| r[0].text.as_str()).collect();
    assert_eq!(marks, vec![" ", ">", " "]); // only #5323 is flagged
}

#[test]
fn mine_empty_yields_no_rows() {
    let data: MineData = parse(include_str!("fixtures/mine_empty.json"));
    assert!(data.search.nodes.is_empty());
}

#[test]
fn mine_tolerates_a_missing_rollup_and_partial_errors() {
    // GitHub returns a null `statusCheckRollup` for a commit with no checks and
    // attaches a top-level `errors` array. The response must still parse and
    // simply report an empty semaphore rather than failing the whole fetch.
    let data: MineData = parse(include_str!("fixtures/mine_no_rollup.json"));
    let rows = prs::build_rows(data.search.nodes);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 123);
    assert!(rows[0].checks.is_empty());
    assert_eq!(rows[0].status, None);
    // One of its two review threads is still unresolved.
    assert_eq!(rows[0].unresolved, 1);
    assert!(!rows[0].unresolved_capped);
}

#[test]
fn mine_partial_null_surfaces_graphql_error() {
    // `data` is present but a required (non-Option) field — `commits` — is null,
    // so typing the data fails. With an `errors` array attached, the real GitHub
    // message must surface instead of a generic JSON parse error.
    let err = github::parse_graphql::<MineData>(
        include_str!("fixtures/mine_null_commits.json").as_bytes(),
    )
    .expect_err("a null non-Option field should fail to type");
    assert!(
        err.to_string()
            .contains("Resource not accessible by integration"),
        "expected the GraphQL error to surface, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Recently merged
// ---------------------------------------------------------------------------

#[test]
fn merged_parses_sorts_desc_and_caps() {
    let data: MergedData = parse(include_str!("fixtures/merged.json"));
    let rows = merged::build_rows(data.search.nodes, 4, &Default::default());

    assert_eq!(rows.len(), 4); // capped at the limit
    // Most recently merged first.
    assert_eq!(rows[0].number, 6649);
    // No release map supplied, so nothing is annotated as shipped.
    assert!(rows[0].release.is_none());
    // Strictly descending merge timestamps.
    let ts: Vec<&Option<String>> = rows.iter().map(|r| &r.merged_at).collect();
    assert!(ts.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn merged_empty_yields_no_rows() {
    let data: MergedData = parse(include_str!("fixtures/merged_empty.json"));
    assert!(data.search.nodes.is_empty());
}

// ---------------------------------------------------------------------------
// Reviews (PRs to review + reviewed-and-merged)
// ---------------------------------------------------------------------------

#[test]
fn reviews_parse_dedupe_and_derive_states() {
    let data: ReviewsData = parse(include_str!("fixtures/reviews.json"));
    let rows = reviews::build_open_rows(data);

    // #102 is in both searches (a re-review) -> de-duplicated to four rows,
    // ordered by state rank: Awaiting, ReReview, Updated, Reviewed.
    assert_eq!(
        rows.iter().map(|r| (r.number, r.state)).collect::<Vec<_>>(),
        vec![
            (101, ReviewState::Awaiting),
            (102, ReviewState::ReReview),
            (103, ReviewState::Updated),
            (104, ReviewState::Reviewed),
        ]
    );
    // #101 has a null author -> "ghost".
    assert_eq!(rows[0].author, "ghost");
}

#[test]
fn reviews_open_render_uses_palette_and_links() {
    let data: ReviewsData = parse(include_str!("fixtures/reviews.json"));
    let rows = reviews::build_open_rows(data);
    let out = render::render_table(&reviews::open_to_table(&rows, false), true);
    // The Awaiting glyph is yellow (#f9e2af); PR numbers are OSC-8 hyperlinks
    // carrying a per-URL `id=` param.
    assert!(out.contains("38;2;249;226;175"), "expected awaiting yellow");
    assert!(out.contains("\x1b]8;id="));
    assert!(out.contains(";https://github.com/octo/repo/pull/101\x1b\\"));
}

#[test]
fn reviews_ascii_state_letters() {
    let data: ReviewsData = parse(include_str!("fixtures/reviews.json"));
    let rows = reviews::build_open_rows(data);
    let table = reviews::open_to_table(&rows, true); // ascii
    // Column 0 is the margin; column 1 is the review-state glyph.
    let st: Vec<&str> = table.rows.iter().map(|r| r[1].text.as_str()).collect();
    assert_eq!(st, vec!["a", "@", "^", "v"]); // awaiting, re-review, updated, reviewed
}

#[test]
fn reviewed_merged_parses_sorts_desc_with_author() {
    let data: MergedData = parse(include_str!("fixtures/reviewed_merged.json"));
    let rows = reviews::build_merged_rows(data.search.nodes, 10);
    // Most recently merged first; author is carried through.
    assert_eq!(rows[0].number, 201);
    assert_eq!(rows[0].author, "erin");
    assert_eq!(rows[1].number, 202);
    assert_eq!(rows[1].author, "frank");
}

// `model::queue_nodes` takes ownership; a tiny shim keeps the call sites tidy.
fn model_queue_nodes(data: QueueData) -> Vec<prowl::model::QueueEntryNode> {
    prowl::model::queue_nodes(data)
}
