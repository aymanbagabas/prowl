//! Typed serde models for the three GitHub GraphQL queries, plus the fetch
//! helpers that run them. Queries are sent verbatim (the merged query's page
//! size is the only thing we interpolate, so `--merged-limit` is honored).

use crate::github::{Client, Repo};
use crate::status::{self, Checks};
use anyhow::Result;
use serde::Deserialize;

// ----------------------------------------------------------------------------
// Merge queue
// ----------------------------------------------------------------------------

pub const QUEUE_QUERY: &str = r#"query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      nextEntryEstimatedTimeToMerge
      entries(first: 100) {
        nodes {
          position
          enqueuedAt
          headCommit {
            statusCheckRollup {
              contexts(first: 40) {
                checkRunCountsByState { state count }
                statusContextCountsByState { state count }
                nodes { ... on CheckRun { startedAt } }
              }
            }
          }
          pullRequest { number title url headRefName author { login } }
        }
      }
    }
  }
}"#;

#[derive(Debug, Deserialize)]
pub struct QueueData {
    pub repository: Option<QueueRepo>,
}

#[derive(Debug, Deserialize)]
pub struct QueueRepo {
    #[serde(rename = "mergeQueue")]
    pub merge_queue: Option<MergeQueue>,
}

#[derive(Debug, Deserialize)]
pub struct MergeQueue {
    /// Seconds until a newly added entry would merge; shown by the queue header.
    #[serde(rename = "nextEntryEstimatedTimeToMerge")]
    pub next_entry_estimated_time_to_merge: Option<i64>,
    pub entries: QueueEntries,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntries {
    pub nodes: Vec<QueueEntryNode>,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntryNode {
    pub position: i64,
    /// When the entry joined the queue; drives the WAIT column.
    #[serde(rename = "enqueuedAt")]
    pub enqueued_at: Option<String>,
    /// The speculative merge commit the queue is building; its checks' `startedAt`
    /// mark when CI actually began (the BUILD column). `None` until the entry has
    /// a speculative commit.
    #[serde(rename = "headCommit")]
    pub head_commit: Option<QueueCommit>,
    #[serde(rename = "pullRequest")]
    pub pull_request: QueuePr,
}

#[derive(Debug, Deserialize)]
pub struct QueueCommit {
    /// Flat rollup of every check on the commit. Preferred over `checkSuites`:
    /// it is a single (cheaper) connection and front-loads the actual check
    /// runs, whereas the first check suites are often app integrations with no
    /// started run. `None` when the commit has no checks configured.
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<QueueRollup>,
}

#[derive(Debug, Deserialize)]
pub struct QueueRollup {
    pub contexts: QueueContexts,
}

#[derive(Debug, Deserialize)]
pub struct QueueContexts {
    /// GitHub's own per-state check-run counts: exact and unpaginated, unlike
    /// `nodes` (which is capped and only read for the earliest `startedAt`).
    #[serde(rename = "checkRunCountsByState", default)]
    pub check_runs: Vec<StateCount>,
    #[serde(rename = "statusContextCountsByState", default)]
    pub status_contexts: Vec<StateCount>,
    pub nodes: Vec<QueueContext>,
}

/// One rollup context. Only a `CheckRun` carries `startedAt`; a legacy
/// `StatusContext` has no such field and deserializes to `None`.
#[derive(Debug, Deserialize)]
pub struct QueueContext {
    #[serde(rename = "startedAt")]
    pub started_at: Option<String>,
}

impl QueueEntryNode {
    /// The earliest moment any check on the speculative merge commit began
    /// running (RFC 3339). `None` when nothing has started yet (or there is no
    /// speculative commit / no checks), which the BUILD column renders as a
    /// dash. RFC 3339 `...Z` timestamps sort lexically == chronologically, so
    /// `min` is earliest.
    /// Failing / running / passing checks on the speculative merge commit —
    /// the queue's own CI semaphore. Empty when the entry has no speculative
    /// commit or no checks yet.
    pub fn checks(&self) -> Checks {
        let mut c = Checks::default();
        let Some(rollup) = self
            .head_commit
            .as_ref()
            .and_then(|h| h.status_check_rollup.as_ref())
        else {
            return c;
        };
        for sc in &rollup.contexts.check_runs {
            c.add(status::check_run_lamp(&sc.state), sc.count);
        }
        for sc in &rollup.contexts.status_contexts {
            c.add(status::status_context_lamp(&sc.state), sc.count);
        }
        c
    }

    pub fn build_started_at(&self) -> Option<String> {
        self.head_commit
            .as_ref()?
            .status_check_rollup
            .as_ref()?
            .contexts
            .nodes
            .iter()
            .filter_map(|c| c.started_at.clone())
            .min()
    }
}

#[derive(Debug, Deserialize)]
pub struct QueuePr {
    pub number: i64,
    pub title: String,
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    pub author: Option<Login>,
}

#[derive(Debug, Deserialize)]
pub struct Login {
    pub login: String,
}

/// Extract the entry nodes from a parsed queue response. A null queue or an
/// empty queue both yield `[]`.
pub fn queue_nodes(data: QueueData) -> Vec<QueueEntryNode> {
    data.repository
        .and_then(|r| r.merge_queue)
        .map(|q| q.entries.nodes)
        .unwrap_or_default()
}

/// The queue-level estimate (seconds until a newly added entry would merge),
/// or `None` when there is no queue or the API omits it.
pub fn queue_next_eta(data: &QueueData) -> Option<i64> {
    data.repository
        .as_ref()
        .and_then(|r| r.merge_queue.as_ref())
        .and_then(|q| q.next_entry_estimated_time_to_merge)
}

/// Fetch the merge-queue entries and the queue-level ETA. A null or empty queue
/// yields `([], None)`.
pub fn fetch_queue(client: &Client, repo: &Repo) -> Result<(Vec<QueueEntryNode>, Option<i64>)> {
    let data: QueueData = client.graphql(
        QUEUE_QUERY,
        serde_json::json!({ "owner": repo.owner, "name": repo.name }),
    )?;
    let eta = queue_next_eta(&data);
    Ok((queue_nodes(data), eta))
}

// ----------------------------------------------------------------------------
// My open PRs
// ----------------------------------------------------------------------------

pub const MINE_QUERY: &str = r#"query($q: String!) {
  search(type: ISSUE, first: 50, query: $q) {
    nodes {
      ... on PullRequest {
        number title url mergeable mergeStateStatus isDraft updatedAt headRefName
        mergeQueueEntry { position state }
        reviewThreads(first: 100) { totalCount nodes { isResolved } }
        commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 1) {
          checkRunCountsByState { state count }
          statusContextCountsByState { state count }
        } } } } }
      }
    }
  }
}"#;

