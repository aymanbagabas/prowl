//! My-open-PRs view: rows, sorting, styling, and table building.
//!
//! One row is: a change marker, a single mergeability glyph (the whole "can I
//! merge this?" answer), the PR number + title, and then the detail group that explains a blocked PR — a failing/running/passing check
//! semaphore and the unresolved-review-thread count. Conflicts are *only* the
//! glyph's job, so nothing is reported twice.

use crate::model::PrNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE, Checks, Lamp, Mergeable, PEACH, Status};
use anstyle::Style;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrRow {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    /// The leading glyph: whether GitHub would let this merge right now.
    pub mergeable: Mergeable,
    /// Coarse CI/merge state; not rendered, it is the bell's change key.
    pub status: Option<Status>,
    /// Failing / running / passing check runs on the last commit.
    pub checks: Checks,
    /// Unresolved review threads (capped at one page — see `unresolved_capped`).
    pub unresolved: usize,
    /// Whether the PR has more review threads than the page we counted.
    pub unresolved_capped: bool,
    pub queue: Option<(i64, String)>,
    pub url: String,
    pub updated_at: Option<String>,
}

/// Build rows sorted by last update time (most recent first).
pub fn build_rows(nodes: Vec<PrNode>) -> Vec<PrRow> {
    let mut rows: Vec<PrRow> = nodes
        .into_iter()
        .map(|pr| {
            let checks = pr.checks();
            let mergeable =
                status::mergeable_of(pr.merge_state_status.as_deref(), pr.mergeable.as_deref());
            let (unresolved, unresolved_capped) = pr.review_threads.unresolved();
            PrRow {
                number: pr.number,
                is_draft: pr.is_draft,
                mergeable,
                status: status::derive_status(mergeable, checks),
                checks,
                unresolved,
                unresolved_capped,
                queue: pr.merge_queue_entry.map(|e| (e.position, e.state)),
                title: pr.title,
                url: pr.url,
                updated_at: pr.updated_at,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.number.cmp(&a.number))
    });
    rows
}

/// Drop PRs that are in the merge queue: they're shown in the Merge Queue
/// section, so listing them here too would be redundant. Kept separate from
/// `build_rows` so the caller can skip it when the queue section is hidden (and
/// the PR would otherwise vanish entirely).
pub fn without_queued(mut rows: Vec<PrRow>) -> Vec<PrRow> {
    rows.retain(|r| r.queue.is_none());
    rows
}

/// One lamp of the check semaphore: dim when zero, its palette color (bold)
/// when not, so only the counts that matter catch the eye.
fn lamp_cell(n: u64, lamp: Lamp) -> Cell {
    if n == 0 {
        Cell::styled("0".to_string(), Style::new().dimmed())
    } else {
        Cell::styled(n.to_string(), status::fg(status::lamp_color(lamp)).bold())
    }
}

