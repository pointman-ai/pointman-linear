//! The extension's process: the node starts it (`bin/linear`, from the extension's folder) and talks to it
//! over stdin and stdout. The `board` pane is a Linear team's or project's board, drawn by the apps' own board,
//! with full control of it: a move, a rank, an edit, a new card or a comment goes to Linear, and every open
//! board asks Linear what changed every 30 seconds.
//! `linear.boards` lists the teams and projects it can open, for the apps' Add a project board.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use linear::{
    board, by_id, issue_input, newest, project_filter, sort_between, targets, team_filter, Linear, Scope, Tracker,
    CONTENT_MAX,
};
use pointman_extension::{Extension, Handle, Job, Pane, Refused};
use regex::Regex;
use serde_json::{json, Value};

/// How often the vault's linear.* names are looked at, and how long an opened key is kept.
const LIST_EVERY: Duration = Duration::from_secs(60);
const OPEN_EVERY: Duration = Duration::from_secs(1800);
/// How often a board asks Linear for all of it again, not just what changed: issues that left the team
/// or the project only go then.
const WHOLE_EVERY: Duration = Duration::from_secs(600);
/// How many opened cards keep their detail on a board.
const DETAILS_KEPT: usize = 20;

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

/// One open board: which team or project, Linear's issues as last seen, and the opened cards' detail.
struct Shown {
    scope: Scope,
    issues: HashMap<String, Value>,
    /// opened cards' body, comments and children, by issue id; `opened` has the latest last
    details: HashMap<String, Value>,
    opened: Vec<String>,
    since: Option<String>,
    looked: Instant,
    whole: Instant,
}

