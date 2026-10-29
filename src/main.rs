//! The extension's process: the node starts it (`bin/linear`, from the extension's folder) and talks to it
//! over stdin and stdout. The `board` pane is a Linear team's or project's board, drawn by the apps' own board,
//! with full control of it: a move, a rank, an edit, a new card or a comment goes to Linear, and every open
//! board asks Linear what changed every 30 seconds.
//! `linear.boards` lists the teams and projects it can open, for the apps' Add a project board: every key's in the
//! vault, one per Linear workspace, and a board is asked with the key that sees it.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use linear::{
    board, by_id, handle, issue_input, issue_lines, issue_out, newest, project_filter, search_filter, sort_between,
    targets, team_filter, Linear, Scope, Tracker, CONTENT_MAX,
};
use pointman_extension::protocol::PaneOpenResponse;
use pointman_extension::v1::PaneInput;
use pointman_extension::{Extension, Handle, Job, Pane, Refused};
use regex::Regex;
use serde_json::{json, Map, Value};

/// How often the vault's linear.* names are looked at, and how long an opened key is kept. A list older
/// than LIST_EVERY is still used while it's asked for again behind (each ask is a round trip to core, 0.2 s
/// from the Mac, that a board's open waited on: Pointman thread 6216); one older than OPEN_EVERY, or one
/// with no key in it, is asked for first.
const LIST_EVERY: Duration = Duration::from_secs(60);
const OPEN_EVERY: Duration = Duration::from_secs(1800);
/// How often a board asks Linear for all of it again, not just what changed: issues that left the team
/// or the project only go then.
const WHOLE_EVERY: Duration = Duration::from_secs(600);
/// How many opened cards keep their detail on a board.
const DETAILS_KEPT: usize = 20;

/// The vault's Linear API keys for this machine (`linear.<label>`, by label): one per Linear workspace, and
/// a board goes with the key that sees its team or project.
#[derive(Default)]
struct Vault {
    listed: Option<Instant>,
    names: Vec<String>,
    /// The list being asked for behind, None in it if the node couldn't say.
    asking: Option<Receiver<Option<Vec<String>>>>,
    /// Each key opened, by its name in the vault, and when.
    opened: HashMap<String, (Instant, Option<String>)>,
}

/// What's done about the vault's list, from its age, whether it's being asked for already and whether it
/// has a key in it.
#[derive(Debug, PartialEq)]
enum Relist {
    Now,
    Behind,
    Kept,
}

fn relist(age: Option<Duration>, asking: bool, empty: bool) -> Relist {
    match age {
        None => Relist::Now,
        Some(age) if age > OPEN_EVERY || (empty && age > LIST_EVERY) => Relist::Now,
        Some(age) if age > LIST_EVERY && !asking => Relist::Behind,
        Some(_) => Relist::Kept,
    }
}

fn list(h: &Handle) -> Option<Vec<String>> {
    let mut names = h.secrets("linear.").ok()?;
    names.sort();
    Some(names)
}

