use crate::{
    model::{self, Availability, Cache, Forge, Issue, SearchPage, Status},
    storage,
};
use anyhow::{Context, Result, anyhow, ensure};
use reqwest::{StatusCode, Url, blocking::Client, header::HeaderMap};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    io::Read,
    path::Path,
    sync::Mutex,
    time::{Duration, UNIX_EPOCH},
};

const PAGE: usize = 20;
const SEARCH_REQUESTS_PER_CYCLE: usize = 6;
const GITHUB_PAGE_SIZE: usize = 100;
const GITHUB_MAX_PAGE: usize = 1000 / GITHUB_PAGE_SIZE;
const CHECKS: usize = 8;
const GITLAB_PUBLIC_CHECKS: usize = 18;
const TERMS: [&str; 3] = ["bounty", "reward", "payout"];

#[derive(Deserialize)]
struct Remote {
    id: Option<u64>,
    title: String,
    state: String,
    html_url: Option<String>,
    web_url: Option<String>,
    body: Option<String>,
    description: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
    assignees: Option<Vec<Value>>,
    assignee: Option<Value>,
    pull_request: Option<Value>,
    confidential: Option<bool>,
    repository: Option<Repository>,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct Repository {
    #[serde(default)]
    private: bool,
    #[serde(default)]
    archived: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Label {
    Text(String),
    Object { name: String },
}

impl Remote {
    fn into_issue(self, forge: Forge) -> Result<Option<Issue>> {
        if self.pull_request.is_some()
            || self.confidential == Some(true)
            || self.archived == Some(true)
            || self.repository.is_some_and(|r| r.private || r.archived)
        {
            return Ok(None);
        }
        let url = self
            .html_url
            .or(self.web_url)
            .context("issue URL missing")?;
        let (url, _) = issue_url(forge, &url)?;
        let body = self.body.or(self.description).unwrap_or_default();
        let labels: Vec<_> = self
            .labels
            .into_iter()
            .map(|label| match label {
                Label::Text(name) | Label::Object { name } => name,
            })
            .collect();
        let reward = model::reward(&self.title, &body, &labels);
        let state = match self.state.as_str() {
            "closed" => Availability::Closed,
            "open" | "opened" => match (self.assignees, self.assignee) {
                (Some(items), _) if !items.is_empty() => Availability::Assigned,
                (_, Some(_)) => Availability::Assigned,
                (Some(_), None) => Availability::Open,
                (None, None) if forge == Forge::Codeberg => Availability::Open,
                _ => Availability::Unreachable,
            },
            _ => Availability::Unreachable,
        };
        Ok(Some(Issue {
            forge,
            provider_id: self.id,
            title: self.title,
            url: url.into(),
            labels,
            state,
            reward,
            checked: model::now(),
            body: body.chars().take(12_000).collect(),
        }))
    }
}

fn api(forge: Forge, segments: &[&str]) -> Url {
    let base = match forge {
        Forge::GitHub => "https://api.github.com/",
        Forge::GitLab => "https://gitlab.com/api/v4",
        Forge::Codeberg => "https://codeberg.org/api/v1",
    };
    let mut url = Url::parse(base).expect("static API base");
    url.path_segments_mut()
        .expect("hierarchical API base")
        .pop_if_empty()
        .extend(segments);
    url
}

pub fn issue_url(forge: Forge, raw: &str) -> Result<(Url, Url)> {
    let mut web = Url::parse(raw)?;
    ensure!(
        web.scheme() == "https"
            && web.host_str() == Some(forge.host())
            && web.port().is_none()
            && web.username().is_empty()
            && web.password().is_none(),
        "unexpected issue origin"
    );
    web.set_query(None);
    web.set_fragment(None);
    let (project, number) = web
        .path()
        .trim_start_matches('/')
        .rsplit_once("/issues/")
        .context("unexpected issue path")?;
    let project = if forge == Forge::GitLab {
        project.strip_suffix("/-").unwrap_or(project)
    } else {
        project
    };
    ensure!(
        storage::valid_path(project) && number.parse::<u64>().is_ok(),
        "invalid issue path"
    );
    let segments: Vec<_> = project.split('/').collect();
    if forge == Forge::GitLab {
        ensure!(segments.len() >= 2, "invalid project path");
        let endpoint = api(forge, &["projects", project, "issues", number]);
        Ok((web, endpoint))
    } else {
        ensure!(segments.len() == 2, "invalid repository path");
        let endpoint = api(
            forge,
            &["repos", segments[0], segments[1], "issues", number],
        );
        Ok((web, endpoint))
    }
}

struct Session<'a> {
    client: Client,
    cache: &'a mut Cache,
    forge: Forge,
    commit: &'a dyn Fn(&Cache, String) -> Result<()>,
}

impl Session<'_> {
    fn get(&mut self, url: Url, token: Option<&str>) -> Result<Option<(Value, HeaderMap)>> {
        ensure!(
            self.cache.status[&self.forge].retry_at <= model::now(),
            "provider cooldown is active; pagination progress retained"
        );
        let mut request = self.client.get(url).header("Accept", "application/json");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(reqwest::Error::without_url)?;
        let headers = response.headers().clone();
        let status = response.status();
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .or_else(|| headers.get("ratelimit-remaining"))
            .is_some_and(|h| h == "0");
        if exhausted
            || status == StatusCode::TOO_MANY_REQUESTS
            || (self.forge == Forge::GitHub && status == StatusCode::FORBIDDEN)
            || headers.contains_key("retry-after")
        {
            let now = model::now();
            let retry = headers
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| {
                    h.parse::<u64>()
                        .ok()
                        .map(|secs| now.saturating_add(secs))
                        .or_else(|| {
                            httpdate::parse_http_date(h)
                                .ok()?
                                .duration_since(UNIX_EPOCH)
                                .ok()
                                .map(|d| d.as_secs())
                        })
                });
            let reset = headers
                .get("x-ratelimit-reset")
                .or_else(|| headers.get("ratelimit-reset"))
                .and_then(|h| h.to_str().ok()?.parse::<u64>().ok());
            let retry_at = retry
                .into_iter()
                .chain(reset)
                .filter(|t| *t > now)
                .max()
                .unwrap_or_else(|| now.saturating_add(3600));
            self.cache.status.entry(self.forge).or_default().retry_at = retry_at;
            (self.commit)(self.cache, "Cooldown recorded".into())?;
            let reason = if status == StatusCode::TOO_MANY_REQUESTS {
                "rate limited"
            } else if headers.contains_key("retry-after") {
                "server requested backoff"
            } else if exhausted {
                "rate-limit quota exhausted"
            } else {
                "access forbidden"
            };
            if !status.is_success() {
                return Err(anyhow!("HTTP {} · {reason}", status.as_u16()));
            }
        }
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        ensure!(status.is_success(), "HTTP {status}");
        const LIMIT: u64 = 4 * 1024 * 1024;
        let mut bytes = Vec::new();
        response.take(LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= LIMIT, "response exceeds 4 MiB");
        Ok(Some((
            serde_json::from_slice(&bytes).context("invalid API JSON")?,
            headers,
        )))
    }

    fn update(&mut self, remote: Value, previous: Option<&str>) -> Result<()> {
        let issue = serde_json::from_value::<Remote>(remote)?.into_issue(self.forge)?;
        if let Some(issue) = issue {
            let key = issue.key();
            if issue.reward.is_some()
                || self.cache.issues.contains_key(&key)
                || self.cache.issues.contains_key(&issue.url)
                || previous.is_some()
            {
                if let Some(previous) = previous.filter(|previous| *previous != key) {
                    self.cache.issues.remove(previous);
                }
                if key != issue.url {
                    self.cache.issues.remove(&issue.url);
                }
                self.cache.issues.insert(key, issue);
            }
        } else if let Some(existing) = previous.and_then(|key| self.cache.issues.get_mut(key)) {
            existing.state = Availability::Unreachable;
            existing.checked = model::now();
        }
        Ok(())
    }

    fn recheck(&mut self) -> Result<()> {
        let mut queue: Vec<_> = self
            .cache
            .issues
            .iter()
            .map(|(key, issue)| (issue.checked, key.clone(), issue.url.clone()))
            .collect();
        queue.sort_unstable();
        for (_, key, url) in queue.into_iter().take(CHECKS) {
            let (_, endpoint) = issue_url(self.forge, &url)?;
            // Public-only detail reads preserve the discovery visibility boundary.
            match self.get(endpoint, None)? {
                Some((value, _)) => self.update(value, Some(&key))?,
                None => {
                    let issue = self
                        .cache
                        .issues
                        .get_mut(&key)
                        .context("cache entry disappeared")?;
                    issue.state = Availability::Unreachable;
                    issue.checked = model::now();
                }
            }
        }
        Ok(())
    }

    fn discover(&mut self) -> Result<String> {
        let token = match self.forge {
            Forge::GitHub => std::env::var("GITHUB_TOKEN").ok(),
            Forge::GitLab => Some(storage::gitlab_token()?),
            Forge::Codeberg => None,
        };
        let mut scanned = 0;
        let mut incomplete = false;
        let mut public_checks = 0;
        let mut seen = HashSet::new();
        let mut completed = HashSet::new();
        let mut term_index =
            self.cache.progress.entry(self.forge).or_default().next_term % TERMS.len();

        for _ in 0..SEARCH_REQUESTS_PER_CYCLE {
            for _ in 0..TERMS.len() {
                if !completed.contains(TERMS[term_index]) {
                    break;
                }
                term_index = (term_index + 1) % TERMS.len();
            }
            if completed.len() == TERMS.len() {
                break;
            }
            let term = TERMS[term_index];
            term_index = (term_index + 1) % TERMS.len();
            let progress = self.cache.progress.entry(self.forge).or_default();
            let task = progress
                .terms
                .get(term)
                .and_then(|queue| queue.last())
                .cloned()
                .unwrap_or_default();
            let base = search(self.forge, term, 1, task.range);
            let url = if let Some(raw) = &task.next_url {
                checked_page_url(&base, raw)?
            } else {
                base.clone()
            };
            let (mut value, headers) = self
                .get(url.clone(), token.as_deref())?
                .context("site-wide issue search endpoint unavailable")?;
            let mut split = None;
            let mut capped = false;
            let items = if self.forge == Forge::GitHub {
                let timed_out = value["incomplete_results"].as_bool() == Some(true);
                incomplete |= timed_out;
                if value["total_count"].as_u64().is_some_and(|n| n > 1000) || timed_out {
                    let [start, end] = task.range.unwrap_or([0, model::now() / DAY]);
                    if start < end {
                        let middle = start + (end - start) / 2;
                        split = Some([[start, middle], [middle + 1, end]]);
                    } else {
                        capped = true;
                    }
                }
                value["items"].take()
            } else {
                value
            };
            let items = items.as_array().context("expected issue list")?;
            scanned += items.len();
            if self.forge == Forge::GitLab {
                // Work out the full page's verification cost before accepting any progress.
                let mut candidates = Vec::new();
                let mut ids = HashSet::new();
                for entry in items {
                    let candidate =
                        serde_json::from_value::<Remote>(entry.clone())?.into_issue(self.forge)?;
                    if let Some(issue) = candidate.filter(|issue| issue.reward.is_some()) {
                        let key = issue.key();
                        if !seen.contains(&key) && ids.insert(key) {
                            candidates.push(issue);
                        }
                    }
                }
                if public_checks + candidates.len() > GITLAB_PUBLIC_CHECKS {
                    break;
                }
                for issue in candidates {
                    let (_, endpoint) = issue_url(self.forge, &issue.url)?;
                    public_checks += 1;
                    if let Some((public, _)) = self.get(endpoint, None)? {
                        self.update(public, None)?;
                    }
                    seen.insert(issue.key());
                }
            } else {
                for entry in items {
                    self.update(entry.clone(), None)?;
                }
            }

            let next = if split.is_some() {
                None
            } else {
                next_page(self.forge, &base, &url, &headers)?
            };
            let progress = self.cache.progress.entry(self.forge).or_default();
            progress.next_term = term_index;
            if task.range.is_none() && task.next_url.is_none() {
                progress.blocked_ranges.remove(term);
            }
            let queue = progress.terms.entry(term.into()).or_default();
            queue.pop();
            if let Some(ranges) = split {
                queue.extend(ranges.into_iter().map(|range| SearchPage {
                    range: Some(range),
                    next_url: None,
                }));
            } else if let Some(next) = next {
                queue.push(SearchPage {
                    next_url: Some(next.into()),
                    ..task
                });
            }
            if capped {
                progress.blocked_ranges.insert(term.into(), true);
            }
            if queue.is_empty() {
                completed.insert(term);
            }
            let message = format!("Site-wide search scanned {scanned} results · synchronizing");
            self.cache.status.entry(self.forge).or_default().message = message.clone();
            (self.commit)(self.cache, message)?;
            if self.cache.status[&self.forge].retry_at > model::now() {
                break;
            }
        }

        let progress = self.cache.progress.entry(self.forge).or_default();
        let mut reasons = Vec::new();
        if completed.len() != TERMS.len() {
            reasons.push("continuation saved for the next cycle");
        }
        if incomplete {
            reasons.push("incomplete API results");
        }
        if !progress.blocked_ranges.is_empty() {
            reasons.push("a GitHub date partition exceeds provider coverage");
        }
        let coverage = if reasons.is_empty() {
            String::new()
        } else {
            format!(" (partial coverage: {})", reasons.join(", "))
        };
        Ok(format!(
            "Site-wide search scanned {scanned} results{coverage}"
        ))
    }
}

