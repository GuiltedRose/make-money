use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub const FRESH: u64 = 24 * 3600;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Forge {
    GitHub,
    GitLab,
    Codeberg,
}

impl Forge {
    pub const ALL: [Self; 3] = [Self::GitHub, Self::GitLab, Self::Codeberg];
    pub fn host(self) -> &'static str {
        match self {
            Self::GitHub => "github.com",
            Self::GitLab => "gitlab.com",
            Self::Codeberg => "codeberg.org",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reward {
    pub currency: String,
    pub cents: u64,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub forge: Forge,
    #[serde(default)]
    pub provider_id: Option<u64>,
    pub title: String,
    pub url: String,
    pub body: String,
    pub labels: Vec<String>,
    pub checked: u64,
    pub state: Availability,
    pub reward: Option<Reward>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Availability {
    Open,
    Assigned,
    Stale,
    Closed,
    Unreachable,
}

impl Issue {
    pub fn key(&self) -> String {
        self.provider_id
            .map_or_else(|| self.url.clone(), |id| format!("{:?}:{id}", self.forge))
    }

    pub fn availability(&self, now: u64) -> Availability {
        if matches!(self.state, Availability::Open | Availability::Assigned)
            && now.saturating_sub(self.checked) >= FRESH
        {
            Availability::Stale
        } else {
            self.state
        }
    }

    pub fn explicitly_mentions_reversing(&self) -> bool {
        REVERSING.is_match(&self.title)
            || REVERSING.is_match(&self.body)
            || self.labels.iter().any(|label| {
                label.trim().eq_ignore_ascii_case("reversing") || REVERSING.is_match(label)
            })
    }

    pub fn difficulty(&self) -> Option<(u8, &str)> {
        self.labels
            .iter()
            .filter_map(|label| {
                let level = match label.to_ascii_lowercase().trim() {
                    "hard" | "advanced" | "difficulty: hard" | "complex" => 3,
                    "medium" | "intermediate" | "difficulty: medium" => 2,
                    "easy" | "difficulty: easy" | "good first issue" | "beginner" => 1,
                    _ => return None,
                };
                Some((level, label.as_str()))
            })
            .max_by_key(|(level, _)| *level)
    }
}

static REVERSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:reverse[-_\s]+engineer(?:ing|ed)?|reversing[-_\s]+(?:bounty|challenge|task))\b",
    )
    .expect("valid reverse engineering regex")
});

static OFFER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?i)^\s*(?:[-*]\s+)?(?:\*\*)?\[?(?:bounty|reward|payout)(?:\*\*)?\]?\s*[:=–—-]?\s*(?:\*\*)?\s*(?:(US\$|\$|USD|EUR|€|GBP|£)\s*)?((?:[0-9]{1,9}|[0-9]{1,3}(?:,[0-9]{3}){1,2})(?:\.[0-9]{1,2})?)\s*(USD|EUR|GBP)?\s*(?:\*\*)?\s*\]?\s*$"
).expect("valid reward regex")
});

pub fn reward(title: &str, body: &str, labels: &[String]) -> Option<Reward> {
    let mut result: Option<Reward> = None;
    let title_offer = title
        .strip_prefix('[')
        .and_then(|s| s.split_once(']'))
        .map(|(offer, _)| offer);
    for line in std::iter::once(title)
        .chain(title_offer)
        .chain(labels.iter().map(String::as_str))
        .chain(body.lines())
    {
        let Some(m) = OFFER.captures(line.trim().trim_start_matches("**")) else {
            continue;
        };
        let prefix = m
            .get(1)
            .map(|v| match v.as_str().to_ascii_uppercase().as_str() {
                "€" | "EUR" => "EUR",
                "£" | "GBP" => "GBP",
                "$" => "",
                _ => "USD",
            });
        let suffix = m.get(3).map(|v| v.as_str().to_ascii_uppercase());
        let currency = match (prefix, suffix.as_deref()) {
            (Some(""), Some("USD")) => "USD",
            (Some(""), None) => "$",
            (Some(value), None) if !value.is_empty() => value,
            (Some(value), Some(other)) if value == other => value,
            (None, Some(value)) => value,
            _ => continue,
        };
        let amount = m[2].replace(',', "");
        let (whole, fraction) = amount.split_once('.').unwrap_or((&amount, ""));
        let whole = whole.parse::<u64>().ok()?.checked_mul(100)?;
        let fraction = match fraction.len() {
            0 => 0,
            1 => fraction.parse::<u64>().ok()? * 10,
            _ => fraction.parse::<u64>().ok()?,
        };
        let cents = whole + fraction;
        if cents == 0 {
            continue;
        }
        if result
            .as_ref()
            .is_some_and(|r| r.currency != currency || r.cents != cents)
        {
            return None;
        }
        result = Some(Reward {
            currency: currency.into(),
            cents,
            evidence: line.trim().into(),
        });
    }
    result
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub attempted: u64,
    pub message: String,
    pub retry_at: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SearchPage {
    pub range: Option<[u64; 2]>,
    pub next_url: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SearchProgress {
    pub terms: BTreeMap<String, Vec<SearchPage>>,
    pub next_term: usize,
    pub blocked_ranges: BTreeMap<String, bool>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Cache {
    pub issues: BTreeMap<String, Issue>,
    pub status: BTreeMap<Forge, Status>,
    #[serde(default)]
    pub progress: BTreeMap<Forge, SearchProgress>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_amount_and_preserves_currency() {
        let r = reward("Fix rendering", "- **Reward:** US$ 1,200.50", &[]).unwrap();
        assert_eq!((r.currency.as_str(), r.cents), ("USD", 120_050));
        let r = reward("[Bounty] $75", "", &[]).unwrap();
        assert_eq!((r.currency.as_str(), r.cents), ("$", 7_500));
    }

    #[test]
    fn ignores_uncertain_amounts() {
        for text in [
            "Reward: 25",
            "Reward: $25 CAD",
            "Bounty: €10–€30",
            "Budget: USD 100",
            "Was reward: USD 200",
            "Reward: USD 12,34",
        ] {
            assert!(reward("", text, &[]).is_none(), "{text}");
        }
        assert!(reward("", "Reward: EUR 100\nReward: EUR 200", &[]).is_none());
    }

    #[test]
    fn expires_availability() {
        let issue = Issue {
            forge: Forge::GitHub,
            provider_id: None,
            title: String::new(),
            url: String::new(),
            body: String::new(),
            labels: vec![],
            checked: 1,
            state: Availability::Open,
            reward: None,
        };
        assert_eq!(issue.availability(FRESH), Availability::Open);
        assert_eq!(issue.availability(FRESH + 1), Availability::Stale);
    }
}
