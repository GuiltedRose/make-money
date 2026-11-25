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
    #[serde(default)]
    pub crypto_decimal: Option<String>,
    pub evidence: String,
}

impl Reward {
    pub const SCALE: u128 = 1_000_000_000_000_000_000;

    pub fn scaled(&self) -> u128 {
        self.crypto_decimal
            .as_deref()
            .and_then(decimal_units)
            .unwrap_or(u128::from(self.cents) * (Self::SCALE / 100))
    }

    pub fn amount_text(&self) -> String {
        self.crypto_decimal
            .clone()
            .unwrap_or_else(|| format!("{}.{:02}", self.cents / 100, self.cents % 100))
    }
}

fn decimal_units(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let whole = whole.parse::<u128>().ok()?.checked_mul(Reward::SCALE)?;
    let places = 18_u32.checked_sub(fraction.len().try_into().ok()?)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    whole.checked_add(fraction.checked_mul(10_u128.pow(places))?)
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

static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:bounty(?: amount)?|cash reward|reward|payout|paid issue|prize|compensation)(?:\s*[:=–—]\s*|\s*-\s+|\s+|$)").unwrap()
});
static AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:(US\$|CA\$|AU\$|NZ\$|\$|USD|EUR|GBP|CAD|AUD|NZD|CHF|USDC|USDT|SOL|ETH|BTC|€|£)\s*)?((?:[0-9]{1,9}|[0-9]{1,3}(?:,[0-9]{3}){1,2})(?:\.[0-9]{1,18})?)\s*(USD|EUR|GBP|CAD|AUD|NZD|CHF|USDC|USDT|SOL|ETH|BTC)?$").unwrap()
});
static AMOUNT_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^((?:[0-9$€£+-]|(?:US|CA|AU|NZ)\$|(?:USD|EUR|GBP|CAD|AUD|NZD|CHF|USDC|USDT|SOL|ETH|BTC)\b).+?)\s+(?:bounty|reward|payout)$").unwrap()
});

#[derive(Debug, PartialEq)]
pub enum RewardEvidence {
    Missing,
    Ambiguous,
    Stated(Reward),
}

impl RewardEvidence {
    pub fn into_reward(self) -> Option<Reward> {
        match self {
            Self::Stated(reward) => Some(reward),
            _ => None,
        }
    }
}

fn parse_amount(text: &str) -> Option<(&'static str, u64, Option<String>)> {
    let m = AMOUNT.captures(text)?;
    let currency = |text: &str| match text.to_ascii_uppercase().as_str() {
        "US$" | "USD" => "USD",
        "€" | "EUR" => "EUR",
        "£" | "GBP" => "GBP",
        "CA$" | "CAD" => "CAD",
        "AU$" | "AUD" => "AUD",
        "NZ$" | "NZD" => "NZD",
        "CHF" => "CHF",
        "USDC" => "USDC",
        "USDT" => "USDT",
        "SOL" => "SOL",
        "ETH" => "ETH",
        "BTC" => "BTC",
        _ => "$",
    };
    let prefix = m.get(1).map(|v| currency(v.as_str()));
    let suffix = m.get(3).map(|v| currency(v.as_str()));
    let currency = match (prefix, suffix) {
        (Some("$"), Some("USD")) => "USD",
        (Some(prefix), Some(suffix)) if prefix == suffix => prefix,
        (Some(currency), None) | (None, Some(currency)) => currency,
        _ => return None,
    };
    let amount = m[2].replace(',', "");
    let (whole, fraction) = amount.split_once('.').unwrap_or((&amount, ""));
    if matches!(currency, "USDC" | "USDT" | "SOL" | "ETH" | "BTC") {
        return (decimal_units(&amount)? > 0).then_some((currency, 0, Some(amount)));
    }
    if fraction.len() > 2 {
        return None;
    }
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.parse::<u64>().ok()?,
    };
    let cents = whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(fraction)?;
    (cents != 0).then_some((currency, cents, None))
}

fn declaration(line: &str) -> Option<String> {
    let line = line
        .trim()
        .trim_start_matches('💎')
        .trim_start_matches('\u{fe0f}')
        .trim_start();
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line);
    let line = line
        .replace("**", "")
        .replace("__", "")
        .replace('`', "")
        .replace("] ", " ");
    let line = line.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace());
    if let Some((label, amount)) = line.trim_matches('|').split_once('|') {
        let label = label.trim();
        if DECLARATION
            .find(label)
            .is_some_and(|m| m.end() == label.len())
        {
            return Some(amount.trim().into());
        }
    }
    if let Some(label) = DECLARATION.find(line) {
        return Some(line[label.end()..].trim().into());
    }
    AMOUNT_FIRST.captures(line).map(|m| m[1].into())
}