const DAY: u64 = 86_400;

fn utc_date(day: u64) -> String {
    let stamp = httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(day * DAY));
    let parts: Vec<_> = stamp.split_ascii_whitespace().collect();
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| *name == parts[2])
    .expect("HTTP date month")
        + 1;
    format!("{}-{month:02}-{}", parts[3], parts[1])
}

fn checked_page_url(base: &Url, raw: &str) -> Result<Url> {
    let url = base.join(raw)?;
    ensure!(
        url.origin() == base.origin()
            && url.path() == base.path()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "unexpected pagination link"
    );
    for (key, value) in base.query_pairs().filter(|(key, _)| key != "page") {
        ensure!(
            url.query_pairs()
                .any(|(actual_key, actual_value)| actual_key == key && actual_value == value),
            "pagination link changed the search"
        );
    }
    Ok(url)
}

fn page_number(url: &Url) -> Result<usize> {
    url.query_pairs()
        .find(|(key, _)| key == "page")
        .context("missing page number")?
        .1
        .parse()
        .context("invalid page number")
}

fn next_page(forge: Forge, base: &Url, current: &Url, headers: &HeaderMap) -> Result<Option<Url>> {
    let current_page = page_number(current)?;
    if forge == Forge::GitHub && current_page >= GITHUB_MAX_PAGE {
        return Ok(None);
    }
    for link in headers.get_all("link").iter() {
        let mut remaining = link.to_str()?;
        while let Some((_, uri)) = remaining.split_once('<') {
            let Some((target, rest)) = uri.split_once('>') else {
                break;
            };
            let (params, tail) = rest.split_once(',').unwrap_or((rest, ""));
            remaining = tail;
            if params.split(';').any(|param| {
                param.trim().strip_prefix("rel=").is_some_and(|rel| {
                    rel.trim_matches('"')
                        .split_whitespace()
                        .any(|r| r == "next")
                })
            }) {
                let target = checked_page_url(base, target)?;
                let page = page_number(&target)?;
                ensure!(page > current_page, "pagination did not advance");
                return Ok((forge != Forge::GitHub || page <= GITHUB_MAX_PAGE).then_some(target));
            }
        }
    }
    if forge == Forge::GitLab {
        if let Some(next) = headers.get("x-next-page") {
            let next = next.to_str()?;
            if !next.is_empty() && next != "0" {
                let page: usize = next.parse().context("invalid GitLab next page")?;
                ensure!(page > current_page, "GitLab pagination did not advance");
                let mut url = base.clone();
                let query: Vec<_> = base
                    .query_pairs()
                    .filter(|(key, _)| key != "page")
                    .collect();
                url.query_pairs_mut()
                    .clear()
                    .extend_pairs(query)
                    .append_pair("page", next);
                return Ok(Some(url));
            }
        }
    }
    if forge == Forge::Codeberg
        && headers
            .get("x-total-count")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|total| total > current_page.saturating_mul(PAGE))
    {
        let page = current_page
            .checked_add(1)
            .context("Codeberg page overflow")?;
        let mut url = base.clone();
        let query: Vec<_> = base
            .query_pairs()
            .filter(|(key, _)| key != "page")
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(query)
            .append_pair("page", &page.to_string());
        return Ok(Some(url));
    }
    Ok(None)
}

