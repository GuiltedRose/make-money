use crate::{
    model::{
        self, Availability, Cache, DiscoveryCounts, Forge, Issue, PublicRepository, RewardEvidence,
        SearchPage, Status,
    },
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
const RECENT_REQUESTS_PER_CYCLE: usize = 3;
const GITLAB_HISTORY_PAGE: usize = 9;
const GITLAB_RECENT_PAGE: usize = 3;
const CODEBERG_AUTH_RECHECKS: usize = 2;
const CODEBERG_AUTH_PUBLIC_CHECKS: usize = CHECKS - CODEBERG_AUTH_RECHECKS;
const GITHUB_PAGE_SIZE: usize = 100;
const GITHUB_MAX_PAGE: usize = 1000 / GITHUB_PAGE_SIZE;
const CHECKS: usize = 8;
const GITLAB_PUBLIC_CHECKS: usize = 18;
const TERMS: [&str; 3] = ["bounty", "reward", "payout"];

const GITHUB_TERMS: &[&str] = &[
    "bounty",
    "reward",
    "payout",
    "label:bounty",
    r#"label:"paid issue""#,
    r#""paid issue""#,
    r#""algora.io""#,
    r#""polar.sh""#,
    r#""issuehunt.io""#,
    r#""bountysource.com""#,
    r#""reverse engineering" bounty"#,
];

fn discovery_terms(forge: Forge) -> &'static [&'static str] {
    if forge == Forge::GitHub {
        GITHUB_TERMS
    } else {
        &TERMS
    }
}

