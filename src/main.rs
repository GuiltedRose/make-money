mod model;
mod storage;
mod sync;

use anyhow::{Result, bail};
use eframe::egui::{self, Color32, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use model::{Availability, Cache, Forge, Issue};
use reqwest::Url;
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, SystemTime},
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rank {
    Availability,
    Reward,
    Difficulty,
}

struct App {
    root: PathBuf,
    cache: Cache,
    markdown_cache: CommonMarkCache,
    cache_modified: Option<SystemTime>,
    receiver: Option<mpsc::Receiver<String>>,
    message: String,
    error: String,
    text: String,
    forge: Option<Forge>,
    availability: Option<Availability>,
    currency: String,
    minimum: u32,
    difficulty: Option<u8>,
    reversing_only: bool,
    rank: Rank,
    selected: Option<String>,
    page: usize,
}

impl App {
    fn new(root: PathBuf) -> Self {
        let mut app = Self {
            root,
            cache: Cache::default(),
            markdown_cache: CommonMarkCache::default(),
            cache_modified: None,
            receiver: None,
            message: String::new(),
            error: String::new(),
            text: String::new(),
            forge: None,
            availability: Some(Availability::Open),
            currency: String::new(),
            minimum: 0,
            difficulty: None,
            reversing_only: false,
            rank: Rank::Availability,
            selected: None,
            page: 0,
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        let modified = storage::cache_modified(&self.root);
        match storage::load(&self.root) {
            Ok(cache) => {
                self.cache = cache;
                self.cache_modified = modified;
                self.error.clear();
            }
            Err(e) => self.error = format!("Cache could not be loaded: {e:#}"),
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.message = "Refreshing…".into();
        let root = self.root.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = sync::run(&root, |status| {
                let _ = tx.send(status);
                ctx.request_repaint();
            });
            let _ = tx.send(match result {
                Ok(()) => "Refresh complete".into(),
                Err(e) => format!("{e:#}"),
            });
            drop(tx);
            ctx.request_repaint();
        });
    }
}

const STATES: [Availability; 5] = [
    Availability::Open,
    Availability::Assigned,
    Availability::Stale,
    Availability::Closed,
    Availability::Unreachable,
];

const PAGE_SIZE: usize = 50;

fn reward_text(issue: &Issue) -> String {
    issue
        .reward
        .as_ref()
        .map(|r| format!("{} {}", r.currency, r.amount_text()))
        .unwrap_or_else(|| "Reward no longer stated".into())
}

fn difficulty_text(issue: &Issue) -> &'static str {
    match issue.difficulty().map(|(level, _)| level) {
        Some(1) => "Easy",
        Some(2) => "Medium",
        Some(3) => "Hard",
        _ => "Unknown",
    }
}