fn search(forge: Forge, term: &str, page: usize, range: Option<[u64; 2]>) -> Url {
    let mut url = match forge {
        Forge::GitHub => api(forge, &["search", "issues"]),
        Forge::GitLab => api(forge, &["search"]),
        Forge::Codeberg => api(forge, &["repos", "issues", "search"]),
    };
    let mut query = url.query_pairs_mut();
    query.append_pair("page", &page.to_string()).append_pair(
        if forge == Forge::Codeberg {
            "limit"
        } else {
            "per_page"
        },
        &(if forge == Forge::GitHub {
            GITHUB_PAGE_SIZE
        } else if forge == Forge::GitLab {
            GITLAB_PUBLIC_CHECKS
        } else {
            PAGE
        })
        .to_string(),
    );
    match forge {
        Forge::GitHub => {
            let mut terms =
                format!("{term} in:title,body is:issue is:open is:public archived:false");
            if let Some([start, end]) = range {
                terms.push_str(&format!(" created:{}..{}", utc_date(start), utc_date(end)));
            }
            query
                .append_pair("q", &terms)
                .append_pair("sort", "updated")
                .append_pair("order", "desc");
        }
        Forge::GitLab => {
            query
                .append_pair("scope", "issues")
                .append_pair("search", term)
                .append_pair("confidential", "false")
                .append_pair("state", "opened");
        }
        Forge::Codeberg => {
            query
                .append_pair("q", term)
                .append_pair("state", "open")
                .append_pair("type", "issues");
        }
    }
    drop(query);
    url
}