pub fn reward_evidence(title: &str, body: &str, labels: &[String]) -> RewardEvidence {
    let title_offer = title
        .strip_prefix('[')
        .and_then(|s| s.split_once(']'))
        .map(|(offer, _)| offer);
    let mut fenced = None;
    let lines = body.lines().filter(|line| {
        let text = line.trim_start();
        if let Some(marker) = ["```", "~~~"]
            .into_iter()
            .find(|marker| text.starts_with(marker))
        {
            if fenced == Some(marker) {
                fenced = None;
            } else if fenced.is_none() {
                fenced = Some(marker);
            }
            return false;
        }
        fenced.is_none() && !text.starts_with('>')
    });
    let mut result: Option<Reward> = None;
    for line in std::iter::once(title)
        .chain(title_offer)
        .chain(labels.iter().map(String::as_str))
        .chain(lines)
    {
        let Some(amount) = declaration(line) else {
            continue;
        };
        let Some((currency, cents, crypto_decimal)) = parse_amount(&amount) else {
            if amount.bytes().any(|c| c.is_ascii_digit()) {
                if line == title
                    && title_offer.is_some_and(|offer| {
                        declaration(offer).is_some_and(|amount| parse_amount(&amount).is_some())
                    })
                {
                    continue;
                }
                return RewardEvidence::Ambiguous;
            }
            continue;
        };
        let reward = Reward {
            currency: currency.into(),
            cents,
            crypto_decimal,
            evidence: line.trim().into(),
        };
        if result.as_ref().is_some_and(|previous| {
            previous.currency != currency || previous.scaled() != reward.scaled()
        }) {
            return RewardEvidence::Ambiguous;
        }
        result = Some(reward);
    }
    result.map_or(RewardEvidence::Missing, RewardEvidence::Stated)
}

#[cfg(test)]
pub fn reward(title: &str, body: &str, labels: &[String]) -> Option<Reward> {
    reward_evidence(title, body, labels).into_reward()
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryCounts {
    pub scanned: usize,
    pub duplicates: usize,
    pub missing_rewards: usize,
    pub ambiguous_rewards: usize,
    pub invalid_entries: usize,
    pub accepted_candidates: usize,
}

impl std::fmt::Display for DiscoveryCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} scanned, {} accepted, {} duplicate, {} missing reward, {} ambiguous, {} invalid",
            self.scanned,
            self.accepted_candidates,
            self.duplicates,
            self.missing_rewards,
            self.ambiguous_rewards,
            self.invalid_entries
        )
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RateBucket {
    pub remaining: Option<u64>,
    pub reset_at: Option<u64>,
    pub retry_at: u64,
    #[serde(default)]
    pub consecutive_backoffs: u32,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub attempted: u64,
    pub message: String,
    pub retry_at: u64,
    #[serde(default)]
    pub counts: DiscoveryCounts,
    #[serde(default)]
    pub rate_buckets: BTreeMap<String, RateBucket>,
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
    #[serde(default)]
    pub next_recent: usize,
    #[serde(default)]
    pub catalog_next: Option<String>,
    #[serde(default)]
    pub catalog_day: Option<u64>,
    #[serde(default)]
    pub catalog_limited: bool,
    #[serde(default)]
    pub catalog_fallback_pending: bool,
    pub blocked_ranges: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicRepository {
    pub path: String,
    #[serde(default)]
    pub provider_id: Option<u64>,
    pub activity: String,
    pub added_at: u64,
    pub scanned_at: u64,
    pub issue_next: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Cache {
    pub issues: BTreeMap<String, Issue>,
    pub status: BTreeMap<Forge, Status>,
    #[serde(default)]
    pub progress: BTreeMap<Forge, SearchProgress>,
    #[serde(skip)]
    pub repositories: BTreeMap<Forge, BTreeMap<String, PublicRepository>>,
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
    fn parses_tables_and_explicit_currency_variants() {
        for (input, expected) in [
            ("[Bounty CA$250] Fix it", ("CAD", 25000)),
            ("| Bounty | EUR 125.50 |", ("EUR", 12550)),
            ("£75 reward", ("GBP", 7500)),
            ("💎 $50 bounty", ("$", 5000)),
            ("Paid issue: 150 AUD", ("AUD", 15000)),
        ] {
            let r = reward(input, "", &[]).unwrap();
            assert_eq!((r.currency.as_str(), r.cents), expected);
        }
    }

    #[test]
    fn distinguishes_missing_and_ambiguous_amounts() {
        assert_eq!(reward_evidence("Bounty", "", &[]), RewardEvidence::Missing);
        assert_eq!(
            reward_evidence("Bounty: $100–$200", "", &[]),
            RewardEvidence::Ambiguous
        );
        assert_eq!(
            reward_evidence("Reward: €200 USD", "", &[]),
            RewardEvidence::Ambiguous
        );
    }

    #[test]
    fn rejects_signed_amounts_without_confusing_issue_numbers_with_rewards() {
        assert_eq!(
            reward_evidence("Bounty -100 USD", "", &[]),
            RewardEvidence::Ambiguous
        );
        assert_eq!(
            reward("Fix issue 123 with a bounty", "Reward: EUR 50", &[])
                .unwrap()
                .cents,
            5000
        );
    }

    #[test]
    fn keeps_crypto_precision_and_rejects_alternatives() {
        let parsed = reward("Bounty: 0.000000000000000001 ETH", "", &[]).unwrap();
        assert_eq!(parsed.scaled(), 1);
        assert_eq!(parsed.amount_text(), "0.000000000000000001");
        assert_eq!(
            reward("Bounty: 120.50 USDC", "", &[]).unwrap().currency,
            "USDC"
        );
        assert!(reward("Bounty: 0.5 BTC or 8 SOL", "", &[]).is_none());
        assert!(reward("Bounty: USD 1.001", "", &[]).is_none());
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
