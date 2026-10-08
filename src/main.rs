//! The extension's process: the node starts it (`bin/linear`, from the extension's folder) and talks to it
//! over stdin and stdout. The `board` pane is a Linear team's board, drawn by the apps' own board; a move,
//! a rank or an edit goes to Linear, and every open board asks Linear what changed every minute.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Context;
use linear::{board, by_id, edit_input, newest, sort_between, Linear, Team, Tracker, CONTENT_MAX};
use pointman_extension::{Extension, Handle, Job, Pane, Refused};
use regex::Regex;
use serde_json::{json, Value};

/// How often the vault's linear.* names are looked at, and how long an opened key is kept.
const LIST_EVERY: Duration = Duration::from_secs(60);
const OPEN_EVERY: Duration = Duration::from_secs(1800);
/// How often a board asks Linear for all of it again, not just what changed: issues that left the team
/// or the project only go then.
const WHOLE_EVERY: Duration = Duration::from_secs(600);

/// The vault's Linear API key for this machine (`linear.<label>`, the first by label).
#[derive(Default)]
struct Vault {
    listed: Option<Instant>,
    names: Vec<String>,
    opened: Option<(Instant, String, Option<String>)>,
}

impl Vault {
    fn key(&mut self, h: &Handle) -> Option<String> {
        if self.listed.is_none_or(|t| t.elapsed() > LIST_EVERY) {
            self.listed = Some(Instant::now());
            if let Ok(got) = h.request("secret.list", json!({"prefix": "linear."})) {
                let mut names: Vec<String> = got["names"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                    .unwrap_or_default();
                names.sort();
                self.names = names;
            }
        }
        let name = self.names.first()?.clone();
        if let Some((at, n, key)) = &self.opened {
            if *n == name && at.elapsed() < OPEN_EVERY {
                return key.clone();
            }
        }
        let found = Regex::new(r"lin_api_[A-Za-z0-9]{40}").expect("compiles");
        let key = match h.secret(&name) {
            Ok(value) => found.find(&value).map(|m| m.as_str().to_string()),
            Err(e) => {
                h.log(format!("couldn't open {name}: {e}"));
                None
            }
        };
        self.opened = Some((Instant::now(), name, key.clone()));
        key
    }
}

/// One open board: which team and project, and Linear's issues as last seen.
struct Shown {
    team: Team,
    project: Option<String>,
    issues: HashMap<String, Value>,
    since: Option<String>,
    looked: Instant,
    whole: Instant,
}

struct Boards {
    h: Handle,
    map: Tracker,
    vault: Mutex<Vault>,
    open: Mutex<HashMap<String, Shown>>,
}

impl Boards {
    fn open(&self) -> MutexGuard<'_, HashMap<String, Shown>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn linear(&self) -> anyhow::Result<Linear> {
        let settings = self.h.settings();
        let url = settings["api_url"].as_str().filter(|u| !u.is_empty()).unwrap_or("https://api.linear.app/graphql");
        let key = self.vault.lock().unwrap_or_else(|e| e.into_inner()).key(&self.h);
        let key = key.context("no Linear API key in the vault: add one with /linear add")?;
        Ok(Linear::new(url, &key))
    }

    fn content(&self, shown: &Shown) -> Value {
        board(&self.map, &shown.team, shown.project.as_deref(), &shown.issues, CONTENT_MAX)
    }

    /// pane.open: the team from the target, else the settings' (a board opened from the menu).
    fn opened(&self, p: &Pane) -> anyhow::Result<Value> {
        let settings = self.h.settings();
        let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let team_key = text(&p.target()["team"])
            .or_else(|| text(&settings["team"]))
            .context("which Linear team? Open the board with {team: \"ALA\"}, or set the extension's team")?;
        let project = text(&p.target()["project"])
            .or_else(|| text(&settings["project"]).filter(|_| p.target()["team"].is_null()));
        let linear = self.linear()?;
        let team = linear.team(&team_key)?;
        let issues = by_id(linear.issues(&team.key, project.as_deref(), None)?);
        let title = format!("{} · Linear", project.as_deref().unwrap_or(&team.name));
        let shown =
            Shown { team, project, since: newest(&issues), issues, looked: Instant::now(), whole: Instant::now() };
        let content = self.content(&shown);
        self.open().insert(p.id().to_string(), shown);
        Ok(json!({"title": title, "board": content}))
    }