fn show_markdown(ui: &mut egui::Ui, cache: &mut CommonMarkCache, issue: &Issue) {
    let previous_commands = ui.ctx().output(|output| output.commands.len());
    CommonMarkViewer::new().show(ui, cache, &issue.body);
    ui.ctx().output_mut(|output| {
        let commands = output.commands.split_off(previous_commands);
        let base = Url::parse(&issue.url).ok();
        for command in commands {
            match command {
                egui::OutputCommand::OpenUrl(mut open) => {
                    if let Some(url) = base
                        .as_ref()
                        .and_then(|base| base.join(&open.url).ok())
                        .filter(|url| {
                            matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                        })
                    {
                        open.url = url.into();
                        output.commands.push(egui::OutputCommand::OpenUrl(open));
                    }
                }
                other => output.commands.push(other),
            }
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let mut changed = false;
        if let Some(rx) = &self.receiver {
            loop {
                match rx.try_recv() {
                    Ok(message) => {
                        self.message = message;
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.receiver = None;
                        self.reload();
                        break;
                    }
                }
            }
        }
        if changed || storage::cache_modified(&self.root) != self.cache_modified {
            self.reload();
        }
        let now = model::now();
        ctx.request_repaint_after(Duration::from_secs(2));
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                ui.heading("Forge Bounties");
                if ui
                    .add_enabled(
                        self.receiver.is_none(),
                        egui::Button::new("Refresh from forges"),
                    )
                    .clicked()
                {
                    self.refresh(ctx);
                }
                if ui.button("Reload cache").clicked() {
                    self.reload();
                }
            });
            if !self.message.is_empty() {
                ui.label(&self.message);
            }
            if !self.error.is_empty() {
                ui.colored_label(Color32::LIGHT_RED, &self.error);
            }
            ui.add_space(7.0);
        });
        egui::SidePanel::left("filters")
            .resizable(true)
            .default_width(215.0)
            .show(ctx, |ui| {
                ui.heading("Filters");
                egui::ComboBox::from_label("Forge")
                    .selected_text(self.forge.map_or("All".into(), |f| format!("{f:?}")))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.forge, None, "All");
                        for forge in Forge::ALL {
                            ui.selectable_value(&mut self.forge, Some(forge), format!("{forge:?}"));
                        }
                    });
                egui::ComboBox::from_label("Availability")
                    .selected_text(self.availability.map_or("All".into(), |a| format!("{a:?}")))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.availability, None, "All");
                        for state in STATES {
                            ui.selectable_value(
                                &mut self.availability,
                                Some(state),
                                format!("{state:?}"),
                            );
                        }
                    });
                egui::ComboBox::from_label("Difficulty")
                    .selected_text(match self.difficulty {
                        Some(1) => "Easy",
                        Some(2) => "Medium",
                        Some(3) => "Hard",
                        Some(_) => "Unknown",
                        None => "All",
                    })
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            (None, "All"),
                            (Some(1), "Easy"),
                            (Some(2), "Medium"),
                            (Some(3), "Hard"),
                            (Some(4), "Unknown"),
                        ] {
                            ui.selectable_value(&mut self.difficulty, value, label);
                        }
                    });
                if ui.checkbox(&mut self.reversing_only, "Reverse engineering only")
                    .on_hover_text("Requires a stated reward and explicit wording in the issue title, description, or labels.")
                    .changed()
                {
                    self.page = 0;
                }
                egui::ComboBox::from_label("Currency")
                    .selected_text(if self.currency.is_empty() {
                        "All"
                    } else {
                        &self.currency
                    })
                    .show_ui(ui, |ui| {
                        for currency in ["", "USD", "EUR", "GBP", "CAD", "AUD", "NZD", "CHF", "$", "USDC", "USDT", "SOL", "ETH", "BTC"] {
                            ui.selectable_value(
                                &mut self.currency,
                                currency.into(),
                                if currency.is_empty() { "All" } else { currency },
                            );
                        }
                    });
                ui.add_enabled(
                    !self.currency.is_empty(),
                    egui::DragValue::new(&mut self.minimum)
                        .prefix("Min reward ")
                        .speed(1.0),
                );
                ui.separator();
                ui.heading("Forge status");
                for forge in Forge::ALL {
                    ui.strong(format!("{forge:?}"));
                    if let Some(status) = self.cache.status.get(&forge) {
                        ui.label(&status.message);
                        ui.small(format!(
                            "Attempted {} min ago",
                            now.saturating_sub(status.attempted) / 60
                        ));
                        if status.retry_at > now {
                            ui.small(format!(
                                "Retry in {} min",
                                (status.retry_at - now).div_ceil(60)
                            ));
                        }
                    } else {
                        ui.small("No refresh recorded");
                    }
                }
            });
        if let Some(issue) = self
            .selected
            .as_ref()
            .and_then(|id| self.cache.issues.get(id))
        {
            egui::SidePanel::right("detail")
                .default_width(325.0)
                .resizable(true)
                .show(ctx, |ui| {
                    if ui.button("Close details").clicked() {
                        self.selected = None;
                    }
                    egui::ScrollArea::both().id_salt(&issue.url).show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        ui.heading(&issue.title);
                        ui.label(format!(
                            "{:?} · Checked {} min ago",
                            issue.availability(now),
                            now.saturating_sub(issue.checked) / 60
                        ));
                        ui.strong(reward_text(issue));
                        if let Some(reward) = &issue.reward {
                            ui.label(&reward.evidence);
                        }
                        if let Some((_, label)) = issue.difficulty() {
                            ui.label(format!("Difficulty estimate from label “{label}”"));
                        }
                        if sync::issue_url(issue.forge, &issue.url).is_ok()
                            && ui.button("Open issue in browser").clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(&issue.url));
                        }
                        ui.separator();
                        ui.label(issue.labels.join(" · "));
                        show_markdown(ui, &mut self.markdown_cache, issue);
                    });
                });
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.text)
                            .hint_text("Search cached issues"),
                    )
                    .changed()
                {
                    self.page = 0;
                }
                egui::ComboBox::from_label("Rank by")
                    .selected_text(format!("{:?}", self.rank))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.rank, Rank::Availability, "Availability");
                        ui.selectable_value(&mut self.rank, Rank::Reward, "Reward");
                        ui.selectable_value(&mut self.rank, Rank::Difficulty, "Difficulty");
                    });
            });
            let query = self.text.trim().to_lowercase();
            let mut rows: Vec<_> = self
                .cache
                .issues
                .values()
                .filter(|issue| {
                    self.forge.is_none_or(|forge| issue.forge == forge)
                        && self
                            .availability
                            .is_none_or(|state| issue.availability(now) == state)
                        && self
                            .difficulty
                            .is_none_or(|d| issue.difficulty().map_or(4, |(v, _)| v) == d)
                        && (!self.reversing_only
                            || (issue.reward.is_some() && issue.explicitly_mentions_reversing()))
                        && (self.availability != Some(Availability::Open) || issue.reward.is_some())
                        && (self.currency.is_empty()
                            || issue.reward.as_ref().is_some_and(|r| {
                                r.currency == self.currency
                                    && r.scaled() >= u128::from(self.minimum) * model::Reward::SCALE
                            }))
                        && (query.is_empty()
                            || issue.title.to_lowercase().contains(&query)
                            || issue.url.to_lowercase().contains(&query)
                            || issue.body.to_lowercase().contains(&query)
                            || issue
                                .labels
                                .iter()
                                .any(|label| label.to_lowercase().contains(&query)))
                })
                .collect();
            rows.sort_unstable_by(|a, b| {
                let availability = a.availability(now).cmp(&b.availability(now));
                let difficulty = a
                    .difficulty()
                    .map_or(4, |(v, _)| v)
                    .cmp(&b.difficulty().map_or(4, |(v, _)| v));
                let reward = match (&a.reward, &b.reward) {
                    (Some(a), Some(b)) => a
                        .currency
                        .cmp(&b.currency)
                        .then_with(|| b.scaled().cmp(&a.scaled())),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                };
                match self.rank {
                    Rank::Availability => availability.then(reward).then(difficulty),
                    Rank::Reward => reward.then(availability).then(difficulty),
                    Rank::Difficulty => difficulty.then(availability).then(reward),
                }
                .then_with(|| a.url.cmp(&b.url))
            });
            let last = rows.len().div_ceil(PAGE_SIZE).saturating_sub(1);
            self.page = self.page.min(last);
            ui.horizontal(|ui| {
                ui.label(format!("{} matches", rows.len()));
                if last > 0 {
                    if ui
                        .add_enabled(self.page > 0, egui::Button::new("Previous"))
                        .clicked()
                    {
                        self.page -= 1;
                    }
                    ui.label(format!("{} / {}", self.page + 1, last + 1));
                    if ui
                        .add_enabled(self.page < last, egui::Button::new("Next"))
                        .clicked()
                    {
                        self.page += 1;
                    }
                }
            });
            if self.cache.issues.is_empty() {
                ui.label("No opportunities cached. Refresh when online.");
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for issue in rows.into_iter().skip(self.page * PAGE_SIZE).take(PAGE_SIZE) {
                        let key = issue.key();
                        if ui
                            .selectable_label(
                                self.selected.as_deref() == Some(key.as_str()),
                                RichText::new(&issue.title).strong(),
                            )
                            .clicked()
                        {
                            self.selected = Some(key);
                        }
                        ui.small(format!(
                            "{:?} · {} · {} · {:?}",
                            issue.forge,
                            reward_text(issue),
                            difficulty_text(issue),
                            issue.availability(now)
                        ));
                        ui.separator();
                    }
                });
        });
    }
}