impl Vault {
    /// The vault's linear.* names, by label, listed again as `relist` says.
    fn names(&mut self, h: &Handle) -> Vec<String> {
        if let Some(asking) = &self.asking {
            match asking.try_recv() {
                Ok(got) => {
                    self.names = got.unwrap_or_else(|| self.names.clone());
                    self.asking = None;
                }
                Err(TryRecvError::Disconnected) => self.asking = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        match relist(self.listed.map(|t| t.elapsed()), self.asking.is_some(), self.names.is_empty()) {
            Relist::Now => {
                self.listed = Some(Instant::now());
                self.asking = None;
                if let Some(names) = list(h) {
                    self.names = names;
                }
            }
            Relist::Behind => {
                self.listed = Some(Instant::now());
                let (tx, rx) = mpsc::channel();
                self.asking = Some(rx);
                let h = h.clone();
                std::thread::spawn(move || tx.send(list(&h)));
            }
            Relist::Kept => {}
        }
        self.names.clone()
    }

    /// One of them's key, opened again after OPEN_EVERY.
    fn key(&mut self, h: &Handle, name: &str) -> Option<String> {
        if let Some((at, key)) = self.opened.get(name) {
            if at.elapsed() < OPEN_EVERY {
                return key.clone();
            }
        }
        let found = Regex::new(r"lin_api_[A-Za-z0-9]{40}").expect("compiles");
        let key = match h.secret(name) {
            Ok(value) => found.find(&value).map(|m| m.as_str().to_string()),
            Err(e) => {
                h.log(format!("couldn't open {name}: {e}"));
                None
            }
        };
        self.opened.insert(name.to_string(), (Instant::now(), key.clone()));
        key
    }
}

/// One open board: which team or project, Linear's issues as last seen, and the opened cards' detail.
struct Shown {
    scope: Scope,
    /// The vault's key it's asked with (linear.<label>).
    account: String,
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
    /// Which of the vault's keys sees a target ("team:ALA", "project:<id>"), from linear.boards and the
    /// boards opened, so a board's open asks that one first.
    seen: Mutex<HashMap<String, String>>,
}

impl Boards {
    fn open(&self) -> MutexGuard<'_, HashMap<String, Shown>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seen(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The vault's Linear keys, by name (linear.<label>): none is an error that says how to add one.
    fn accounts(&self) -> anyhow::Result<Vec<String>> {
        let names = self.vault.lock().unwrap_or_else(|e| e.into_inner()).names(&self.h);
        if names.is_empty() {
            bail!("no Linear API key in the vault: add one with /linear add");
        }
        Ok(names)
    }

    /// Linear, asked with one of the vault's keys.
    fn linear(&self, account: &str) -> anyhow::Result<Linear> {
        let settings = self.h.settings();
        let url = settings["api_url"].as_str().filter(|u| !u.is_empty()).unwrap_or("https://api.linear.app/graphql");
        let key = self.vault.lock().unwrap_or_else(|e| e.into_inner()).key(&self.h, account);
        let key = key
            .with_context(|| format!("{account} isn't a Linear API key (lin_api_…): add it again with /linear add"))?;
        Ok(Linear::new(url, &key))
    }

    /// The keys a command tries: the one it names (`work` or `linear.work`), or every key in the vault.
    fn tried(&self, job: &Job) -> anyhow::Result<Vec<String>> {
        let all = self.accounts()?;
        let Some(named) = text(job, "account") else { return Ok(all) };
        let name = if named.starts_with("linear.") { named.to_string() } else { format!("linear.{named}") };
        match all.contains(&name) {
            true => Ok(vec![name]),
            false => bail!("no Linear key {name} in the vault: {}", all.join(", ")),
        }
    }

    /// Asks with each key in turn until one answers; if none does, says why each didn't.
    fn first<T>(&self, job: &Job, ask: impl Fn(&Linear) -> anyhow::Result<T>) -> anyhow::Result<(Linear, T)> {
        let mut refused = Vec::new();
        for account in self.tried(job)? {
            match self.linear(&account) {
                Ok(linear) => match ask(&linear) {
                    Ok(done) => return Ok((linear, done)),
                    Err(e) => refused.push((account, e)),
                },
                Err(e) => refused.push((account, e)),
            }
        }
        match refused.len() {
            1 => Err(refused.pop().map(|(_, e)| e).expect("one key was tried")),
            _ => Err(refused_by(refused, "No Linear key in the vault could do that")),
        }
    }

    /// A team's scope (states, people, labels, sprints) by its key, with the key that sees it.
    fn team_scope(&self, job: &Job, key: &str) -> anyhow::Result<(Linear, Scope)> {
        self.first(job, |linear| linear.team(&key.to_uppercase()))
    }

    /// An issue by its ref (ALA-12), with the key that sees it.
    fn issue_ref(&self, job: &Job, name: &str) -> anyhow::Result<(Linear, Value)> {
        self.first(job, |linear| linear.find(name))
    }

    fn content(&self, shown: &Shown) -> Value {
        board(&self.map, &shown.scope, &shown.issues, &shown.details, CONTENT_MAX)
    }

    /// pane.open: a team or a Linear project, from the target (a connected project's): {team: key} or
    /// {project: id}.
    fn opened(&self, p: &Pane) -> anyhow::Result<PaneOpenResponse> {
        let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let started = Instant::now();
        let (team, project) = (text(&p.target()["team"]), text(&p.target()["project"]));
        let (filter, seen_as) = match (&team, &project) {
            (Some(team), None) => (team_filter(team), format!("team:{team}")),
            (None, Some(project)) => (project_filter(project), format!("project:{project}")),
            _ => bail!(
                "which Linear team or project? Open the board with {{team: \"ALA\"}} or {{project: id}}: \
                        /linear boards lists them"
            ),
        };
        // the key that saw it before first, then the rest by label: the first that sees it opens it
        let mut accounts = self.accounts()?;
        if let Some(known) = self.seen().get(&seen_as).cloned() {
            if let Some(i) = accounts.iter().position(|a| *a == known) {
                let known = accounts.remove(i);
                accounts.insert(0, known);
            }
        }
        let (mut got, mut refused) = (None, Vec::<(String, anyhow::Error)>::new());
        for account in &accounts {
            let tried = self.linear(account).and_then(|linear| {
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
                Ok((scope?, by_id(issues?)))
            });
            match tried {
                Ok(found) => {
                    got = Some((account.clone(), found));
                    break;
                }
                Err(e) => refused.push((account.clone(), e)),
            }
        }
        let Some((account, (scope, issues))) = got else {
            return Err(refused_by(refused, "none of the vault's Linear keys opens it"));
        };
        self.seen().insert(seen_as, account.clone());
        self.h.log(format!("board {}: {} issues in {} ms", scope.name, issues.len(), started.elapsed().as_millis()));
        let title = format!("{} · Linear", scope.name);
        let shown = Shown {
            scope,
            account,
            since: newest(&issues),
            issues,
            details: HashMap::new(),
            opened: Vec::new(),
            looked: Instant::now(),
            whole: Instant::now(),
        };
        let content = self.content(&shown);
        self.open().insert(p.id().to_string(), shown);
        Ok(PaneOpenResponse::board(&content).titled(&title))
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
        p.show_board(&self.content(shown));
    }

    /// A gesture on the board: move, rank, edit, open (the card's detail), comment or create. Linear makes
    /// the change, and the card settles where Linear puts it; what Linear refuses snaps back with its
    /// reason.
    fn input(&self, p: &Pane, event: &Value) -> anyhow::Result<()> {
        let kind = event["kind"].as_str().unwrap_or_default();
        let refuse = |why: String| -> anyhow::Result<()> { Err(Refused::new(why).into()) };
        let (scope, issues, account) = {
            let open = self.open();
            let Some(shown) = open.get(p.id()) else { return refuse("this board is closed".into()) };
            (shown.scope.clone(), shown.issues.clone(), shown.account.clone())
        };
        let linear = match self.linear(&account) {
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
                (shown.scope.clone(), if whole { None } else { shown.since.clone() }, opened, shown.account.clone())
            };
            let (scope, since, opened, account) = due;
            let got = self.linear(&account).and_then(|l| {
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
            p.show_board(&self.content(shown));
        }
    }
}

/// Why nothing opened: one key's own error as it is, or each key's after `what`.
/// A job's text parameter, trimmed, if it has one that isn't empty.
fn text<'a>(job: &'a Job, name: &str) -> Option<&'a str> {
    job.params[name].as_str().map(str::trim).filter(|t| !t.is_empty())
}

/// A parameter a command can't do without.
fn need<'a>(job: &'a Job, name: &str) -> anyhow::Result<&'a str> {
    text(job, name).ok_or_else(|| anyhow!("{} needs `{name}`", job.kind))
}