pub fn run(root: &Path, report: impl Fn(String) + Sync) -> Result<()> {
    let _lock = storage::lock(root)?;
    storage::prepare(root)?;
    let cache = Mutex::new(storage::load(root)?);
    let client = Client::builder()
        .user_agent("forge-bounties/0.1")
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .build()?;
    std::thread::scope(|scope| {
        let workers = Forge::ALL.map(|forge| {
            let cache = &cache;
            let report = &report;
            let client = client.clone();
            scope.spawn(move || run_forge(root, cache, report, client, forge))
        });
        let mut degraded = false;
        for (forge, worker) in Forge::ALL.into_iter().zip(workers) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    report(format!("{forge:?} · {error:#}"));
                    degraded = true;
                }
                Err(_) => {
                    report(format!("{forge:?} · worker stopped unexpectedly"));
                    degraded = true;
                }
            }
        }
        ensure!(
            !degraded,
            "refresh finished with deferred or failed operations"
        );
        Ok(())
    })
}

fn run_forge(
    root: &Path,
    shared: &Mutex<Cache>,
    report: &dyn Fn(String),
    client: Client,
    forge: Forge,
) -> Result<()> {
    let mut cache = Cache::default();
    {
        let combined = shared.lock().map_err(|_| anyhow!("cache writer stopped"))?;
        cache.issues.extend(
            combined
                .issues
                .iter()
                .filter(|(_, issue)| issue.forge == forge)
                .map(|(key, issue)| (key.clone(), issue.clone())),
        );
        cache
            .status
            .extend(combined.status.get(&forge).cloned().map(|s| (forge, s)));
        cache
            .progress
            .extend(combined.progress.get(&forge).cloned().map(|p| (forge, p)));
    }
    let commit = |local: &Cache, message: String| -> Result<()> {
        {
            let mut combined = shared.lock().map_err(|_| anyhow!("cache writer stopped"))?;
            storage::commit_forge(root, &combined, local, forge)?;
            combined.issues.retain(|_, issue| issue.forge != forge);
            combined.issues.extend(local.issues.clone());
            combined.status.extend(local.status.clone());
            combined.progress.extend(local.progress.clone());
        }
        report(format!("{forge:?} · {message}"));
        Ok(())
    };

    let now = model::now();
    let status = cache.status.entry(forge).or_default();
    ensure!(
        status.retry_at <= now
            && (status.attempted == 0 || now.saturating_sub(status.attempted) >= 300),
        "refresh deferred by cooldown"
    );
    *status = Status {
        attempted: now,
        message: "Refresh interrupted or in progress".into(),
        retry_at: 0,
    };
    commit(&cache, "Refresh started".into())?;
    let mut session = Session {
        client,
        cache: &mut cache,
        forge,
        commit: &commit,
    };
    let checks = session.recheck();
    let discovery = if session.cache.status[&forge].retry_at > model::now() {
        Err(anyhow!("discovery deferred by cooldown"))
    } else {
        session.discover()
    };
    let mut failed = false;
    let mut message = match discovery {
        Ok(message) => message,
        Err(error) => {
            failed = true;
            format!("Discovery failed: {error:#}")
        }
    };
    if let Err(error) = checks {
        failed = true;
        message.push_str(&format!(" | Recheck failed: {error:#}"));
    }
    cache.status.entry(forge).or_default().message = message.clone();
    commit(&cache, message)?;
    ensure!(!failed, "provider refresh did not finish");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_gitlab_nested_project_requests() {
        let (_, url) = issue_url(
            Forge::GitLab,
            "https://gitlab.com/group/sub/project/-/issues/7",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproject/issues/7"
        );
    }

    #[test]
    fn restricts_issue_origins() {
        for raw in [
            "http://github.com/o/r/issues/1",
            "https://github.com.evil.test/o/r/issues/1",
            "https://github.com:8443/o/r/issues/1",
            "https://github.com/o/r/pull/1",
        ] {
            assert!(issue_url(Forge::GitHub, raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn github_follows_links_and_stops_at_documented_cap() {
        let start = search(Forge::GitHub, "bounty", 1, None);
        assert!(
            start
                .query_pairs()
                .any(|(k, v)| k == "per_page" && v == "100")
        );
        let second = search(Forge::GitHub, "bounty", 2, None);
        let mut headers = HeaderMap::new();
        headers.insert("link", format!("<{second}>; rel=\"next\"").parse().unwrap());
        assert_eq!(
            next_page(Forge::GitHub, &start, &start, &headers).unwrap(),
            Some(second)
        );
        let last = search(Forge::GitHub, "bounty", GITHUB_MAX_PAGE, None);
        assert!(
            next_page(Forge::GitHub, &start, &last, &headers)
                .unwrap()
                .is_none()
        );
        assert!(checked_page_url(&start, "https://example.com/search/issues?page=2").is_err());
    }

    #[test]
    fn date_partitions_use_inclusive_utc_days() {
        let query = search(Forge::GitHub, "bounty", 1, Some([0, 1]));
        let terms = query.query_pairs().find(|(key, _)| key == "q").unwrap().1;
        assert!(terms.ends_with("created:1970-01-01..1970-01-02"));
    }

    #[test]
    fn builds_search_urls_for_each_forge() {
        for (forge, path) in [
            (Forge::GitHub, "/search/issues"),
            (Forge::GitLab, "/api/v4/search"),
            (Forge::Codeberg, "/api/v1/repos/issues/search"),
        ] {
            let url = search(forge, "bounty", 2, None);
            assert_eq!(url.path(), path);
            assert!(url.query_pairs().any(|(k, v)| k == "page" && v == "2"));
            if forge == Forge::GitLab {
                assert!(
                    url.query_pairs()
                        .any(|(k, v)| k == "scope" && v == "issues")
                );
                assert!(
                    url.query_pairs()
                        .any(|(k, v)| k == "confidential" && v == "false")
                );
            }
        }
    }
}