fn main() -> Result<()> {
    let root = storage::directory()?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => eframe::run_native(
            "Forge Bounties",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([1150.0, 740.0])
                    .with_min_inner_size([760.0, 480.0]),
                ..Default::default()
            },
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(egui::Visuals::dark());
                Ok(Box::new(App::new(root)))
            }),
        )
        .map_err(|e| anyhow::anyhow!("{e}")),
        [arg] if arg == "--sync" => sync::run(&root, |message| println!("{message}")),
        [arg] if arg == "--status" => {
            let cache = storage::load(&root)?;
            println!(
                "{} cached opportunities in {}",
                cache.issues.len(),
                root.display()
            );
            println!("{}", serde_json::to_string_pretty(&cache.status)?);
            Ok(())
        }
        [arg] if arg == "--export-legacy" => {
            let mut cache = storage::load(&root)?;
            cache.issues = cache
                .issues
                .into_values()
                .map(|issue| (issue.url.clone(), issue))
                .collect();
            serde_json::to_writer(std::io::stdout().lock(), &cache)?;
            Ok(())
        }
        [arg] if arg == "--help" => {
            println!("forge-bounties [--sync | --status | --export-legacy | --help]");
            Ok(())
        }
        _ => bail!("usage: forge-bounties [--sync | --status | --export-legacy | --help]"),
    }
}