/// The fields of an issue a job sets, as the board's edit says them (title, body, state, owner, priority,
/// target, estimate, labels, sprint, parent). One given as null clears it.
fn fields_of(job: &Job) -> Map<String, Value> {
    let set = ["title", "body", "state", "owner", "priority", "target", "estimate", "labels", "sprint", "parent"];
    set.iter().filter_map(|f| job.params.get(*f).map(|v| (f.to_string(), v.clone()))).collect()
}

/// The issue a parent field names, as the board's input looks parents up.
fn parents(linear: &Linear, fields: &Map<String, Value>) -> anyhow::Result<HashMap<String, Value>> {
    let mut issues = HashMap::new();
    if let Some(parent) = fields.get("parent").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()) {
        let found = linear.find(parent)?;
        issues.insert(found["id"].as_str().unwrap_or(parent).to_string(), found);
    }
    Ok(issues)
}

/// An open Linear project by its id or name.
fn project_id(linear: &Linear, named: &str) -> anyhow::Result<Value> {
    let projects = linear.projects()?;
    let found =
        projects.iter().find(|p| p["id"] == named || p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(named)));
    found.map(|p| p["id"].clone()).ok_or_else(|| anyhow!("no open Linear project {named}"))
}

fn refused_by(mut refused: Vec<(String, anyhow::Error)>, what: &str) -> anyhow::Error {
    if refused.len() == 1 {
        return refused.remove(0).1;
    }
    let each: Vec<String> = refused.iter().map(|(account, e)| format!("{account}: {e:#}")).collect();
    anyhow::anyhow!("{what} ({})", each.join("; "))
}