fn recent_term(terms: &[&'static str], pass: usize, cursor: usize) -> (&'static str, usize) {
    if pass == 0 {
        (terms[0], cursor)
    } else {
        let index = cursor % (terms.len() - 1);
        (terms[index + 1], index + 1)
    }
}

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
    fn into_issue(self, forge: Forge) -> Result<Option<(Issue, bool)>> {
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
        let evidence = model::reward_evidence(&self.title, &body, &labels);
        let ambiguous = evidence == RewardEvidence::Ambiguous;
        let reward = evidence.into_reward();
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
        Ok(Some((
            Issue {
                forge,
                provider_id: self.id,
                title: self.title,
                url: url.into(),
                labels,
                state,
                reward,
                checked: model::now(),
                body: body.chars().take(12_000).collect(),
            },
            ambiguous,
        )))
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
        let bucket = request_bucket(self.forge, &url, token.is_some());
        let now = model::now();
        let status = &self.cache.status[&self.forge];
        ensure!(
            status.retry_at <= now,
            "provider cooldown is active; pagination progress retained"
        );
        if let Some(rate) = status.rate_buckets.get(bucket) {
            ensure!(
                rate.retry_at <= now,
                "{bucket} API cooldown active for {} min; pagination progress retained",
                (rate.retry_at.saturating_sub(now)).div_ceil(60)
            );
        }
        let mut request = self.client.get(url).header("Accept", "application/json");
        if let Some(token) = token {
            request = if self.forge == Forge::Codeberg {
                let mut authorization =
                    reqwest::header::HeaderValue::from_str(&format!("token {token}"))
                        .context("invalid Codeberg token format")?;
                authorization.set_sensitive(true);
                request.header(reqwest::header::AUTHORIZATION, authorization)
            } else {
                request.bearer_auth(token)
            };
        }
        let mut response = request.send().map_err(reqwest::Error::without_url)?;
        let headers = response.headers().clone();
        let status = response.status();
        let secondary = if self.forge == Forge::GitHub
            && status == StatusCode::FORBIDDEN
            && !headers.contains_key("retry-after")
            && remaining(&headers) != Some(0)
        {
            let mut bytes = Vec::new();
            response.by_ref().take(8192).read_to_end(&mut bytes).is_ok()
                && github_secondary_limit(&bytes)
        } else {
            false
        };
        let state = self.cache.status.entry(self.forge).or_default();
        if let Some((scope, retry_at)) = observe_rate(
            self.forge,
            bucket,
            state,
            status,
            &headers,
            secondary,
            model::now(),
        ) {
            let message = format!(
                "{scope} cooldown · retry in {} min",
                retry_at.saturating_sub(model::now()).div_ceil(60)
            );
            (self.commit)(self.cache, message.clone())?;
            if !status.is_success() {
                return Err(anyhow!("HTTP {} · {message}", status.as_u16()));
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
        self.store_issue(issue.map(|(issue, _)| issue), previous);
        Ok(())
    }

    fn store_issue(&mut self, issue: Option<Issue>, previous: Option<&str>) {
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
    }

    fn recheck(&mut self, limit: usize) -> Result<()> {
        let mut queue: Vec<_> = self
            .cache
            .issues
            .iter()
            .map(|(key, issue)| (issue.checked, key.clone(), issue.url.clone()))
            .collect();
        queue.sort_unstable();
        for (_, key, url) in queue.into_iter().take(limit) {
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

    fn discover_public_repositories(&mut self, budget: usize, reason: &str) -> Result<String> {
        let mut counts = DiscoveryCounts::default();
        let mut seen = HashSet::new();
        let mut visited = HashSet::new();
        let mut failures = Vec::new();
        let mut catalog_pages = 0;
        let mut issue_pages = 0;
        let mut unavailable_repositories = 0;
        let current_day = model::now() / DAY;
        let catalog_requests =
            if self.cache.progress[&self.forge].catalog_next.is_some() && budget >= 3 {
                2
            } else {
                1
            };
        for pass in 0..budget {
            let catalog = pass < catalog_requests;
            let step = if catalog {
                self.catalog_page(current_day, pass == 1).map(|()| true)
            } else {
                let repos = self.cache.repositories.entry(self.forge).or_default();
                let Some(repo) =
                    select_repository(repos.values(), &visited, pass == catalog_requests).cloned()
                else {
                    break;
                };
                visited.insert(repo.path.clone());
                self.repository_issue_page(&repo, &mut seen, &mut counts)
            };
            match step {
                Ok(scanned) => {
                    if catalog {
                        catalog_pages += 1;
                    } else if scanned {
                        issue_pages += 1;
                    } else {
                        unavailable_repositories += 1;
                    }
                    let message =
                        format!("Anonymous repository discovery · {counts} · synchronizing");
                    let status = self.cache.status.entry(self.forge).or_default();
                    status.counts = counts.clone();
                    status.message = message.clone();
                    (self.commit)(self.cache, message)?;
                }
                Err(error) => {
                    failures.push(error.to_string());
                    if self.cache.status[&self.forge].retry_at > model::now() || !catalog {
                        break;
                    }
                }
            }
        }
        if catalog_pages == 0 && issue_pages == 0 && !failures.is_empty() {
            return Err(anyhow!(
                "Anonymous public repository discovery unavailable ({reason}): {}",
                failures.join("; ")
            ));
        }
        let progress = &self.cache.progress[&self.forge];
        let repository_count = self
            .cache
            .repositories
            .get(&self.forge)
            .map_or(0, |repos| repos.len());
        let catalog_state = if progress.catalog_limited {
            "provider catalog limit or incomplete catalog response"
        } else if progress.catalog_next.is_some() {
            "catalog continuation saved"
        } else if catalog_pages == 0 {
            "catalog not advanced"
        } else {
            "catalog page exhausted"
        };
        let errors = if failures.is_empty() {
            String::new()
        } else {
            format!(" · {}", failures.join("; "))
        };
        Ok(format!(
            "Anonymous repository discovery ({reason}) · partial coverage · {repository_count} indexed · {catalog_pages} catalog pages · {issue_pages} issue pages · {unavailable_repositories} unavailable repos · {catalog_state} · {counts}{errors}"
        ))
    }

    fn catalog_page(&mut self, today: u64, head_only: bool) -> Result<()> {
        let progress = &self.cache.progress[&self.forge];
        let day = if head_only {
            today
        } else {
            progress.catalog_day.unwrap_or(today)
        };
        let base = repository_catalog(self.forge, day)?;
        let url = if head_only {
            base.clone()
        } else {
            progress
                .catalog_next
                .as_deref()
                .map(|raw| checked_page_url(&base, raw))
                .transpose()?
                .unwrap_or_else(|| base.clone())
        };
        let (value, headers) = self
            .get(url.clone(), None)?
            .context("public repository catalog unavailable")?;
        let entries = catalog_entries(self.forge, &value)?;
        let next = next_page(self.forge, &base, &url, &headers)?;
        let now = model::now();
        let repositories = self.cache.repositories.entry(self.forge).or_default();
        for entry in entries {
            if let Some((path, activity)) = public_repository(self.forge, entry) {
                let provider_id = entry["id"].as_u64();
                repositories
                    .entry(path.clone())
                    .and_modify(|repo| {
                        repo.provider_id = provider_id.or(repo.provider_id);
                        if activity > repo.activity {
                            repo.activity = activity.clone();
                        }
                    })
                    .or_insert_with(|| PublicRepository {
                        path,
                        provider_id,
                        activity,
                        added_at: now,
                        ..PublicRepository::default()
                    });
            }
        }
        if !head_only {
            let progress = self.cache.progress.entry(self.forge).or_default();
            progress.catalog_day = next.as_ref().map(|_| day);
            progress.catalog_limited = self.forge == Forge::GitHub
                && (value["incomplete_results"].as_bool() == Some(true)
                    || value["total_count"]
                        .as_u64()
                        .is_some_and(|total| total > 1000));
            progress.catalog_next = next.map(Into::into);
        }
        Ok(())
    }

    fn repository_issue_page(
        &mut self,
        repo: &PublicRepository,
        seen: &mut HashSet<String>,
        counts: &mut DiscoveryCounts,
    ) -> Result<bool> {
        let base = repository_issues(self.forge, &repo.path, repo.provider_id)?;
        let url = repo
            .issue_next
            .as_deref()
            .map(|raw| checked_page_url(&base, raw))
            .transpose()?
            .unwrap_or_else(|| base.clone());
        let response = self.get(url.clone(), None)?;
        let scanned = response.is_some();
        let next = if let Some((value, headers)) = response {
            let entries = value.as_array().context("expected public issue list")?;
            let mut page_seen = HashSet::new();
            for issue in decode_page(self.forge, entries, seen, &mut page_seen, counts) {
                counts.accepted_candidates += usize::from(issue.reward.is_some());
                self.store_issue(Some(issue), None);
            }
            seen.extend(page_seen);
            next_page(self.forge, &base, &url, &headers)?
        } else {
            // A removed or newly private repository contributes no current public results.
            None
        };
        if let Some(repo) = self
            .cache
            .repositories
            .entry(self.forge)
            .or_default()
            .get_mut(&repo.path)
        {
            repo.scanned_at = model::now();
            repo.issue_next = next.map(Into::into);
        }
        Ok(scanned)
    }

    fn discover(&mut self, root: &Path, codeberg_token: Option<&str>) -> Result<String> {
        let token = match self.forge {
            Forge::GitHub => std::env::var("GITHUB_TOKEN").ok(),
            Forge::GitLab => storage::credential("GITLAB_TOKEN")?,
            Forge::Codeberg => codeberg_token.map(str::to_owned),
        };
        if self.forge == Forge::GitLab && token.is_none() {
            return self.discover_public_repositories(
                SEARCH_REQUESTS_PER_CYCLE,
                "GitLab global search requires authentication",
            );
        }
        if self
            .cache
            .progress
            .entry(self.forge)
            .or_default()
            .catalog_fallback_pending
        {
            let result = self.discover_public_repositories(
                SEARCH_REQUESTS_PER_CYCLE,
                "resuming fallback after the previous global failure",
            );
            if result.is_ok() {
                self.cache
                    .progress
                    .entry(self.forge)
                    .or_default()
                    .catalog_fallback_pending = false;
            }
            return result;
        }
        let mut counts = DiscoveryCounts::default();
        let mut incomplete = false;
        let mut public_checks = 0;
        let mut seen = HashSet::new();
        let mut completed = HashSet::new();
        let terms = discovery_terms(self.forge);
        let progress = self.cache.progress.entry(self.forge).or_default();
        let mut term_index = progress.next_term % terms.len();
        let mut recent_index = progress.next_recent;
        let today = model::now() / DAY;
        for pass in 0..SEARCH_REQUESTS_PER_CYCLE {
            let recent = pass < RECENT_REQUESTS_PER_CYCLE;
            let term = if recent {
                let (term, next) = recent_term(terms, pass, recent_index);
                recent_index = next;
                term
            } else {
                while completed.len() < terms.len() && completed.contains(terms[term_index]) {
                    term_index = (term_index + 1) % terms.len();
                }
                if completed.len() == terms.len() {
                    break;
                }
                let term = terms[term_index];
                term_index = (term_index + 1) % terms.len();
                term
            };
            let key = term.to_owned();
            let progress = self.cache.progress.entry(self.forge).or_default();
            let task = if recent {
                SearchPage::default()
            } else {
                progress
                    .terms
                    .get(&key)
                    .and_then(|queue| queue.last())
                    .cloned()
                    .unwrap_or_default()
            };
            let mut base = search(self.forge, term, 1, task.range);
            if recent {
                base = recent_search(self.forge, base, today.saturating_sub(7));
            }
            let authenticated_codeberg = self.forge == Forge::Codeberg && token.is_some();
            if authenticated_codeberg {
                set_query_pair(&mut base, "limit", if recent { "1" } else { "2" });
            }
            let url = match &task.next_url {
                Some(raw) if self.forge == Forge::GitLab => gitlab_history_url(&base, raw)?,
                Some(raw) if self.forge == Forge::Codeberg => codeberg_history_url(&base, raw)?,
                Some(raw) => checked_page_url(&base, raw)?,
                None => base.clone(),
            };
            let result = self.get(url.clone(), token.as_deref());
            let (mut value, headers) = match result {
                Ok(Some(page)) => page,
                result if global_unavailable(&result) => {
                    let reason = result
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "global issue search unavailable".into());
                    let budget = SEARCH_REQUESTS_PER_CYCLE - pass - 1;
                    if budget == 0 {
                        self.cache
                            .progress
                            .entry(self.forge)
                            .or_default()
                            .catalog_fallback_pending = true;
                        return Ok(format!(
                            "Global search unavailable ({reason}); anonymous repository fallback queued for next refresh · {counts}"
                        ));
                    }
                    return self.discover_public_repositories(budget, &reason);
                }
                result => result?.context("issue search endpoint unavailable")?,
            };
            let mut split = None;
            let mut capped = false;
            let items = if self.forge == Forge::GitHub {
                let timed_out = value["incomplete_results"].as_bool() == Some(true);
                incomplete |= timed_out;
                if !recent && (value["total_count"].as_u64().is_some_and(|n| n > 1000) || timed_out)
                {
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
            let mut page_seen = HashSet::new();
            let mut page_counts = DiscoveryCounts::default();
            let candidates =
                decode_page(self.forge, items, &seen, &mut page_seen, &mut page_counts);
            if self.forge == Forge::GitLab || authenticated_codeberg {
                let limit = if authenticated_codeberg {
                    CODEBERG_AUTH_PUBLIC_CHECKS
                } else {
                    GITLAB_PUBLIC_CHECKS
                };
                let payable = candidates
                    .iter()
                    .filter(|issue| issue.reward.is_some())
                    .count();
                if public_checks + payable > limit {
                    break;
                }
                for issue in candidates
                    .into_iter()
                    .filter(|issue| issue.reward.is_some())
                {
                    let (_, endpoint) = issue_url(self.forge, &issue.url)?;
                    public_checks += 1;
                    if let Some((public, _)) = self.get(endpoint, None)? {
                        match serde_json::from_value::<Remote>(public)
                            .map_err(anyhow::Error::from)
                            .and_then(|remote| remote.into_issue(self.forge))
                        {
                            Ok(Some((public, _))) if public.key() == issue.key() => {
                                page_counts.accepted_candidates +=
                                    usize::from(public.reward.is_some());
                                self.store_issue(Some(public), None);
                            }
                            Ok(None) => {}
                            _ => page_counts.invalid_entries += 1,
                        }
                    }
                }
            } else {
                for issue in candidates {
                    page_counts.accepted_candidates += usize::from(issue.reward.is_some());
                    self.store_issue(Some(issue), None);
                }
            }
            seen.extend(page_seen);
            counts.scanned += page_counts.scanned;
            counts.duplicates += page_counts.duplicates;
            counts.missing_rewards += page_counts.missing_rewards;
            counts.ambiguous_rewards += page_counts.ambiguous_rewards;
            counts.invalid_entries += page_counts.invalid_entries;
            counts.accepted_candidates += page_counts.accepted_candidates;

            let next = if recent || split.is_some() {
                None
            } else {
                next_page(self.forge, &base, &url, &headers)?
            };
            let progress = self.cache.progress.entry(self.forge).or_default();
            if recent {
                progress.next_recent = recent_index;
            } else {
                progress.next_term = term_index;
                if task.range.is_none() && task.next_url.is_none() {
                    progress.blocked_ranges.remove(&key);
                }
                let queue = progress.terms.entry(key.clone()).or_default();
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
                    progress.blocked_ranges.insert(key, true);
                }
                if queue.is_empty() {
                    completed.insert(term);
                }
            }
            let message = format!("Site-wide search · {counts} · synchronizing");
            let status = self.cache.status.entry(self.forge).or_default();
            status.message = message.clone();
            status.counts = counts.clone();
            (self.commit)(self.cache, message)?;
            let status = &self.cache.status[&self.forge];
            let bucket = request_bucket(self.forge, &url, token.is_some());
            let now = model::now();
            if status.retry_at > now
                || status
                    .rate_buckets
                    .get(bucket)
                    .is_some_and(|rate| rate.retry_at > now)
            {
                break;
            }
        }

        let progress = self.cache.progress.entry(self.forge).or_default();
        let mut reasons = Vec::new();
        if completed.len() != terms.len() {
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
        let mode = if token.is_none() {
            "Anonymous site-wide search"
        } else {
            "Authenticated site-wide search"
        };
        Ok(format!("{mode} · {counts}{coverage}"))
    }
}

fn select_repository<'a>(
    repositories: impl Iterator<Item = &'a PublicRepository>,
    visited: &HashSet<String>,
    prefer_resume: bool,
) -> Option<&'a PublicRepository> {
    repositories
        .filter(|repo| !visited.contains(&repo.path))
        .min_by(|a, b| {
            let priority =
                |repo: &PublicRepository| match (prefer_resume, repo.issue_next.is_some()) {
                    (true, true) | (false, false) => 0,
                    _ => 1,
                };
            priority(a)
                .cmp(&priority(b))
                .then(a.scanned_at.cmp(&b.scanned_at))
                .then_with(|| b.activity.cmp(&a.activity))
                .then_with(|| a.path.cmp(&b.path))
        })
}

fn repository_catalog(forge: Forge, day: u64) -> Result<Url> {
    match forge {
        Forge::GitHub => {
            let mut url = api(forge, &["search", "repositories"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("per_page", &GITHUB_PAGE_SIZE.to_string())
                .append_pair(
                    "q",
                    &format!(
                        "is:public archived:false pushed:>={}",
                        utc_date(day.saturating_sub(30))
                    ),
                )
                .append_pair("sort", "updated")
                .append_pair("order", "desc");
            Ok(url)
        }
        Forge::GitLab => {
            let mut url = api(forge, &["projects"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("per_page", &PAGE.to_string())
                .append_pair("visibility", "public")
                .append_pair("archived", "false")
                .append_pair("with_issues_enabled", "true")
                .append_pair("order_by", "last_activity_at")
                .append_pair("sort", "desc");
            Ok(url)
        }
        Forge::Codeberg => {
            let mut url = api(forge, &["repos", "search"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("limit", &PAGE.to_string())
                .append_pair("private", "false")
                .append_pair("sort", "updated")
                .append_pair("order", "desc");
            Ok(url)
        }
    }
}

fn repository_issues(forge: Forge, path: &str, provider_id: Option<u64>) -> Result<Url> {
    ensure!(
        storage::valid_path(path) && path.contains('/'),
        "invalid public repository path"
    );
    match forge {
        Forge::GitHub => {
            let id = provider_id
                .context("GitHub repository ID missing")?
                .to_string();
            let mut url = api(forge, &["repositories", &id, "issues"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("per_page", &PAGE.to_string())
                .append_pair("state", "open")
                .append_pair("sort", "updated")
                .append_pair("direction", "desc");
            Ok(url)
        }
        Forge::GitLab => {
            let mut url = api(forge, &["projects", path, "issues"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("per_page", &PAGE.to_string())
                .append_pair("state", "opened")
                .append_pair("order_by", "updated_at")
                .append_pair("sort", "desc");
            Ok(url)
        }
        Forge::Codeberg => {
            let (owner, repo) = path
                .split_once('/')
                .context("invalid Codeberg repository path")?;
            ensure!(!repo.contains('/'), "invalid Codeberg repository path");
            let mut url = api(forge, &["repos", owner, repo, "issues"]);
            url.query_pairs_mut()
                .append_pair("page", "1")
                .append_pair("limit", &PAGE.to_string())
                .append_pair("state", "open")
                .append_pair("type", "issues");
            Ok(url)
        }
    }
}

fn catalog_entries(forge: Forge, value: &Value) -> Result<&Vec<Value>> {
    let entries = match forge {
        Forge::GitHub => &value["items"],
        Forge::GitLab => value,
        Forge::Codeberg => &value["data"],
    };
    entries
        .as_array()
        .context("expected public repository array")
}

fn public_repository(forge: Forge, entry: &Value) -> Option<(String, String)> {
    if entry["archived"].as_bool() == Some(true) {
        return None;
    }
    let (path, activity) = match forge {
        Forge::GitHub
            if entry["private"].as_bool() == Some(false)
                && entry["has_issues"].as_bool() != Some(false)
                && entry["id"].as_u64().is_some() =>
        {
            (entry["full_name"].as_str()?, entry["pushed_at"].as_str())
        }
        Forge::GitLab
            if entry["visibility"]
                .as_str()
                .is_none_or(|visibility| visibility == "public")
                && entry["issues_enabled"].as_bool() != Some(false) =>
        {
            (
                entry["path_with_namespace"].as_str()?,
                entry["last_activity_at"].as_str(),
            )
        }
        Forge::Codeberg
            if entry["private"].as_bool() == Some(false)
                && entry["has_issues"].as_bool() != Some(false) =>
        {
            (entry["full_name"].as_str()?, entry["updated_at"].as_str())
        }
        _ => return None,
    };
    if !storage::valid_path(path) || !path.contains('/') {
        return None;
    }
    Some((path.into(), activity.unwrap_or_default().into()))
}

fn request_bucket(forge: Forge, url: &Url, authenticated: bool) -> &'static str {
    match forge {
        Forge::GitHub if url.path().starts_with("/search/") => "search",
        Forge::GitHub => "core",
        Forge::GitLab if authenticated => "authenticated",
        Forge::GitLab => "anonymous",
        Forge::Codeberg => "api",
    }
}

fn header_number(headers: &HeaderMap, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| headers.get(*name)?.to_str().ok()?.parse().ok())
}

fn remaining(headers: &HeaderMap) -> Option<u64> {
    header_number(headers, &["x-ratelimit-remaining", "ratelimit-remaining"])
}

fn github_secondary_limit(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    value["message"].as_str().is_some_and(|message| {
        let message = message.to_ascii_lowercase();
        message.contains("secondary rate limit") || message.contains("abuse detection")
    })
}

fn observe_rate(
    forge: Forge,
    bucket: &'static str,
    state: &mut Status,
    code: StatusCode,
    headers: &HeaderMap,
    secondary: bool,
    now: u64,
) -> Option<(&'static str, u64)> {
    let remaining = remaining(headers);
    let reset = header_number(headers, &["x-ratelimit-reset", "ratelimit-reset"]);
    let retry_after = headers.get("retry-after");
    let retry = retry_after
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .map(|seconds| now.saturating_add(seconds))
                .or_else(|| {
                    httpdate::parse_http_date(value)
                        .ok()?
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                })
        });
    let rate = state.rate_buckets.entry(bucket.into()).or_default();
    if remaining.is_some() {
        rate.remaining = remaining;
    }
    if reset.is_some() {
        rate.reset_at = reset;
    }
    let exhausted = remaining == Some(0);
    if !exhausted && retry_after.is_none() && code != StatusCode::TOO_MANY_REQUESTS && !secondary {
        if code.is_success() {
            rate.consecutive_backoffs = 0;
        }
        return None;
    }
    let gitlab_named_bucket = forge == Forge::GitLab
        && headers
            .get("ratelimit-name")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|name| match bucket {
                "authenticated" => name == "throttle_authenticated_api",
                "anonymous" => name == "throttle_unauthenticated_api",
                _ => false,
            });
    let scoped = exhausted && !secondary && (forge == Forge::GitHub || gitlab_named_bucket);
    let fallback = 3600_u64 << rate.consecutive_backoffs.min(4);
    rate.consecutive_backoffs = rate.consecutive_backoffs.saturating_add(1);
    let retry_at = retry
        .into_iter()
        .chain(reset.filter(|_| exhausted))
        .filter(|time| *time > now)
        .max()
        .unwrap_or_else(|| now.saturating_add(fallback));
    if scoped {
        rate.retry_at = rate.retry_at.max(retry_at);
        Some((bucket, rate.retry_at))
    } else {
        state.retry_at = state.retry_at.max(retry_at);
        Some(("provider", state.retry_at))
    }
}

fn decode_page(
    forge: Forge,
    entries: &[Value],
    seen: &HashSet<String>,
    page_seen: &mut HashSet<String>,
    counts: &mut DiscoveryCounts,
) -> Vec<Issue> {
    let mut issues = Vec::new();
    for entry in entries {
        counts.scanned += 1;
        match serde_json::from_value::<Remote>(entry.clone())
            .map_err(anyhow::Error::from)
            .and_then(|remote| remote.into_issue(forge))
        {
            Ok(Some((issue, ambiguous))) => {
                let key = issue.key();
                if seen.contains(&key) || !page_seen.insert(key) {
                    counts.duplicates += 1;
                    continue;
                }
                if ambiguous {
                    counts.ambiguous_rewards += 1;
                } else if issue.reward.is_none() {
                    counts.missing_rewards += 1;
                }
                issues.push(issue);
            }
            Ok(None) => {}
            Err(_) => counts.invalid_entries += 1,
        }
    }
    issues
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
    let mut url = base.join(raw)?;
    ensure!(
        url.origin() == base.origin()
            && url.path() == base.path()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "unexpected pagination link"
    );
    for (key, value) in base.query_pairs().filter(|(key, _)| key != "page") {
        let mut matches = url.query_pairs().filter(|(actual, _)| actual == &key);
        let supplied = matches.next().map(|(_, value)| value.into_owned());
        ensure!(
            matches.next().is_none()
                && supplied
                    .as_deref()
                    .is_none_or(|actual| actual == value.as_ref()),
            "pagination link changed the search"
        );
        drop(matches);
        if supplied.is_none() {
            url.query_pairs_mut().append_pair(&key, &value);
        }
    }
    Ok(url)
}

fn set_query_pair(url: &mut Url, key: &str, value: &str) {
    let query: Vec<_> = url
        .query_pairs()
        .filter(|(current, _)| current != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(query)
        .append_pair(key, value);
}

fn gitlab_history_url(base: &Url, raw: &str) -> Result<Url> {
    if let Ok(url) = checked_page_url(base, raw) {
        return Ok(url);
    }
    let mut legacy = base.clone();
    set_query_pair(&mut legacy, "per_page", &GITLAB_PUBLIC_CHECKS.to_string());
    let old = checked_page_url(&legacy, raw)?;
    let offset = page_number(&old)?
        .checked_sub(1)
        .and_then(|page| page.checked_mul(GITLAB_PUBLIC_CHECKS))
        .context("invalid GitLab offset")?;
    let page = (offset / GITLAB_HISTORY_PAGE)
        .checked_add(1)
        .context("GitLab page overflow")?;
    let mut url = base.clone();
    set_query_pair(&mut url, "page", &page.to_string());
    Ok(url)
}

fn codeberg_history_url(base: &Url, raw: &str) -> Result<Url> {
    if let Ok(url) = checked_page_url(base, raw) {
        return Ok(url);
    }
    let target = base.join(raw)?;
    let old_limit = target
        .query_pairs()
        .find(|(key, _)| key == "limit")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .filter(|limit| [PAGE, 2].contains(limit))
        .context("invalid Codeberg page size")?;
    let mut old_base = base.clone();
    set_query_pair(&mut old_base, "limit", &old_limit.to_string());
    let old = checked_page_url(&old_base, raw)?;
    let offset = page_number(&old)?
        .checked_sub(1)
        .and_then(|page| page.checked_mul(old_limit))
        .context("invalid Codeberg offset")?;
    let new_limit: usize = base
        .query_pairs()
        .find(|(key, _)| key == "limit")
        .context("missing Codeberg page size")?
        .1
        .parse()?;
    ensure!(new_limit > 0, "invalid Codeberg page size");
    let page = (offset / new_limit)
        .checked_add(1)
        .context("Codeberg page overflow")?;
    let mut url = base.clone();
    set_query_pair(&mut url, "page", &page.to_string());
    Ok(url)
}

fn global_unavailable(result: &Result<Option<(Value, HeaderMap)>>) -> bool {
    match result {
        Ok(None) => true,
        Err(error) => error
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|status| {
                status.is_server_error()
                    || matches!(
                        status,
                        StatusCode::BAD_REQUEST
                            | StatusCode::FORBIDDEN
                            | StatusCode::METHOD_NOT_ALLOWED
                    )
            }),
        _ => false,
    }
}

fn recent_search(forge: Forge, mut url: Url, since: u64) -> Url {
    match forge {
        Forge::GitHub => {
            if let Some((_, query)) = url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
            {
                set_query_pair(
                    &mut url,
                    "q",
                    &format!("{query} updated:>={}", utc_date(since)),
                );
            }
        }
        Forge::GitLab => set_query_pair(&mut url, "per_page", &GITLAB_RECENT_PAGE.to_string()),
        Forge::Codeberg => {
            url.query_pairs_mut()
                .append_pair("since", &format!("{}T00:00:00Z", utc_date(since)));
        }
    }
    url
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
    let github_search = forge == Forge::GitHub && base.path().starts_with("/search/");
    if github_search && current_page >= GITHUB_MAX_PAGE {
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
                return Ok((!github_search || page <= GITHUB_MAX_PAGE).then_some(target));
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
    let limit = base
        .query_pairs()
        .find(|(key, _)| key == "limit")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(PAGE);
    if forge == Forge::Codeberg
        && headers
            .get("x-total-count")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|total| total > current_page.saturating_mul(limit))
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
            GITLAB_HISTORY_PAGE
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
    let mut cache = storage::load(root)?;
    storage::load_repository_index(root, &mut cache)?;
    let cache = Mutex::new(cache);
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
        if let Some(repositories) = combined.repositories.get(&forge) {
            cache.repositories.insert(forge, repositories.clone());
        }
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
            if let Some(repositories) = local.repositories.get(&forge) {
                combined.repositories.insert(forge, repositories.clone());
            }
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
    status.attempted = now;
    status.message = "Refresh interrupted or in progress".into();
    status.retry_at = 0;
    status.counts = DiscoveryCounts::default();
    commit(&cache, "Refresh started".into())?;
    let mut session = Session {
        client,
        cache: &mut cache,
        forge,
        commit: &commit,
    };
    let codeberg_token = if forge == Forge::Codeberg {
        storage::credential("CODEBERG_TOKEN")
    } else {
        Ok(None)
    };
    let recheck_limit = if matches!(&codeberg_token, Ok(Some(_))) {
        CODEBERG_AUTH_RECHECKS
    } else {
        CHECKS
    };
    let checks = session.recheck(recheck_limit);
    let discovery = if session.cache.status[&forge].retry_at > model::now() {
        Err(anyhow!("discovery deferred by cooldown"))
    } else {
        codeberg_token.and_then(|token| session.discover(root, token.as_deref()))
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
    let status = cache.status.entry(forge).or_default();
    let now = model::now();
    for (bucket, rate) in &status.rate_buckets {
        if rate.retry_at > now {
            message.push_str(&format!(
                " | {bucket} retry in {} min",
                (rate.retry_at - now).div_ceil(60)
            ));
        }
    }
    status.message = message.clone();
    commit(&cache, message)?;
    ensure!(!failed, "provider refresh did not finish");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_restores_omitted_public_filters_but_rejects_conflicts() {
        let base = repository_catalog(Forge::Codeberg, 0).unwrap();
        let next = checked_page_url(&base, "?page=2&limit=20").unwrap();
        assert!(
            next.query_pairs()
                .any(|(k, v)| k == "private" && v == "false")
        );
        assert!(checked_page_url(&base, "?page=2&private=true").is_err());
        assert!(checked_page_url(&base, "?page=2&private=false&private=true").is_err());
    }

    #[test]
    fn github_anonymous_repository_fallback_uses_separate_quota_and_pagination() {
        let catalog = repository_catalog(Forge::GitHub, 40).unwrap();
        assert_eq!(catalog.path(), "/search/repositories");
        assert_eq!(request_bucket(Forge::GitHub, &catalog, false), "search");
        assert!(
            catalog
                .query_pairs()
                .any(|(k, v)| k == "q" && v.contains("is:public") && v.contains("pushed:>="))
        );
        let public = serde_json::json!({"full_name":"owner/project","id":18,"private":false,"pushed_at":"2026-01-01"});
        assert_eq!(
            public_repository(Forge::GitHub, &public).unwrap().0,
            "owner/project"
        );
        assert!(
            public_repository(
                Forge::GitHub,
                &serde_json::json!({"full_name":"owner/project","id":18,"private":true})
            )
            .is_none()
        );
        let base = repository_issues(Forge::GitHub, "owner/project", Some(18)).unwrap();
        assert_eq!(base.path(), "/repositories/18/issues");
        assert_eq!(request_bucket(Forge::GitHub, &base, false), "core");
        let mut current = base.clone();
        set_query_pair(&mut current, "page", "10");
        let mut following = base.clone();
        set_query_pair(&mut following, "page", "11");
        let mut headers = HeaderMap::new();
        headers.insert(
            "link",
            format!("<{following}>; rel=\"next\"").parse().unwrap(),
        );
        assert_eq!(
            next_page(Forge::GitHub, &base, &current, &headers).unwrap(),
            Some(following)
        );
    }

    #[test]
    fn gitlab_anonymous_public_catalog_and_nested_project_issues() {
        let catalog = repository_catalog(Forge::GitLab, 0).unwrap();
        assert_eq!(catalog.path(), "/api/v4/projects");
        assert!(
            catalog
                .query_pairs()
                .any(|(k, v)| k == "visibility" && v == "public")
        );
        let project = serde_json::json!({"path_with_namespace":"group/sub/project","visibility":"public","last_activity_at":"2026-01-01"});
        assert_eq!(
            public_repository(Forge::GitLab, &project).unwrap().0,
            "group/sub/project"
        );
        let issues = repository_issues(Forge::GitLab, "group/sub/project", None).unwrap();
        assert_eq!(
            issues.path(),
            "/api/v4/projects/group%2Fsub%2Fproject/issues"
        );
        assert!(
            public_repository(
                Forge::GitLab,
                &serde_json::json!({"path_with_namespace":"g/p","visibility":"private"})
            )
            .is_none()
        );
    }

    #[test]
    fn repository_scan_resumes_and_prioritizes_active_repositories() {
        let active = PublicRepository {
            path: "x/active".into(),
            activity: "2026".into(),
            ..Default::default()
        };
        let old = PublicRepository {
            path: "x/old".into(),
            activity: "2025".into(),
            issue_next: Some("page 2".into()),
            ..Default::default()
        };
        assert_eq!(
            select_repository([&old, &active].into_iter(), &HashSet::new(), false)
                .unwrap()
                .path,
            "x/active"
        );
        assert_eq!(
            select_repository([&old, &active].into_iter(), &HashSet::new(), true)
                .unwrap()
                .path,
            "x/old"
        );
    }

    #[test]
    fn rate_limits_isolate_github_search_and_honor_server_backoff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "200".parse().unwrap());
        let mut status = Status::default();
        assert_eq!(
            observe_rate(
                Forge::GitHub,
                "search",
                &mut status,
                StatusCode::OK,
                &headers,
                false,
                100
            ),
            Some(("search", 200))
        );
        assert_eq!(status.retry_at, 0);
        assert!(!status.rate_buckets.contains_key("core"));
        headers.clear();
        headers.insert("retry-after", "250".parse().unwrap());
        assert_eq!(
            observe_rate(
                Forge::GitHub,
                "core",
                &mut status,
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
                false,
                100
            ),
            Some(("provider", 350))
        );
    }

    #[test]
    fn gitlab_named_primary_and_codeberg_backoff_remain_distinct() {
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "0".parse().unwrap());
        headers.insert("ratelimit-reset", "200".parse().unwrap());
        headers.insert(
            "ratelimit-name",
            "throttle_authenticated_api".parse().unwrap(),
        );
        let mut status = Status::default();
        assert_eq!(
            observe_rate(
                Forge::GitLab,
                "authenticated",
                &mut status,
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
                false,
                100
            ),
            Some(("authenticated", 200))
        );
        assert_eq!(status.retry_at, 0);
        assert_eq!(
            observe_rate(
                Forge::Codeberg,
                "api",
                &mut status,
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
                false,
                100
            ),
            Some(("provider", 200))
        );
        assert!(!github_secondary_limit(
            br#"{"message":"Resource not accessible"}"#
        ));
        assert!(github_secondary_limit(
            br#"{"message":"You exceeded a secondary rate limit."}"#
        ));
    }

    #[test]
    fn codeberg_anonymous_catalog_public_issues_and_optional_auth_offset() {
        let catalog = repository_catalog(Forge::Codeberg, 0).unwrap();
        assert_eq!(catalog.path(), "/api/v1/repos/search");
        assert!(
            catalog
                .query_pairs()
                .any(|(k, v)| k == "private" && v == "false")
        );
        let data = serde_json::json!({"data":[{"full_name":"owner/project","private":false,"updated_at":"2026-01-01"}]});
        assert_eq!(
            public_repository(
                Forge::Codeberg,
                &catalog_entries(Forge::Codeberg, &data).unwrap()[0]
            )
            .unwrap()
            .0,
            "owner/project"
        );
        assert!(
            public_repository(
                Forge::Codeberg,
                &serde_json::json!({"full_name":"owner/project","private":true})
            )
            .is_none()
        );
        let scoped = repository_issues(Forge::Codeberg, "owner/project", None).unwrap();
        assert_eq!(scoped.path(), "/api/v1/repos/owner/project/issues");
        assert!(
            scoped
                .query_pairs()
                .any(|(k, v)| k == "state" && v == "open")
        );
        let old = search(Forge::Codeberg, "bounty", 3, None);
        let mut new = search(Forge::Codeberg, "bounty", 1, None);
        set_query_pair(&mut new, "limit", "2");
        assert_eq!(
            page_number(&codeberg_history_url(&new, old.as_str()).unwrap()).unwrap(),
            21
        );
        assert!(global_unavailable(&Ok(None)));
        assert!(!global_unavailable(&Err(anyhow!("provider cooldown"))));
    }

    #[test]
    fn targeted_github_queries_keep_public_issue_scope() {
        for term in [
            "label:bounty",
            r#"label:"paid issue""#,
            r#""algora.io""#,
            r#""reverse engineering" bounty"#,
        ] {
            assert!(discovery_terms(Forge::GitHub).contains(&term));
            assert!(!discovery_terms(Forge::GitLab).contains(&term));
            let url = search(Forge::GitHub, term, 1, None);
            assert!(url.query_pairs().any(|(k, v)| k == "q"
                && v.contains(term)
                && v.contains("is:public")
                && v.contains("is:issue")));
        }
    }

    #[test]
    fn targeted_queries_remain_public_and_preserve_recent_independence() {
        let terms = discovery_terms(Forge::GitHub);
        for (term, qualifier) in [
            ("label:bounty", "label:bounty"),
            (r#"label:"paid issue""#, "paid issue"),
            (r#""algora.io""#, "algora.io"),
            (r#""reverse engineering" bounty"#, "reverse engineering"),
        ] {
            assert!(terms.contains(&term));
            let history = search(Forge::GitHub, term, 4, Some([0, 1]));
            let recent = recent_search(Forge::GitHub, search(Forge::GitHub, term, 1, None), 1);
            assert!(recent.query_pairs().any(|(k, v)| k == "q"
                && v.contains("is:public")
                && v.contains(qualifier)
                && v.contains("updated:>=1970-01-02")));
            assert_eq!(page_number(&history).unwrap(), 4);
            assert_eq!(page_number(&recent).unwrap(), 1);
        }
        assert_eq!(recent_term(terms, 0, 8), ("bounty", 8));
    }

    #[test]
    fn gitlab_legacy_cursor_keeps_exact_offset() {
        let base = search(Forge::GitLab, "bounty", 1, None);
        let mut old = search(Forge::GitLab, "bounty", 2, None);
        set_query_pair(&mut old, "per_page", "18");
        let resumed = gitlab_history_url(&base, old.as_str()).unwrap();
        assert_eq!(page_number(&resumed).unwrap(), 3);
        assert!(
            resumed
                .query_pairs()
                .any(|(k, v)| k == "per_page" && v == "9")
        );
    }

    #[test]
    fn malformed_entries_do_not_discard_valid_neighbors() {
        let issue = serde_json::json!({
            "id": 12, "title": "[Bounty] $75", "state": "open",
            "html_url": "https://github.com/owner/project/issues/8"
        });
        let mut counts = DiscoveryCounts::default();
        let rows = decode_page(
            Forge::GitHub,
            &[serde_json::json!({}), issue.clone(), issue],
            &HashSet::new(),
            &mut HashSet::new(),
            &mut counts,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].reward.is_some());
        assert_eq!(
            (counts.scanned, counts.invalid_entries, counts.duplicates),
            (3, 1, 1)
        );
    }

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