#[derive(Debug, Deserialize)]
pub struct MineData {
    pub search: MineNodes,
}

#[derive(Debug, Deserialize)]
pub struct MineNodes {
    pub nodes: Vec<PrNode>,
}

#[derive(Debug, Deserialize)]
pub struct PrNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    /// The PR's head branch.
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    #[serde(rename = "mergeQueueEntry")]
    pub merge_queue_entry: Option<QueueEntry>,
    #[serde(rename = "reviewThreads")]
    pub review_threads: ReviewThreads,
    pub commits: Commits,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntry {
    pub position: i64,
    pub state: String,
}

/// The PR's review threads. There is no "unresolved" aggregate, so we page the
/// first 100 and count them; `total_count` tells us whether that page was
/// complete (a PR with more than 100 threads renders its count as `100+`).
#[derive(Debug, Deserialize)]
pub struct ReviewThreads {
    #[serde(rename = "totalCount")]
    pub total_count: u64,
    pub nodes: Vec<ReviewThread>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewThread {
    #[serde(rename = "isResolved")]
    pub is_resolved: bool,
}

impl ReviewThreads {
    /// (unresolved threads on the fetched page, whether the page was truncated).
    pub fn unresolved(&self) -> (usize, bool) {
        let n = self.nodes.iter().filter(|t| !t.is_resolved).count();
        (n, self.total_count > self.nodes.len() as u64)
    }
}

#[derive(Debug, Deserialize)]
pub struct Commits {
    pub nodes: Vec<CommitNode>,
}

#[derive(Debug, Deserialize)]
pub struct CommitNode {
    pub commit: Commit,
}

#[derive(Debug, Deserialize)]
pub struct Commit {
    /// `null` when the commit has no checks or statuses at all.
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
pub struct Rollup {
    pub contexts: RollupCounts,
}

/// GitHub's own per-state tallies for the rollup. Using the aggregates instead
/// of paging `contexts` keeps the query cheap and the counts exact — no phantom
/// zero-run check suites, no truncated page to compensate for.
#[derive(Debug, Deserialize)]
pub struct RollupCounts {
    #[serde(rename = "checkRunCountsByState")]
    pub check_runs: Vec<StateCount>,
    #[serde(rename = "statusContextCountsByState")]
    pub status_contexts: Vec<StateCount>,
}

#[derive(Debug, Deserialize)]
pub struct StateCount {
    pub state: String,
    pub count: u64,
}

impl PrNode {
    /// The failing / running / passing check counts for the PR's last commit.
    /// Check runs and legacy commit statuses are folded into the same semaphore.
    pub fn checks(&self) -> Checks {
        let mut c = Checks::default();
        let Some(rollup) = self
            .commits
            .nodes
            .first()
            .and_then(|n| n.commit.status_check_rollup.as_ref())
        else {
            return c;
        };
        for sc in &rollup.contexts.check_runs {
            c.add(status::check_run_lamp(&sc.state), sc.count);
        }
        for sc in &rollup.contexts.status_contexts {
            c.add(status::status_context_lamp(&sc.state), sc.count);
        }
        c
    }
}

pub fn mine_search(repo: &Repo, me: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:open author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, me
    )
}

pub fn fetch_my_prs(client: &Client, repo: &Repo, me: &str) -> Result<Vec<PrNode>> {
    let q = mine_search(repo, me);
    let data: MineData = client.graphql(MINE_QUERY, serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

// ----------------------------------------------------------------------------
// Recently merged
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MergedData {
    pub search: MergedNodes,
}

#[derive(Debug, Deserialize)]
pub struct MergedNodes {
    pub nodes: Vec<MergedNode>,
}

#[derive(Debug, Deserialize)]
pub struct MergedNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    /// PR author; used by the reviewed-and-merged section (the Mine merged
    /// section ignores it, since those are all the viewer's own PRs).
    pub author: Option<Login>,
    #[serde(rename = "mergedAt")]
    pub merged_at: Option<String>,
}

/// The recently-merged query; `first` is the page size (clamped 1..=100). Used
/// for both the "my merged PRs" and "reviewed & merged" sections (only the
/// search query differs).
pub fn merged_query(limit: usize) -> String {
    let first = limit.clamp(1, 100);
    format!(
        r#"query($q: String!) {{
  search(type: ISSUE, first: {first}, query: $q) {{
    nodes {{
      ... on PullRequest {{
        number title url headRefName mergedAt author {{ login }}
      }}
    }}
  }}
}}"#
    )
}