    /// A gesture on the board: move, rank, open or edit. Linear makes the change, and the card settles
    /// where Linear puts it; what Linear refuses snaps back with its reason.
    fn input(&self, p: &Pane, event: &Value) -> anyhow::Result<()> {
        let kind = event["kind"].as_str().unwrap_or_default();
        let item = event["item"].as_str().unwrap_or_default().to_string();
        let refuse = |why: String| -> anyhow::Result<()> { Err(Refused::new(why).into()) };
        let (issue, team) = {
            let open = self.open();
            let Some(shown) = open.get(p.id()) else { return refuse("this board is closed".into()) };
            let Some(issue) = shown.issues.get(&item).cloned() else {
                return match kind {
                    "move" | "rank" | "edit" | "open" => refuse(format!("{item} isn't on this board")),
                    other => refuse(format!("{other} isn't something a board does")),
                };
            };
            (issue, shown.team.clone())
        };
        let input = match kind {
            "open" => return Ok(()), // the app opens the card, and Linear from its url
            "move" => {
                let to = event["to"].as_str().unwrap_or_default();
                match team.states.iter().find(|s| s["name"] == to) {
                    Some(state) => json!({"stateId": state["id"]}),
                    None => return refuse(format!("{to} isn't one of {}'s states", team.key)),
                }
            }
            "rank" => {
                let (next_to, before) = match (event["before"].as_str(), event["after"].as_str()) {
                    (Some(b), _) => (b, true),
                    (None, Some(a)) => (a, false),
                    _ => return refuse("a rank says which card it goes before or after".into()),
                };
                let column: Vec<Value> = {
                    let open = self.open();
                    let shown = open.get(p.id()).context("this board is closed")?;
                    let state = shown.issues.get(next_to).map(|i| i["state"]["id"].clone());
                    shown
                        .issues
                        .values()
                        .filter(|i| i["id"] != item.as_str() && Some(&i["state"]["id"]) == state.as_ref())
                        .cloned()
                        .collect()
                };
                match sort_between(&column, next_to, before) {
                    Some(order) => json!({"sortOrder": order}),
                    None => return refuse(format!("{next_to} isn't on this board")),
                }
            }
            "edit" => match event["fields"].as_object().map(edit_input) {
                Some(Ok(input)) => input,
                Some(Err(why)) => return refuse(why),
                None => return refuse("an edit says which fields".into()),
            },
            other => return refuse(format!("{other} isn't something a board does")),
        };
        let issue_id = issue["id"].as_str().unwrap_or_default();
        let after = match self.linear().and_then(|l| l.update(issue_id, input)) {
            Ok(after) => after,
            Err(e) => return refuse(format!("{e:#}")),
        };
        let mut open = self.open();
        if let Some(shown) = open.get_mut(p.id()) {
            shown.issues.insert(item, after);
            p.show_board(self.content(shown));
        }
        Ok(())
    }

    /// Every open board whose time has come asks Linear what changed (all of it every ten minutes), and
    /// show_board sends the apps only that.
    fn refresh(&self) {
        let every = Duration::from_secs(self.h.settings()["refresh_seconds"].as_u64().unwrap_or(60).max(1));
        for p in self.h.panes("board") {
            let due = {
                let open = self.open();
                let Some(shown) = open.get(p.id()) else { continue };
                if shown.looked.elapsed() < every {
                    continue;
                }
                let whole = shown.whole.elapsed() >= WHOLE_EVERY;
                (shown.team.key.clone(), shown.project.clone(), if whole { None } else { shown.since.clone() })
            };
            let (team, project, since) = due;
            let got = self.linear().and_then(|l| l.issues(&team, project.as_deref(), since.as_deref()));
            let mut open = self.open();
            let Some(shown) = open.get_mut(p.id()) else { continue };
            shown.looked = Instant::now();
            match got {
                Ok(issues) if since.is_none() => {
                    shown.issues = by_id(issues);
                    shown.whole = Instant::now();
                }
                Ok(issues) => shown.issues.extend(by_id(issues)),
                Err(e) => {
                    self.h.log(format!("pane {}: couldn't ask Linear: {e:#}", p.id()));
                    continue;
                }
            }
            shown.since = newest(&shown.issues).or(shown.since.take());
            p.show_board(self.content(shown));
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let boards =
        Arc::new(Boards { h: ext.handle(), map: Tracker::default(), vault: Mutex::default(), open: Mutex::default() });

    // `/linear add` posted (an agent, an older app): the apps open the key's form for it
    ext.command("linear.add", |_: &Job| {
        Ok(json!({"text": "A Linear API key is added in the Pointman app, so it never goes in a post: \
                           type `/linear add` there and paste it."}))
    });

    let b = boards.clone();
    ext.pane("board", move |p: &Pane| {
        let answer = b.opened(p)?;
        let on = b.clone();
        p.on_input(move |p: &Pane, event: &Value| on.input(p, event));
        let gone = b.clone();
        p.on_close(move |p: &Pane, _: &str| {
            gone.open().remove(p.id());
        });
        Ok(answer)
    });
    let b = boards.clone();
    ext.every(Duration::from_secs(1), move |_: &Handle| b.refresh(), "boards");

    ext.run()
}