/// A board's input as one JSON object, its kind beside its fields, as `input` reads it.
fn gesture(event: &PaneInput) -> Value {
    let mut out = match event.json() {
        Value::Object(fields) => Value::Object(fields),
        _ => json!({}),
    };
    out["kind"] = json!(event.kind);
    out
}

/// A call's answer from its thread (one that panicked, as an error).
fn joined<T>(h: std::thread::ScopedJoinHandle<'_, anyhow::Result<T>>) -> anyhow::Result<T> {
    h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("a call to Linear failed unexpectedly")))
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let boards = Arc::new(Boards {
        h: ext.handle(),
        map: Tracker::default(),
        vault: Mutex::default(),
        open: Mutex::default(),
        seen: Mutex::default(),
    });

    // `/linear add` posted (an agent, an older app): the apps open the key's form for it
    ext.command("linear.add", |_: &Job| {
        Ok(json!({"text": "A Linear API key is added in the Pointman app, so it never goes in a post: \
                           type `/linear add` there and paste it."}))
    });

    // what a project can be connected to (Pointman's Project pane, MB-378; the board's `targets`): the
    // teams and the open Linear projects the key sees, each as the target the board opens by
    // teams and projects the key sees, each as the target the board opens by: every key's, one per Linear
    // workspace (a team two keys see is listed once, as the first's)
    let b = boards.clone();
    ext.command("linear.boards", move |_: &Job| {
        let accounts = b.accounts()?;
        let listed = |names: Vec<String>, what: &str| match names.len() {
            0 => format!("No Linear {what}s"),
            n => format!("{n} Linear {what}{}: {}", if n == 1 { "" } else { "s" }, names.join(", ")),
        };
        let (mut all, mut lines, mut refused) =
            (Vec::<Value>::new(), Vec::new(), Vec::<(String, anyhow::Error)>::new());
        for account in &accounts {
            let got = b.linear(account).and_then(|linear| Ok((linear.teams()?, linear.projects()?)));
            let (teams, projects) = match got {
                Ok(got) => got,
                Err(e) => {
                    refused.push((account.clone(), e));
                    continue;
                }
            };
            let team_names = teams
                .iter()
                .map(|t| {
                    format!("{} ({})", t["name"].as_str().unwrap_or_default(), t["key"].as_str().unwrap_or_default())
                })
                .collect();
            let project_names = projects.iter().filter_map(|p| p["name"].as_str()).map(String::from).collect();
            let text = format!("{}. {}.", listed(team_names, "team"), listed(project_names, "project"));
            lines.push(if accounts.len() > 1 { format!("{account}: {text}") } else { text });
            for t in targets(&teams, &projects) {
                let seen_as = match (t["target"]["team"].as_str(), t["target"]["project"].as_str()) {
                    (Some(team), _) => format!("team:{team}"),
                    (_, Some(project)) => format!("project:{project}"),
                    _ => continue,
                };
                if all.iter().any(|a| a["target"] == t["target"]) {
                    continue;
                }
                b.seen().entry(seen_as).or_insert_with(|| account.clone());
                all.push(t);
            }
        }
        if lines.is_empty() {
            return Err(refused_by(refused, "Linear refused every key in the vault"));
        }
        lines.extend(refused.iter().map(|(account, e)| format!("Couldn't ask with {account}: {e:#}")));
        Ok(json!({"targets": all, "text": lines.join("\n")}))
    });

    // what an agent does with Linear itself: look around, find, read, file, change and comment. Each asks with
    // the vault's keys in turn (or the one `account` names), and an issue or team is found with whichever sees it.
    let b = boards.clone();
    ext.command("linear.team", move |job: &Job| {
        let (_, scope) = b.team_scope(job, need(job, "team")?)?;
        let team = &scope.teams[0];
        let states: Vec<Value> = team.states.iter().map(|s| json!({"name": s["name"], "type": s["type"]})).collect();
        let people: Vec<Value> =
            scope.people().iter().map(|p| json!({"handle": handle(p), "name": p["name"]})).collect();
        let labels: Vec<&Value> = scope.labels_for(team).map(|l| &l["name"]).collect();
        let sprints: Vec<Value> = team
            .cycles
            .iter()
            .map(|c| {
                let name = c["name"].as_str().map(String::from).unwrap_or_else(|| format!("Cycle {}", c["number"]));
                json!({"id": c["id"], "name": name, "start": c["startsAt"], "end": c["endsAt"]})
            })
            .collect();
        let names = |v: &[Value], k: &str| v.iter().filter_map(|x| x[k].as_str()).collect::<Vec<_>>().join(", ");
        let text = format!(
            "{} ({}). States: {}. People: {}.",
            team.name,
            team.key,
            names(&states, "name"),
            people.iter().filter_map(|p| p["handle"].as_str()).collect::<Vec<_>>().join(", ")
        );
        Ok(json!({"team": team.key, "name": team.name, "states": states, "people": people, "labels": labels,
                  "sprints": sprints, "text": text}))
    });

    let b = boards.clone();
    ext.command("linear.projects", move |job: &Job| {
        let (mut all, mut refused) = (Vec::<Value>::new(), Vec::new());
        for account in b.tried(job)? {
            match b.linear(&account).and_then(|l| l.projects()) {
                Ok(projects) => {
                    for p in projects {
                        if !all.iter().any(|a| a["id"] == p["id"]) {
                            all.push(p);
                        }
                    }
                }
                Err(e) => refused.push((account, e)),
            }
        }
        if all.is_empty() && !refused.is_empty() {
            return Err(refused_by(refused, "Linear refused every key in the vault"));
        }
        let names: Vec<&str> = all.iter().filter_map(|p| p["name"].as_str()).collect();
        Ok(json!({"projects": all, "text": format!("{} open Linear projects: {}", all.len(), names.join(", "))}))
    });

    let b = boards.clone();
    ext.command("linear.search", move |job: &Job| {
        let filter = search_filter(
            text(job, "query"),
            text(job, "team"),
            text(job, "state"),
            text(job, "owner"),
            text(job, "project"),
            job.params["all"] == true,
        );
        let limit = job.params["limit"].as_u64().unwrap_or(25).clamp(1, 100) as usize;
        let (mut found, mut refused) = (Vec::<Value>::new(), Vec::new());
        for account in b.tried(job)? {
            match b.linear(&account).and_then(|l| l.search(&filter, limit)) {
                Ok(issues) => {
                    for i in issues {
                        if !found.iter().any(|f| f["id"] == i["id"]) {
                            found.push(i);
                        }
                    }
                }
                Err(e) => refused.push((account, e)),
            }
        }
        if found.is_empty() && !refused.is_empty() {
            return Err(refused_by(refused, "Linear refused every key in the vault"));
        }
        found.sort_by(|a, b| b["updatedAt"].as_str().cmp(&a["updatedAt"].as_str()));
        found.truncate(limit);
        let issues: Vec<Value> = found.iter().map(|i| issue_out(&b.map, i)).collect();
        Ok(json!({"issues": issues, "text": issue_lines(&issues)}))
    });

    let b = boards.clone();
    ext.command("linear.issue", move |job: &Job| {
        let (_, issue) = b.issue_ref(job, need(job, "issue")?)?;
        let out = issue_out(&b.map, &issue);
        Ok(json!({"issue": out, "text": format!("{} {} [{}]: {}", out["ref"].as_str().unwrap_or_default(),
            out["title"].as_str().unwrap_or_default(), out["state"].as_str().unwrap_or("?"),
            out["url"].as_str().unwrap_or_default())}))
    });

    let b = boards.clone();
    ext.command("linear.create", move |job: &Job| {
        let fields = fields_of(job);
        if fields.get("title").and_then(Value::as_str).is_none_or(|t| t.trim().is_empty()) {
            bail!("linear.create needs a `title`");
        }
        let (linear, scope) = b.team_scope(job, need(job, "team")?)?;
        let team = &scope.teams[0];
        let mut input = issue_input(&fields, &scope, team, &parents(&linear, &fields)?).map_err(|e| anyhow!(e))?;
        input["teamId"] = json!(team.id);
        if let Some(project) = text(job, "project") {
            input["projectId"] = project_id(&linear, project)?;
        }
        let mut out = issue_out(&b.map, &linear.create(input)?);
        out["team"] = json!(team.key);
        let said = format!(
            "Filed {} {}: {}",
            out["ref"].as_str().unwrap_or_default(),
            out["title"].as_str().unwrap_or_default(),
            out["url"].as_str().unwrap_or_default()
        );
        Ok(json!({"issue": out, "text": said}))
    });

    let b = boards.clone();
    ext.command("linear.update", move |job: &Job| {
        let (linear, issue) = b.issue_ref(job, need(job, "issue")?)?;
        let key = issue["team"]["key"].as_str().context("Linear didn't say which team the issue is in")?;
        let scope = linear.team(key)?;
        let fields = fields_of(job);
        let input =
            issue_input(&fields, &scope, &scope.teams[0], &parents(&linear, &fields)?).map_err(|e| anyhow!(e))?;
        let mut out = issue_out(&b.map, &linear.update(issue["id"].as_str().unwrap_or_default(), input)?);
        out["team"] = json!(key);
        let said = format!(
            "Changed {}: now {} ({})",
            out["ref"].as_str().unwrap_or_default(),
            out["state"].as_str().unwrap_or("?"),
            out["url"].as_str().unwrap_or_default()
        );
        Ok(json!({"issue": out, "text": said}))
    });

    let b = boards.clone();
    ext.command("linear.comment", move |job: &Job| {
        let (linear, issue) = b.issue_ref(job, need(job, "issue")?)?;
        let body = need(job, "body")?;
        linear.comment(issue["id"].as_str().unwrap_or_default(), body)?;
        Ok(json!({"text": format!("Commented on {}", issue["identifier"].as_str().unwrap_or_default())}))
    });

    let b = boards.clone();
    ext.pane("board", move |p: &Pane| {
        let answer = b.opened(p)?;
        let on = b.clone();
        p.on_input(move |p: &Pane, event: &PaneInput| on.input(p, &gesture(event)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vaults_list_is_asked_for_first_then_behind_and_first_again_when_too_old_or_keyless() {
        let secs = |s: u64| Some(Duration::from_secs(s));
        assert_eq!(relist(None, false, true), Relist::Now);
        assert_eq!(relist(secs(10), false, false), Relist::Kept);
        assert_eq!(relist(secs(61), false, false), Relist::Behind);
        assert_eq!(relist(secs(61), true, false), Relist::Kept, "asked for once");
        assert_eq!(relist(secs(61), false, true), Relist::Now, "a key added since is found at once");
        assert_eq!(relist(secs(10), false, true), Relist::Kept);
        assert_eq!(relist(secs(1801), true, false), Relist::Now);
    }
}