pub fn merged_search(repo: &Repo, me: &str, since: &str) -> String {
    // GitHub search can't sort by merge time, but a merge bumps `updatedAt` and
    // later edits only bump it further, so `updated-desc` still surfaces the most
    // recently merged PRs when the result is capped (rows are re-sorted by
    // `mergedAt` for display).
    format!(
        "repo:{}/{} is:pr is:merged author:{} merged:>={} sort:updated-desc",
        repo.owner, repo.name, me, since
    )
}

pub fn fetch_merged(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<Vec<MergedNode>> {
    let q = merged_search(repo, me, since);
    let data: MergedData = client.graphql(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

/// The "merged PRs I reviewed" search: merged PRs in the repo that I reviewed,
/// excluding my own (those live in the Mine view). Same shape as `merged_search`.
pub fn reviewed_merged_search(repo: &Repo, me: &str, since: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:merged reviewed-by:{} -author:{} merged:>={} sort:updated-desc",
        repo.owner, repo.name, me, me, since
    )
}

pub fn fetch_reviewed_merged(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<Vec<MergedNode>> {
    let q = reviewed_merged_search(repo, me, since);
    let data: MergedData = client.graphql(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

// ----------------------------------------------------------------------------
// Reviews (open PRs awaiting / under my review)
// ----------------------------------------------------------------------------

/// Two aliased searches in one request: PRs whose review is requested from me
/// (`requested`) and PRs I've already reviewed (`reviewed`). A PR can appear in
/// both (a re-review). Each node carries my own reviews (so we know if/when I
/// reviewed) and its last commit date (so we can flag "updated since").
pub const REVIEWS_QUERY: &str = r#"query($me: String!, $requested: String!, $reviewed: String!) {
  requested: search(type: ISSUE, first: 50, query: $requested) { nodes { ...rev } }
  reviewed: search(type: ISSUE, first: 50, query: $reviewed) { nodes { ...rev } }
}
fragment rev on PullRequest {
  number title url headRefName isDraft updatedAt
  author { login }
  commits(last: 1) { nodes { commit { committedDate } } }
  reviews(author: $me, first: 20, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) { nodes { submittedAt } }
}"#;

#[derive(Debug, Deserialize)]
pub struct ReviewsData {
    /// PRs requesting my review (directly or via a team, per the scope).
    pub requested: ReviewSearch,
    /// PRs I have already reviewed.
    pub reviewed: ReviewSearch,
}

#[derive(Debug, Deserialize)]
pub struct ReviewSearch {
    pub nodes: Vec<ReviewPrNode>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewPrNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    pub author: Option<Login>,
    pub commits: ReviewCommits,
    /// My reviews on this PR (submitted only; the query filters out PENDING).
    pub reviews: MyReviews,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommits {
    pub nodes: Vec<ReviewCommitNode>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommitNode {
    pub commit: ReviewCommit,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommit {
    #[serde(rename = "committedDate")]
    pub committed_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MyReviews {
    pub nodes: Vec<MyReview>,
}

#[derive(Debug, Deserialize)]
pub struct MyReview {
    #[serde(rename = "submittedAt")]
    pub submitted_at: Option<String>,
}

/// The two review searches: PRs requesting my review and PRs I have reviewed,
/// both open, in this repo, excluding my own. `requested_qualifier` is
/// `review-requested` (me + my teams) or `user-review-requested` (only me).
pub fn reviews_searches(repo: &Repo, me: &str, requested_qualifier: &str) -> (String, String) {
    let requested = format!(
        "repo:{}/{} is:pr is:open {}:{} -author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, requested_qualifier, me, me
    );
    let reviewed = format!(
        "repo:{}/{} is:pr is:open reviewed-by:{} -author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, me, me
    );
    (requested, reviewed)
}

pub fn fetch_reviews(
    client: &Client,
    repo: &Repo,
    me: &str,
    requested_qualifier: &str,
) -> Result<ReviewsData> {
    let (requested, reviewed) = reviews_searches(repo, me, requested_qualifier);
    client.graphql(
        REVIEWS_QUERY,
        serde_json::json!({ "me": me, "requested": requested, "reviewed": reviewed }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pr_query_requests_the_head_branch() {
        assert!(QUEUE_QUERY.contains("headRefName"));
        assert!(MINE_QUERY.contains("headRefName"));
        assert!(merged_query(20).contains("headRefName"));
        assert!(REVIEWS_QUERY.contains("headRefName"));
    }
}