impl Shown {
    /// A card's detail, as Linear has it now: kept for the latest DETAILS_KEPT opened.
    fn keep_detail(&mut self, id: &str, detail: Value) {
        self.details.insert(id.to_string(), detail);
        self.opened.retain(|o| o != id);
        self.opened.push(id.to_string());
        while self.opened.len() > DETAILS_KEPT {
            let gone = self.opened.remove(0);
            self.details.remove(&gone);
        }
    }
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
        board(&self.map, &shown.scope, &shown.issues, &shown.details, CONTENT_MAX)
    }

    /// pane.open: a team or a Linear project, from the target (a connected project's): {team: key} or
    /// {project: id}.
    fn opened(&self, p: &Pane) -> anyhow::Result<Value> {
        let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let started = Instant::now();
        let linear = self.linear()?;
        let (team, project) = (text(&p.target()["team"]), text(&p.target()["project"]));
        let filter = match (&team, &project) {
            (Some(team), None) => team_filter(team),
            (None, Some(project)) => project_filter(project),
            _ => bail!(
                "which Linear team or project? Open the board with {{team: \"ALA\"}} or {{project: id}}: \
                        /linear boards lists them"
            ),
        };
        // the team or project and its issues at once: the issues need only the target's filter
        let (scope, issues) = std::thread::scope(|s| {
            let scope = s.spawn(|| match (&team, &project) {
                (Some(team), _) => linear.team(team),
                (_, Some(project)) => linear.project(project),
                _ => unreachable!("the target was checked above"),
            });
            let issues = s.spawn(|| linear.issues(&filter, None));
            (joined(scope), joined(issues))
        });
        let (scope, issues) = (scope?, by_id(issues?));
        self.h.log(format!("board {}: {} issues in {} ms", scope.name, issues.len(), started.elapsed().as_millis()));
        let title = format!("{} · Linear", scope.name);
        let shown = Shown {
            scope,
            since: newest(&issues),
            issues,
            details: HashMap::new(),
            opened: Vec::new(),
            looked: Instant::now(),
            whole: Instant::now(),
        };
        let content = self.content(&shown);
        self.open().insert(p.id().to_string(), shown);
        Ok(json!({"title": title, "board": content}))
    }

    /// Puts what Linear answered on the board (an issue as it is after a change, a card's detail), and
    /// show_board sends the apps the difference.
    fn settle(&self, p: &Pane, issue: Option<Value>, detail: Option<(&str, Value)>) {
        let mut open = self.open();
        let Some(shown) = open.get_mut(p.id()) else { return };
        if let Some(issue) = issue {
            if let Some(id) = issue["id"].as_str() {
                shown.issues.insert(id.to_string(), issue);
            }
        }
        if let Some((id, detail)) = detail {
            shown.keep_detail(id, detail);
        }
        p.show_board(self.content(shown));
    }

    /// A gesture on the board: move, rank, edit, open (the card's detail), comment or create. Linear makes
    /// the change, and the card settles where Linear puts it; what Linear refuses snaps back with its
    /// reason.
    fn input(&self, p: &Pane, event: &Value) -> anyhow::Result<()> {
        let kind = event["kind"].as_str().unwrap_or_default();
        let refuse = |why: String| -> anyhow::Result<()> { Err(Refused::new(why).into()) };
        let (scope, issues) = {
            let open = self.open();
            let Some(shown) = open.get(p.id()) else { return refuse("this board is closed".into()) };
            (shown.scope.clone(), shown.issues.clone())
        };
        let linear = match self.linear() {
            Ok(l) => l,
            Err(e) => return refuse(format!("{e:#}")),
        };
        if kind == "create" {
            let Some(fields) = event["fields"].as_object() else { return refuse("a new card says its fields".into()) };
            // the team it goes in: its `team` (a key), else the board's first
            let team_key = fields.get("team").and_then(Value::as_str);
            let team = match team_key {
                Some(k) => scope.teams.iter().find(|t| t.key == k),
                None => scope.teams.first(),
            };
            let Some(team) = team else {
                return refuse(format!("{} isn't one of this board's teams", team_key.unwrap_or("a team")));
            };
            let mut fields = fields.clone();
            fields.remove("team");
            if !fields.contains_key("title") {
                return refuse("a new card needs a title".into());
            }
            let mut input = match issue_input(&fields, &scope, team, &issues) {
                Ok(input) => input,
                Err(why) => return refuse(why),
            };
            input["teamId"] = json!(team.id);
            if let Some(project) = &scope.project {
                input["projectId"] = json!(project);
            }
            return match linear.create(input) {
                Ok(made) => {
                    self.settle(p, Some(made), None);
                    Ok(())
                }
                Err(e) => refuse(format!("{e:#}")),
            };
        }
        let item = event["item"].as_str().unwrap_or_default().to_string();
        let Some(issue) = issues.get(&item).cloned() else {
            return match kind {
                "move" | "rank" | "edit" | "open" | "comment" => refuse(format!("{item} isn't on this board")),
                other => refuse(format!("{other} isn't something a board does")),
            };
        };
        let issue_id = issue["id"].as_str().unwrap_or_default().to_string();
        match kind {
            // the card's detail, which the app shows as it lands
            "open" => {
                return match linear.detail(&issue_id) {
                    Ok(detail) => {
                        self.settle(p, None, Some((&item, detail)));
                        Ok(())
                    }
                    Err(e) => refuse(format!("{e:#}")),
                }
            }
            "comment" => {
                let body = event["body"].as_str().map(str::trim).unwrap_or_default();
                if body.is_empty() {
                    return refuse("a comment needs words".into());
                }
                return match linear.comment(&issue_id, body).and_then(|_| linear.detail(&issue_id)) {
                    Ok(detail) => {
                        self.settle(p, None, Some((&item, detail)));
                        Ok(())
                    }
                    Err(e) => refuse(format!("{e:#}")),
                };
            }
            _ => {}
        }
        // the issue's own team: a project's issues can be in several teams
        let Some(team) = scope.team_of(&issue) else { return refuse(format!("{item}'s team isn't on this board")) };
        let input = match kind {
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
                // the column: its state's name, which teams in a project share
                let state = issues.get(next_to).map(|i| i["state"]["name"].clone());
                let column: Vec<Value> = issues
                    .values()
                    .filter(|i| i["id"] != item.as_str() && Some(&i["state"]["name"]) == state.as_ref())
                    .cloned()
                    .collect();
                match sort_between(&column, next_to, before) {
                    Some(order) => json!({"sortOrder": order}),
                    None => return refuse(format!("{next_to} isn't on this board")),
                }
            }
            "edit" => match event["fields"].as_object().map(|f| issue_input(f, &scope, team, &issues)) {
                Some(Ok(input)) => input,
                Some(Err(why)) => return refuse(why),
                None => return refuse("an edit says which fields".into()),
            },
            other => return refuse(format!("{other} isn't something a board does")),
        };
        let after = match linear.update(&issue_id, input) {
            Ok(after) => after,
            Err(e) => return refuse(format!("{e:#}")),
        };
        // an opened card's body changed: its detail again
        let opened = self.open().get(p.id()).is_some_and(|s| s.details.contains_key(&item));
        let detail = if opened && event["fields"].get("body").is_some() { linear.detail(&issue_id).ok() } else { None };
        self.settle(p, Some(after), detail.map(|d| (item.as_str(), d)));
        Ok(())
    }

    /// Every open board whose time has come asks Linear what changed (all of it every ten minutes, with
    /// its teams' people, labels and sprints), and the detail of opened cards that changed; show_board
    /// sends the apps only the difference.
    fn refresh(&self) {
        let every = Duration::from_secs(self.h.settings()["refresh_seconds"].as_u64().unwrap_or(30).max(1));
        for p in self.h.panes("board") {
            let due = {
                let open = self.open();
                let Some(shown) = open.get(p.id()) else { continue };
                if shown.looked.elapsed() < every {
                    continue;
                }
                let whole = shown.whole.elapsed() >= WHOLE_EVERY;
                let opened: Vec<String> = shown.opened.clone();
                (shown.scope.clone(), if whole { None } else { shown.since.clone() }, opened)
            };
            let (scope, since, opened) = due;
            let got = self.linear().and_then(|l| {
                let issues = l.issues(&scope.filter, since.as_deref())?;
                let scope = match (since.is_none(), &scope.project, scope.teams.first()) {
                    (false, _, _) => None,
                    (true, Some(project), _) => Some(l.project(project)?),
                    (true, None, Some(team)) => Some(l.team(&team.key)?),
                    (true, None, None) => None,
                };
                let changed: Vec<(String, Value)> = issues
                    .iter()
                    .filter_map(|i| i["id"].as_str())
                    .filter(|id| opened.iter().any(|o| o == id))
                    .filter_map(|id| Some((id.to_string(), l.detail(id).ok()?)))
                    .collect();
                Ok((issues, scope, changed))
            });
            let mut open = self.open();
            let Some(shown) = open.get_mut(p.id()) else { continue };
            shown.looked = Instant::now();
            match got {
                Ok((issues, scope, changed)) => {
                    if since.is_none() {
                        shown.issues = by_id(issues);
                        shown.whole = Instant::now();
                    } else {
                        shown.issues.extend(by_id(issues));
                    }
                    if let Some(scope) = scope {
                        shown.scope = scope;
                    }
                    for (id, detail) in changed {
                        shown.details.insert(id, detail);
                    }
                }
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

/// A call's answer from its thread (one that panicked, as an error).
fn joined<T>(h: std::thread::ScopedJoinHandle<'_, anyhow::Result<T>>) -> anyhow::Result<T> {
    h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("a call to Linear failed unexpectedly")))
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

    // what a project can be connected to (Pointman's Project pane, MB-378; the board's `targets`): the
    // teams and the open Linear projects the key sees, each as the target the board opens by
    let b = boards.clone();
    ext.command("linear.boards", move |_: &Job| {
        let linear = b.linear()?;
        let (teams, projects) = (linear.teams()?, linear.projects()?);
        let listed = |names: Vec<String>, what: &str| match names.len() {
            0 => format!("No Linear {what}s"),
            n => format!("{n} Linear {what}{}: {}", if n == 1 { "" } else { "s" }, names.join(", ")),
        };
        let team_names = teams
            .iter()
            .map(|t| format!("{} ({})", t["name"].as_str().unwrap_or_default(), t["key"].as_str().unwrap_or_default()))
            .collect();
        let project_names = projects.iter().filter_map(|p| p["name"].as_str()).map(String::from).collect();
        let text = format!("{}. {}.", listed(team_names, "team"), listed(project_names, "project"));
        Ok(json!({"targets": targets(&teams, &projects), "text": text}))
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