pub fn to_table(rows: &[PrRow], ascii: bool, highlight: &HashSet<i64>) -> Table {
    let dim = Style::new().dimmed();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let mark = render::change_marker(highlight.contains(&r.number), ascii);
        let (glyph, color) = (
            status::mergeable_glyph(r.mergeable, ascii),
            status::mergeable_style(r.mergeable).1,
        );
        let merge = Cell::styled(glyph.to_string(), status::fg(color));
        // A draft's number is dimmed; the glyph already reports it as blocked.
        let pr_style = if r.is_draft { dim } else { status::fg(BLUE) };
        let pr = Cell::link_styled(format!("#{}", r.number), r.url.clone(), pr_style);
        let threads = if r.unresolved == 0 {
            Cell::styled("0".to_string(), dim)
        } else {
            let capped = if r.unresolved_capped { "+" } else { "" };
            Cell::styled(
                format!("{}{capped}", r.unresolved),
                status::fg(PEACH).bold(),
            )
        };

        let mut row = vec![mark, merge, pr, Cell::plain(r.title.clone())];
        row.extend([
            lamp_cell(r.checks.fail, Lamp::Fail),
            lamp_cell(r.checks.running, Lamp::Running),
            lamp_cell(r.checks.pass, Lamp::Pass),
            threads,
        ]);
        out.push(row);
    }
    let header = vec!["", "", "PR", "TITLE", "FAIL", "RUN", "PASS", "THREADS"];
    Table { header, rows: out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Commit, CommitNode, Commits, QueueEntry, ReviewThread, ReviewThreads, Rollup, RollupCounts,
        StateCount,
    };

    /// A PR node with the given merge state and per-state check-run counts.
    fn pr(number: i64, mergeable: &str, state: &str, runs: &[(&str, u64)]) -> PrNode {
        PrNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            mergeable: Some(mergeable.to_string()),
            merge_state_status: Some(state.to_string()),
            is_draft: false,
            updated_at: None,
            merge_queue_entry: None,
            review_threads: ReviewThreads {
                total_count: 0,
                nodes: vec![],
            },
            commits: Commits {
                nodes: vec![CommitNode {
                    commit: Commit {
                        status_check_rollup: Some(Rollup {
                            contexts: RollupCounts {
                                check_runs: runs
                                    .iter()
                                    .map(|(state, count)| StateCount {
                                        state: (*state).to_string(),
                                        count: *count,
                                    })
                                    .collect(),
                                status_contexts: vec![],
                            },
                        }),
                    },
                }],
            },
        }
    }

    #[test]
    fn sorts_by_updated_at_then_derives_checks_and_mergeability() {
        let mut a = pr(10, "MERGEABLE", "BLOCKED", &[("SUCCESS", 8)]);
        a.updated_at = Some("2026-06-19T10:00:00Z".to_string());
        let mut b = pr(
            42,
            "CONFLICTING",
            "DIRTY",
            &[("FAILURE", 2), ("IN_PROGRESS", 1), ("SUCCESS", 3)],
        );
        b.updated_at = Some("2026-06-19T09:00:00Z".to_string());
        // #10 was updated more recently than #42, so it sorts first despite the
        // lower number.
        let rows = build_rows(vec![a, b]);
        assert_eq!(rows[0].number, 10);
        assert_eq!(rows[0].mergeable, Mergeable::Blocked);
        assert_eq!(
            rows[0].checks,
            Checks {
                fail: 0,
                running: 0,
                pass: 8
            }
        );
        assert_eq!(rows[0].status, Some(Status::Pass));
        assert_eq!(rows[1].number, 42);
        assert_eq!(rows[1].mergeable, Mergeable::Conflicts);
        assert_eq!(
            rows[1].checks,
            Checks {
                fail: 2,
                running: 1,
                pass: 3
            }
        );
        assert_eq!(rows[1].status, Some(Status::Conflicts));
    }

    #[test]
    fn counts_unresolved_review_threads() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        p.review_threads = ReviewThreads {
            total_count: 3,
            nodes: vec![
                ReviewThread { is_resolved: true },
                ReviewThread { is_resolved: false },
                ReviewThread { is_resolved: false },
            ],
        };
        let rows = build_rows(vec![p]);
        assert_eq!(rows[0].unresolved, 2);
        assert!(!rows[0].unresolved_capped);
    }

    #[test]
    fn flags_a_truncated_review_thread_page() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        // The server reports 120 threads but we only fetched one page of 100.
        p.review_threads = ReviewThreads {
            total_count: 120,
            nodes: (0..100)
                .map(|_| ReviewThread { is_resolved: false })
                .collect(),
        };
        let rows = build_rows(vec![p]);
        assert_eq!(rows[0].unresolved, 100);
        assert!(rows[0].unresolved_capped);
        let table = to_table(&rows, true, &HashSet::new());
        // Last column is THREADS; the `+` says "at least this many".
        assert_eq!(table.rows[0].last().unwrap().text, "100+");
    }

    #[test]
    fn a_commit_without_a_rollup_has_no_checks() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        p.commits.nodes[0].commit.status_check_rollup = None;
        let rows = build_rows(vec![p]);
        assert!(rows[0].checks.is_empty());
        assert_eq!(rows[0].status, None);
    }

    #[test]
    fn queue_entry_becomes_position_and_state() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        p.merge_queue_entry = Some(QueueEntry {
            position: 3,
            state: "QUEUED".to_string(),
        });
        let rows = build_rows(vec![p]);
        assert_eq!(rows[0].queue, Some((3, "QUEUED".to_string())));
    }

    #[test]
    fn without_queued_drops_prs_in_the_merge_queue() {
        let mut queued = pr(1, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        queued.merge_queue_entry = Some(QueueEntry {
            position: 1,
            state: "QUEUED".to_string(),
        });
        let open = pr(2, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        // #1 is queued, #2 isn't — only #2 remains in the open-PRs list.
        let rows = without_queued(build_rows(vec![queued, open]));
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn semaphore_shows_all_three_counts() {
        let rows = build_rows(vec![pr(
            1,
            "MERGEABLE",
            "CLEAN",
            &[
                ("FAILURE", 2),
                ("QUEUED", 1),
                ("IN_PROGRESS", 2),
                ("SUCCESS", 9),
            ],
        )]);
        let table = to_table(&rows, true, &HashSet::new());
        // ..., FAIL, RUN, PASS, THREADS
        let tail: Vec<&str> = table.rows[0][4..].iter().map(|c| c.text.as_str()).collect();
        assert_eq!(tail, ["2", "3", "9", "0"]);
    }
}
