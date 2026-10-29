//! A Linear team's or project's board as Pointman's own board draws it (Pointman's docs/dev/panes.md,
//! "Boards"; MB-343): Linear's workflow states as columns in the projects syntax's seven categories, its issues as cards, and
//! its cycles as sprints, through core's map (tracker.toml, the one its Linear mirror reads). Linear is
//! asked with the vault's API key, over its GraphQL API.
//!
//! The board, as the pane sends it:
//!
//! ```text
//! {project: {key, title, leading_tool: "linear", note?},
//!  states: [{name, category}],                       // the teams', in Linear's order
//!  items: [{id, ref, level, kind, title, state, category, owner, parent, sprint, target, priority,
//!           rank, url, labels, estimate, created_at, updated_at,
//!           body?, comments?, children?}],           // open issues, and those closed in the last 14 days;
//!                                                    // body, comments and children once it's opened
//!  groups: {sprints: [{id, name, start, end}], milestones: [{id, name, target}],
//!           people: [{handle, name, avatar}], labels: [{name, color}]}}
//! ```
//!
//! An item's `owner` is one of `people`'s handles (Linear's display name).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use regex::Regex;
use serde_json::{json, Map, Value};

/// What a board may come to: the stream's 1 MiB a frame, less room for the frame around it.
pub const CONTENT_MAX: usize = 1000 * 1024;
/// Pages of 100 issues one board asks for at most.
const PAGES: usize = 10;
/// How long a closed issue stays on the board.
const CLOSED_DAYS: u32 = 14;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub const ISSUE_FIELDS: &str = "id identifier title priority estimate dueDate url createdAt updatedAt sortOrder \
    team { id } state { id name type } assignee { id name displayName } labels(first: 10) { nodes { id name } } \
    parent { id identifier } cycle { id number name startsAt endsAt } projectMilestone { id name targetDate }";

/// An opened card's detail: its description, comments and sub-issues.
const DETAIL_FIELDS: &str = "id description children(first: 100) { nodes { identifier } } \
    comments(first: 100) { nodes { body createdAt user { name displayName } } }";

/// The people and labels a board offers, beside its teams': the workspace's own labels.
const WORKSPACE_LABELS: &str =
    "issueLabels(first: 250, filter: {team: {null: true}}) { nodes { id name color isGroup } }";

/// Linear's 0–4, as the projects syntax says them (tracker.toml's [priority]; Linear calls 3 "Medium").
pub fn priority_of(n: i64) -> Option<&'static str> {
    match n {
        1 => Some("urgent"),
        2 => Some("high"),
        3 => Some("normal"),
        4 => Some("low"),
        _ => None,
    }
}

/// A priority word back as Linear's number.
pub fn priority_number(word: &str) -> Option<i64> {
    match word {
        "none" | "" => Some(0),
        "urgent" => Some(1),
        "high" => Some(2),
        "normal" | "medium" => Some(3),
        "low" => Some(4),
        _ => None,
    }
}

// ------------------------------------------------------------------ the map

/// Core's tracker.toml for Linear, built in.
pub struct Tracker(Value);

impl Default for Tracker {
    fn default() -> Self {
        Tracker::from_toml(include_str!("../tracker.toml")).expect("tracker.toml reads")
    }
}

impl Tracker {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(Tracker(toml::from_str(text).context("tracker.toml")?))
    }

    /// A Linear workflow state's category: the first [[issue.import]] rule that matches its type and name.
    pub fn category(&self, state_type: &str, state_name: &str) -> String {
        for rule in self.0["issue"]["import"].as_array().into_iter().flatten() {
            if rule["type"].as_str() != Some(state_type) {
                continue;
            }
            if let Some(pattern) = rule["name"].as_str() {
                if !Regex::new(pattern).is_ok_and(|re| re.is_match(state_name)) {
                    continue;
                }
            }
            return rule["category"].as_str().unwrap_or("backlog").to_string();
        }
        "backlog".to_string()
    }

    /// An epic: the EPIC label, or a title like "[EPIC] …" (tracker.toml's [epic]).
    pub fn is_epic(&self, issue: &Value) -> bool {
        let epic = &self.0["epic"];
        let marked: Vec<String> =
            epic["labels"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
        let title = issue["title"].as_str().unwrap_or_default();
        labels(issue).iter().any(|l| marked.contains(&l.to_lowercase()))
            || epic["titles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|t| Regex::new(t).is_ok_and(|re| re.is_match(title)))
    }
}

fn labels(issue: &Value) -> Vec<String> {
    issue["labels"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["name"].as_str())
        .map(String::from)
        .collect()
}

/// A task's kind by Linear's default labels: bug, feature, else task.
pub fn kind_of(issue: &Value) -> &'static str {
    let names: Vec<String> = labels(issue).iter().map(|l| l.to_lowercase()).collect();
    if names.iter().any(|l| l == "bug") {
        "bug"
    } else if names.iter().any(|l| l == "feature") {
        "feature"
    } else {
        "task"
    }
}

/// Linear's sortOrder (a float, smaller first) as a rank that sorts as text: the apps order cards by rank.
pub fn rank(sort_order: f64) -> String {
    format!("{:020.6}", sort_order.clamp(-9.9e11, 9.9e11) + 1e12)
}

// ------------------------------------------------------------------ Linear

/// Linear's GraphQL API, as the key's owner.
pub struct Linear {
    url: String,
    key: String,
    agent: ureq::Agent,
}

/// What a board shows: one team's issues, or a Linear project's, across the teams it has.
#[derive(Clone, Debug)]
pub struct Scope {
    /// The board's project key: the team's, or one from the project's name.
    pub key: String,
    pub name: String,
    /// The Linear project's id, for a project's board: a new issue goes in it.
    pub project: Option<String>,
    /// Its issues, as Linear's IssueFilter.
    pub filter: Value,
    pub teams: Vec<Team>,
    /// The workspace's labels (each team's are on the team).
    pub labels: Vec<Value>,
}

const TEAM_FIELDS: &str = "id key name states { nodes { id name type position } } \
    cycles(first: 50, filter: {endsAt: {gt: \"-P60D\"}}) { nodes { id number name startsAt endsAt } } \
    members(first: 100) { nodes { id name displayName avatarUrl active } } \
    labels(first: 100) { nodes { id name color isGroup } }";

/// A team, as the board needs it: its states in Linear's order, its recent cycles, its people and labels.
#[derive(Clone, Debug)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
    pub states: Vec<Value>,
    pub cycles: Vec<Value>,
    pub members: Vec<Value>,
    pub labels: Vec<Value>,
}

impl Scope {
    /// The people on its teams who can be given a card, by name, each once.
    pub fn people(&self) -> Vec<Value> {
        let mut people: Vec<Value> = Vec::new();
        for m in self.teams.iter().flat_map(|t| &t.members) {
            if m["active"] != false && !people.iter().any(|p| p["id"] == m["id"]) {
                people.push(m.clone());
            }
        }
        people.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
        people
    }

    /// The labels a card on `team` can have: the team's and the workspace's (not label groups).
    pub fn labels_for<'a>(&'a self, team: &'a Team) -> impl Iterator<Item = &'a Value> {
        team.labels.iter().chain(&self.labels).filter(|l| l["isGroup"] != true)
    }

    /// An issue's team on this board.
    pub fn team_of(&self, issue: &Value) -> Option<&Team> {
        self.teams.iter().find(|t| issue["team"]["id"] == t.id.as_str())
    }
}

impl Linear {
    pub fn new(url: &str, key: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(CALL_TIMEOUT).build();
        Linear { url: url.to_string(), key: key.to_string(), agent }
    }

    /// One query or mutation: its `data`, or Linear's own words for what went wrong.
    pub fn gql(&self, query: &str, variables: Value) -> anyhow::Result<Value> {
        let sent = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.key)
            .set("Content-Type", "application/json")
            .send_json(json!({"query": query, "variables": variables}));
        let body: Value = match sent {
            Ok(r) => r.into_json().context("Linear's answer")?,
            Err(ureq::Error::Status(code, r)) => {
                let body: Value = r.into_json().unwrap_or(Value::Null);
                if body["errors"].is_array() {
                    body
                } else if code == 401 || code == 403 {
                    bail!("Linear refused the API key ({code}): add a working one with /linear add");
                } else {
                    bail!("Linear answered {code}");
                }
            }
            Err(e) => bail!("couldn't reach Linear: {e}"),
        };
        if let Some(errors) = body["errors"].as_array().filter(|e| !e.is_empty()) {
            let said: Vec<String> = errors
                .iter()
                .map(|e| {
                    let user = &e["extensions"]["userPresentableMessage"];
                    user.as_str().or(e["message"].as_str()).unwrap_or("an error").to_string()
                })
                .collect();
            bail!("Linear: {}", said.join("; "));
        }
        Ok(body["data"].clone())
    }

    /// A team's board, by its key.
    pub fn team(&self, key: &str) -> anyhow::Result<Scope> {
        let data = self.gql(
            &format!(
                "query($key: String!) {{ teams(filter: {{key: {{eq: $key}}}}) {{ nodes {{ {TEAM_FIELDS} }} }} \
                   {WORKSPACE_LABELS} }}"
            ),
            json!({"key": key}),
        )?;
        let t = data["teams"]["nodes"].get(0).ok_or_else(|| anyhow!("Linear has no team {key} for this key"))?;
        let team = team_of(t);
        Ok(Scope {
            key: team.key.clone(),
            name: team.name.clone(),
            project: None,
            filter: team_filter(&team.key),
            teams: vec![team],
            labels: nodes(&data["issueLabels"]),
        })
    }

    /// A Linear project's board, by its id: its issues from every team it has.
    pub fn project(&self, id: &str) -> anyhow::Result<Scope> {
        let data = self.gql(
            &format!(
                "query($id: String!) {{ project(id: $id) {{ id name teams(first: 10) {{ nodes {{ {TEAM_FIELDS} }} }} }} \
                   {WORKSPACE_LABELS} }}"
            ),
            json!({"id": id}),
        )?;
        let p = &data["project"];
        let name = p["name"].as_str().ok_or_else(|| anyhow!("Linear has no project {id} for this key"))?;
        let teams: Vec<Team> = p["teams"]["nodes"].as_array().into_iter().flatten().map(team_of).collect();
        Ok(Scope {
            key: key_for(name).unwrap_or_else(|| teams.first().map(|t| t.key.clone()).unwrap_or_default()),
            name: name.to_string(),
            project: p["id"].as_str().map(String::from),
            filter: project_filter(p["id"].as_str().unwrap_or(id)),
            teams,
            labels: nodes(&data["issueLabels"]),
        })
    }

    /// Every team the key sees, by name: what a project can be connected to.
    pub fn teams(&self) -> anyhow::Result<Vec<Value>> {
        let data = self.gql("query { teams(first: 250) { nodes { id key name } } }", json!({}))?;
        let mut teams = data["teams"]["nodes"].as_array().cloned().unwrap_or_default();
        teams.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
        Ok(teams)
    }

    /// Every Linear project the key sees that isn't completed or canceled, by name.
    pub fn projects(&self) -> anyhow::Result<Vec<Value>> {
        let query = "query($after: String) { projects(first: 100, after: $after, \
                       filter: {status: {type: {nin: [\"completed\", \"canceled\"]}}}) { \
                       nodes { id name } pageInfo { hasNextPage endCursor } } }";
        let (mut out, mut after) = (Vec::new(), Value::Null);
        for _ in 0..PAGES {
            let page = self.gql(query, json!({"after": after}))?;
            let projects = &page["projects"];
            out.extend(projects["nodes"].as_array().cloned().unwrap_or_default());
            if projects["pageInfo"]["hasNextPage"] != true {
                break;
            }
            after = projects["pageInfo"]["endCursor"].clone();
        }
        out.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
        Ok(out)
    }

    /// A board's issues (its scope's filter): open ones, and those closed in the last 14 days. With
    /// `since`, only those updated after it, closed or not (what changed since the last look).
    pub fn issues(&self, scope: &Value, since: Option<&str>) -> anyhow::Result<Vec<Value>> {
        let mut filter = scope.clone();
        match since {
            Some(since) => filter["updatedAt"] = json!({"gt": since}),
            None => {
                let recent = format!("-P{CLOSED_DAYS}D");
                filter["or"] = json!([{"state": {"type": {"nin": ["completed", "canceled"]}}},
                                      {"completedAt": {"gt": recent}}, {"canceledAt": {"gt": recent}}]);
            }
        }
        let query = format!(
            "query($filter: IssueFilter, $after: String) {{ issues(filter: $filter, first: 100, after: $after) {{ \
               nodes {{ {ISSUE_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }}"
        );
        let (mut out, mut after) = (Vec::new(), Value::Null);
        for _ in 0..PAGES {
            let page = self.gql(&query, json!({"filter": filter, "after": after}))?;
            let issues = &page["issues"];
            out.extend(issues["nodes"].as_array().cloned().unwrap_or_default());
            if issues["pageInfo"]["hasNextPage"] != true {
                break;
            }
            after = issues["pageInfo"]["endCursor"].clone();
        }
        Ok(out)
    }

    /// issueUpdate: the issue as Linear has it after, or Linear's reason it didn't.
    pub fn update(&self, id: &str, input: Value) -> anyhow::Result<Value> {
        let query = format!(
            "mutation($id: String!, $input: IssueUpdateInput!) {{ issueUpdate(id: $id, input: $input) {{ \
               success issue {{ {ISSUE_FIELDS} }} }} }}"
        );
        let done = &self.gql(&query, json!({"id": id, "input": input}))?["issueUpdate"];
        if done["success"] != true {
            bail!("Linear didn't make the change");
        }
        Ok(done["issue"].clone())
    }

    /// issueCreate: the new issue as Linear has it.
    pub fn create(&self, input: Value) -> anyhow::Result<Value> {
        let query = format!(
            "mutation($input: IssueCreateInput!) {{ issueCreate(input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }} }}"
        );
        let done = &self.gql(&query, json!({"input": input}))?["issueCreate"];
        if done["success"] != true {
            bail!("Linear didn't make the issue");
        }
        Ok(done["issue"].clone())
    }

    /// commentCreate on an issue.
    pub fn comment(&self, issue: &str, body: &str) -> anyhow::Result<()> {
        let done = &self.gql(
            "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
            json!({"input": {"issueId": issue, "body": body}}),
        )?["commentCreate"];
        if done["success"] != true {
            bail!("Linear didn't add the comment");
        }
        Ok(())
    }

    /// An opened card's detail, as the board adds it to the item: `{body, comments, children}`.
    pub fn detail(&self, id: &str) -> anyhow::Result<Value> {
        let data =
            self.gql(&format!("query($id: String!) {{ issue(id: $id) {{ {DETAIL_FIELDS} }} }}"), json!({"id": id}))?;
        Ok(detail_of(&data["issue"]))
    }

    /// One issue by its ref (ALA-12) or id, with its description, comments and sub-issues, or why there isn't one.
    pub fn find(&self, id: &str) -> anyhow::Result<Value> {
        let data = self.gql(
            &format!(
                "query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} team {{ id key }} \
                   project {{ id name }} {DETAIL_FIELDS} }} }}"
            ),
            json!({"id": id}),
        )?;
        match data["issue"].is_object() {
            true => Ok(data["issue"].clone()),
            false => bail!("Linear has no issue {id} for this key"),
        }
    }

    /// Issues matching a filter, the latest changed first, at most `limit`.
    pub fn search(&self, filter: &Value, limit: usize) -> anyhow::Result<Vec<Value>> {
        let data = self.gql(
            &format!(
                "query($filter: IssueFilter, $first: Int) {{ issues(filter: $filter, first: $first, \
                   orderBy: updatedAt) {{ nodes {{ {ISSUE_FIELDS} team {{ id key }} project {{ id name }} }} }} }}"
            ),
            json!({"filter": filter, "first": limit.clamp(1, 100)}),
        )?;
        Ok(nodes(&data["issues"]))
    }
}

/// Linear's IssueFilter for a search: words in the title or description, a team (its key), a state (its
/// name), an owner (a display name, or `none`) and a project (its name, or its id as a UUID). Open issues only, unless `all`.
pub fn search_filter(
    words: Option<&str>,
    team: Option<&str>,
    state: Option<&str>,
    owner: Option<&str>,
    project: Option<&str>,
    all: bool,
) -> Value {
    let mut filter = Map::new();
    if let Some(words) = words.map(str::trim).filter(|w| !w.is_empty()) {
        filter.insert(
            "or".into(),
            json!([{"title": {"containsIgnoreCase": words}}, {"description": {"containsIgnoreCase": words}}]),
        );
    }
    if let Some(team) = team {
        filter.insert("team".into(), json!({"key": {"eqIgnoreCase": team}}));
    }
    if let Some(state) = state {
        filter.insert("state".into(), json!({"name": {"eqIgnoreCase": state}}));
    } else if !all {
        filter.insert("state".into(), json!({"type": {"nin": ["completed", "canceled"]}}));
    }
    if let Some(owner) = owner {
        let who = if owner == "none" { json!({"null": true}) } else { json!({"displayName": {"eqIgnoreCase": owner}}) };
        filter.insert("assignee".into(), who);
    }
    if let Some(project) = project {
        // Linear checks an id filter's value is a UUID, so the shape of what's given says which it is
        let uuid = Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .expect("compiles");
        let by = if uuid.is_match(project) {
            json!({"id": {"eq": project}})
        } else {
            json!({"name": {"eqIgnoreCase": project}})
        };
        filter.insert("project".into(), by);
    }
    Value::Object(filter)
}

/// An issue as an agent reads it: the board's item with its team and project, and its detail when it has it.
pub fn issue_out(map: &Tracker, issue: &Value) -> Value {
    let detail = issue["description"].is_string().then(|| detail_of(issue));
    let mut out = item(map, issue, detail.as_ref());
    out["team"] = issue["team"]["key"].clone();
    out["project"] = issue["project"]["name"].clone();
    out
}

/// Issues as a line each, for the text beside them.
pub fn issue_lines(issues: &[Value]) -> String {
    if issues.is_empty() {
        return "No issues".into();
    }
    let line = |i: &Value| {
        let (state, owner) = (i["state"].as_str().unwrap_or("?"), i["owner"].as_str().unwrap_or("nobody"));
        format!("{} {} [{state}, {owner}]", i["ref"].as_str().unwrap_or("?"), i["title"].as_str().unwrap_or_default())
    };
    issues.iter().map(line).collect::<Vec<_>>().join("\n")
}

// ------------------------------------------------------------------ the board

/// One issue as a board item, with its detail (body, comments, children) once it's been opened.
pub fn item(map: &Tracker, issue: &Value, detail: Option<&Value>) -> Value {
    let state = &issue["state"];
    let cycle = &issue["cycle"];
    let sprint = match cycle["name"].as_str().filter(|n| !n.is_empty()) {
        Some(name) => json!(name),
        None => cycle["number"].as_u64().map_or(Value::Null, |n| json!(format!("Cycle {n}"))),
    };
    let mut out = json!({
        "id": issue["id"], "ref": issue["identifier"],
        "level": if map.is_epic(issue) { "epic" } else { "task" }, "kind": kind_of(issue),
        "title": issue["title"], "state": state["name"],
        "category": map.category(state["type"].as_str().unwrap_or_default(), state["name"].as_str().unwrap_or_default()),
        "owner": handle(&issue["assignee"]), "parent": issue["parent"]["identifier"], "sprint": sprint,
        "target": issue["dueDate"], "priority": priority_of(issue["priority"].as_i64().unwrap_or(0)),
        "rank": rank(issue["sortOrder"].as_f64().unwrap_or(0.0)), "url": issue["url"],
        "labels": labels(issue), "estimate": issue["estimate"],
        "created_at": issue["createdAt"], "updated_at": issue["updatedAt"],
    });
    if let Some(Value::Object(d)) = detail {
        for (k, v) in d {
            out[k] = v.clone();
        }
    }
    out
}

/// A Linear user as a board's handle: their display name.
pub fn handle(user: &Value) -> Value {
    user["displayName"].as_str().or(user["name"].as_str()).map_or(Value::Null, |h| json!(h))
}

fn nodes(connection: &Value) -> Vec<Value> {
    connection["nodes"].as_array().cloned().unwrap_or_default()
}

/// An issue's detail as the item carries it: the description as `body`, its comments (oldest first) and
/// its sub-issues by ref.
pub fn detail_of(issue: &Value) -> Value {
    let mut comments = nodes(&issue["comments"]);
    comments
        .sort_by(|a, b| a["createdAt"].as_str().unwrap_or_default().cmp(b["createdAt"].as_str().unwrap_or_default()));
    let comments: Vec<Value> = comments
        .iter()
        .map(|c| json!({"author": handle(&c["user"]), "body": c["body"], "at": c["createdAt"]}))
        .collect();
    let children: Vec<Value> = nodes(&issue["children"]).iter().map(|c| c["identifier"].clone()).collect();
    json!({"body": issue["description"].as_str().unwrap_or_default(), "comments": comments, "children": children})
}

/// The board for a team or a project, from its issues by id, cut to `max` bytes: the longest-closed
/// issues go first, then the least recently changed, and the project says how many. A project across
/// teams has each team's states as columns, by name, the first team's order first.
pub fn board(
    map: &Tracker,
    scope: &Scope,
    issues: &HashMap<String, Value>,
    details: &HashMap<String, Value>,
    max: usize,
) -> Value {
    let mut states: Vec<Value> = Vec::new();
    for s in scope.teams.iter().flat_map(|t| &t.states) {
        let name = s["name"].as_str().unwrap_or_default();
        if !states.iter().any(|x| x["name"] == name) {
            states.push(json!({"name": name, "category": map.category(s["type"].as_str().unwrap_or_default(), name)}));
        }
    }
    let mut cycles: Vec<&Value> = Vec::new();
    for c in scope.teams.iter().flat_map(|t| &t.cycles) {
        if !cycles.iter().any(|x| x["id"] == c["id"]) {
            cycles.push(c);
        }
    }
    cycles.sort_by(|a, b| a["startsAt"].as_str().unwrap_or_default().cmp(b["startsAt"].as_str().unwrap_or_default()));
    let sprints: Vec<Value> = cycles
        .iter()
        .map(|c| {
            let name = c["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
            json!({"id": c["id"], "name": name.unwrap_or_else(|| format!("Cycle {}", c["number"])),
                   "start": c["startsAt"].as_str().map(|s| &s[..s.len().min(10)]),
                   "end": c["endsAt"].as_str().map(|s| &s[..s.len().min(10)])})
        })
        .collect();
    let mut milestones: Vec<Value> = Vec::new();
    for issue in issues.values() {
        let m = &issue["projectMilestone"];
        if m.is_object() && !milestones.iter().any(|x| x["id"] == m["id"]) {
            milestones.push(json!({"id": m["id"], "name": m["name"], "target": m["targetDate"]}));
        }
    }
    milestones.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
    // open first, then the most recently changed: what's kept when it has to be cut
    let mut ordered: Vec<&Value> = issues.values().collect();
    let closed = |i: &Value| matches!(i["state"]["type"].as_str(), Some("completed" | "canceled"));
    ordered.sort_by(|a, b| {
        closed(a)
            .cmp(&closed(b))
            .then_with(|| b["updatedAt"].as_str().unwrap_or_default().cmp(a["updatedAt"].as_str().unwrap_or_default()))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let mut items: Vec<Value> =
        ordered.into_iter().map(|i| item(map, i, i["id"].as_str().and_then(|id| details.get(id)))).collect();
    let people: Vec<Value> = scope
        .people()
        .iter()
        .map(|p| json!({"handle": handle(p), "name": p["name"], "avatar": p["avatarUrl"]}))
        .collect();
    let mut labels: Vec<Value> = Vec::new();
    for l in scope.teams.iter().flat_map(|t| scope.labels_for(t)) {
        if !labels.iter().any(|x| x["name"] == l["name"]) {
            labels.push(json!({"name": l["name"], "color": l["color"]}));
        }
    }
    labels.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
    let mut out = json!({
        "project": {"key": scope.key, "title": scope.name, "leading_tool": "linear"},
        "states": states, "items": [],
        "groups": {"sprints": sprints, "milestones": milestones, "people": people, "labels": labels},
    });
    let mut size = serde_json::to_vec(&out).map_or(0, |b| b.len()) + 200;
    let mut kept = Vec::new();
    for it in items.drain(..) {
        let n = serde_json::to_vec(&it).map_or(0, |b| b.len()) + 1;
        if size + n > max {
            break;
        }
        size += n;
        kept.push(it);
    }
    // the kept ones oldest first, the order an issue never leaves: the apps order cards by rank, and a change
    // goes as edits to its card only while the list keeps its order (a new issue comes last)
    kept.sort_by(|a, b| {
        a["created_at"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["created_at"].as_str().unwrap_or_default())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let left = issues.len() - kept.len();
    if left > 0 {
        let s = if left == 1 { "" } else { "s" };
        out["project"]["note"] = json!(format!("{left} older issue{s} left out: Linear has them"));
    }
    out["items"] = Value::Array(kept);
    out
}

/// What the board opens by, for the apps' Add a project board (Pointman's MB-376–378): each team as
/// `{target: {team}, name, key, kind: "team"}`, then each Linear project as `{target: {project: id}, name,
/// key, kind: "project"}`. `key` is the project key it suggests: a team's own, or one from a project's name.
pub fn targets(teams: &[Value], projects: &[Value]) -> Vec<Value> {
    let teams = teams.iter().filter_map(|t| {
        let key = t["key"].as_str()?;
        Some(json!({"target": {"team": key}, "name": t["name"].as_str().unwrap_or(key), "key": key, "kind": "team"}))
    });
    let projects = projects.iter().filter_map(|p| {
        let (id, name) = (p["id"].as_str()?, p["name"].as_str()?);
        Some(json!({"target": {"project": id}, "name": name, "key": key_for(name), "kind": "project"}))
    });
    teams.chain(projects).collect()
}

/// A team's issues, as Linear's IssueFilter: what its board shows.
pub fn team_filter(key: &str) -> Value {
    json!({"team": {"key": {"eq": key}}})
}

/// A Linear project's issues, as Linear's IssueFilter.
pub fn project_filter(id: &str) -> Value {
    json!({"project": {"id": {"eq": id}}})
}

/// A project key from a name, as the apps suggest one (2–10 capitals and digits, a letter first): its
/// words' first letters (Admin Panel Rinse: APR), or a one-word name's first three (Spark: SPA).
pub fn key_for(name: &str) -> Option<String> {
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    let initials: String = words.iter().filter_map(|w| w.chars().next()).collect();
    let key: String = if words.len() > 1 && initials.len() >= 2 {
        initials.chars().take(10).collect()
    } else {
        words.first()?.chars().take(3).collect()
    };
    Regex::new(r"^[A-Z][A-Z0-9]{1,9}$").ok()?.is_match(&key).then_some(key)
}

fn team_of(t: &Value) -> Team {
    let mut states = t["states"]["nodes"].as_array().cloned().unwrap_or_default();
    states.sort_by(|a, b| a["position"].as_f64().unwrap_or(0.0).total_cmp(&b["position"].as_f64().unwrap_or(0.0)));
    let mut cycles = t["cycles"]["nodes"].as_array().cloned().unwrap_or_default();
    cycles.sort_by(|a, b| a["startsAt"].as_str().unwrap_or_default().cmp(b["startsAt"].as_str().unwrap_or_default()));
    let key = t["key"].as_str().unwrap_or_default();
    Team {
        id: t["id"].as_str().unwrap_or_default().to_string(),
        key: key.to_string(),
        name: t["name"].as_str().unwrap_or(key).to_string(),
        states,
        cycles,
        members: nodes(&t["members"]),
        labels: nodes(&t["labels"]),
    }
}

/// Where an issue goes between its neighbours in its column: the sortOrder that puts it just before or
/// after `next_to`, among `column` (the column's issues, the moving one left out).
pub fn sort_between(column: &[Value], next_to: &str, before: bool) -> Option<f64> {
    let mut orders: Vec<(String, f64)> = column
        .iter()
        .map(|i| (i["id"].as_str().unwrap_or_default().to_string(), i["sortOrder"].as_f64().unwrap_or(0.0)))
        .collect();
    orders.sort_by(|a, b| a.1.total_cmp(&b.1));
    let at = orders.iter().position(|(id, _)| id == next_to)?;
    let here = orders[at].1;
    let other = if before { at.checked_sub(1).map(|i| orders[i].1) } else { orders.get(at + 1).map(|o| o.1) };
    Some(match other {
        Some(o) => (here + o) / 2.0,
        None if before => here - 1.0,
        None => here + 1.0,
    })
}

/// The newest `updatedAt` among issues: where the next look starts.
pub fn newest(issues: &HashMap<String, Value>) -> Option<String> {
    issues.values().filter_map(|i| i["updatedAt"].as_str()).max().map(String::from)
}

/// Issues by id.
pub fn by_id(issues: Vec<Value>) -> HashMap<String, Value> {
    issues.into_iter().filter_map(|i| Some((i["id"].as_str()?.to_string(), i))).collect()
}

/// A card's fields as Linear's issue input, for an edit or a new card on `team`: title, body, state,
/// owner (a handle, or null), priority, target, estimate, labels (names), sprint (an id or a name, or null)
/// and parent (a ref, or null), each checked against the board. Anything else, or a value the board
/// doesn't have, says why.
pub fn issue_input(
    fields: &Map<String, Value>,
    scope: &Scope,
    team: &Team,
    issues: &HashMap<String, Value>,
) -> Result<Value, String> {
    fn text(v: &Value) -> Option<&str> {
        v.as_str().map(str::trim).filter(|t| !t.is_empty())
    }
    // a value as a person typed it, for saying why it's refused
    fn said(v: &Value) -> String {
        v.as_str().map_or_else(|| v.to_string(), String::from)
    }
    let date = Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("compiles");
    let mut input = Map::new();
    for (field, value) in fields {
        let (to, v) = match field.as_str() {
            "title" => ("title", json!(text(value).ok_or("a title can't be empty")?)),
            "body" => ("description", json!(value.as_str().unwrap_or_default())),
            "state" => {
                let state = team.states.iter().find(|s| s["name"] == *value);
                let state = state.ok_or_else(|| format!("{} isn't one of {}'s states", said(value), team.key))?;
                ("stateId", state["id"].clone())
            }
            "owner" => match text(value) {
                None => ("assigneeId", Value::Null),
                Some(h) => {
                    let person = scope.people().into_iter().find(|p| handle(p) == h || p["id"] == h);
                    let person = person.ok_or_else(|| format!("{h} isn't one of {}'s people", team.key))?;
                    ("assigneeId", person["id"].clone())
                }
            },
            "priority" => {
                let word = value.as_str().unwrap_or("none");
                let n = priority_number(word)
                    .ok_or_else(|| format!("{} isn't a priority: urgent, high, normal, low or none", said(value)))?;
                ("priority", json!(n))
            }
            "target" => match text(value) {
                None => ("dueDate", Value::Null),
                Some(d) if date.is_match(d) => ("dueDate", json!(d)),
                Some(d) => return Err(format!("{d} isn't a date: YYYY-MM-DD")),
            },
            "estimate" => match value {
                Value::Null => ("estimate", Value::Null),
                Value::Number(n) => ("estimate", json!(n.as_f64().unwrap_or(0.0).round() as i64)),
                other => return Err(format!("{} isn't an estimate: a number", said(other))),
            },
            "labels" => {
                let names = value.as_array().ok_or("labels are a list of names")?;
                let mut ids = Vec::new();
                for name in names {
                    let found = scope.labels_for(team).find(|l| l["name"] == *name);
                    let found = found.ok_or_else(|| format!("{} isn't one of {}'s labels", said(name), team.key))?;
                    ids.push(found["id"].clone());
                }
                ("labelIds", Value::Array(ids))
            }
            "sprint" => match text(value) {
                None => ("cycleId", Value::Null),
                Some(s) => {
                    let named = |c: &Value| {
                        c["id"] == s
                            || c["name"] == s
                            || c["number"].as_u64().is_some_and(|n| format!("Cycle {n}") == s)
                    };
                    let cycle = team.cycles.iter().find(|c| named(c));
                    ("cycleId", cycle.ok_or_else(|| format!("{s} isn't one of {}'s sprints", team.key))?["id"].clone())
                }
            },
            "parent" => match text(value) {
                None => ("parentId", Value::Null),
                Some(r) => {
                    let parent = issues.values().find(|i| i["identifier"] == r || i["id"] == r);
                    ("parentId", parent.ok_or_else(|| format!("{r} isn't on this board"))?["id"].clone())
                }
            },
            other => return Err(format!("{other} isn't something a Linear card has here")),
        };
        input.insert(to.into(), v);
    }
    if input.is_empty() {
        return Err("nothing to change".into());
    }
    Ok(Value::Object(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str, state: (&str, &str), order: f64, updated: &str) -> Value {
        json!({"id": id, "identifier": format!("ALA-{id}"), "title": format!("Issue {id}"), "priority": 2,
               "dueDate": null, "url": format!("https://linear.app/a/issue/ALA-{id}"), "updatedAt": updated,
               "sortOrder": order, "state": {"id": format!("s-{}", state.1), "name": state.1, "type": state.0},
               "assignee": {"id": "u1", "name": "James Briggs", "displayName": "james"}, "labels": {"nodes": []},
               "team": {"id": "t"}, "estimate": null, "createdAt": "2026-10-01T00:00:00.000Z",
               "parent": null, "cycle": null, "projectMilestone": null})
    }

    /// ALA (Spark): two states, one cycle, two people (one gone), a team label and a label group; the
    /// workspace has a label of its own.
    fn scope() -> Scope {
        let team = Team {
            id: "t".into(),
            key: "ALA".into(),
            name: "Spark".into(),
            states: vec![
                json!({"id": "s-Todo", "name": "Todo", "type": "unstarted"}),
                json!({"id": "s-In Review", "name": "In Review", "type": "started"}),
            ],
            cycles: vec![json!({"id": "c1", "number": 41, "name": null, "startsAt": "2026-10-05T00:00:00.000Z",
                                "endsAt": "2026-10-19T00:00:00.000Z"})],
            members: vec![
                json!({"id": "u1", "name": "James Briggs", "displayName": "james",
                       "avatarUrl": "https://public.linear.app/j.png", "active": true}),
                json!({"id": "u2", "name": "Ada Lovelace", "displayName": "ada", "avatarUrl": null, "active": true}),
                json!({"id": "u3", "name": "Gone Person", "displayName": "gone", "avatarUrl": null, "active": false}),
            ],
            labels: vec![
                json!({"id": "l-bug", "name": "Bug", "color": "#eb5757", "isGroup": false}),
                json!({"id": "l-area", "name": "Area", "color": "#000000", "isGroup": true}),
            ],
        };
        Scope {
            key: "V".into(),
            name: "Voice".into(),
            project: None,
            filter: json!({}),
            teams: vec![team],
            labels: vec![json!({"id": "l-feat", "name": "Feature", "color": "#bb87fc", "isGroup": false})],
        }
    }

    #[test]
    fn states_map_as_the_mirror_files_them() {
        let map = Tracker::default();
        assert_eq!(map.category("started", "In Review"), "review");
        assert_eq!(map.category("started", "In QA"), "review");
        assert_eq!(map.category("started", "On Hold"), "active");
        assert_eq!(map.category("started", "In Progress"), "active");
        assert_eq!(map.category("unstarted", "Todo"), "todo");
        assert_eq!(map.category("canceled", "Duplicate"), "dropped");
        assert_eq!(map.category("completed", "Done"), "done");
        assert_eq!(map.category("triage", "Triage"), "triage");
        assert_eq!(map.category("weird", "?"), "backlog");
    }

    #[test]
    fn epics_kinds_priorities_and_ranks() {
        let map = Tracker::default();
        let mut i = issue("1", ("started", "In Progress"), 3.0, "2026-10-08T00:00:00.000Z");
        assert!(!map.is_epic(&i));
        i["title"] = json!("[EPIC] Voice");
        assert!(map.is_epic(&i));
        i["title"] = json!("Plain");
        i["labels"] = json!({"nodes": [{"name": "Epic"}, {"name": "Bug"}]});
        assert!(map.is_epic(&i));
        assert_eq!(kind_of(&i), "bug");
        assert_eq!(priority_of(3), Some("normal"));
        assert_eq!(priority_of(0), None);
        assert_eq!(priority_number("medium"), Some(3));
        let ranks = [rank(-5.5), rank(-1.0), rank(0.0), rank(2.25), rank(10.0), rank(100000.0)];
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
        assert_eq!(ranks[0].len(), ranks[5].len());
    }

    #[test]
    fn project_keys_come_from_names() {
        assert_eq!(key_for("Admin Panel Rinse").as_deref(), Some("APR"));
        assert_eq!(key_for("Spark").as_deref(), Some("SPA"));
        assert_eq!(key_for("2026 launch"), None); // a key starts with a letter
        assert_eq!(key_for("Q4").as_deref(), Some("Q4"));
        assert_eq!(key_for("A b c d e f g h i j k l").as_deref(), Some("ABCDEFGHIJ"));
        assert_eq!(key_for("—"), None);
    }

    #[test]
    fn the_board_has_the_teams_states_and_its_issues() {
        let map = Tracker::default();
        let scope = scope();
        let mut a = issue("1", ("started", "In Review"), 1.0, "2026-10-08T01:00:00.000Z");
        a["cycle"] = scope.teams[0].cycles[0].clone();
        a["labels"] = json!({"nodes": [{"id": "l-bug", "name": "Bug"}]});
        a["estimate"] = json!(3);
        a["projectMilestone"] = json!({"id": "m1", "name": "Beta", "targetDate": "2026-11-01"});
        let b = issue("2", ("completed", "Done"), 2.0, "2026-10-07T00:00:00.000Z");
        let issues = by_id(vec![a, b]);
        let opened = HashMap::from([("2".to_string(), json!({"body": "Why", "comments": [], "children": ["ALA-9"]}))]);
        let board = board(&map, &scope, &issues, &opened, CONTENT_MAX);
        assert_eq!(board["project"], json!({"key": "V", "title": "Voice", "leading_tool": "linear"}));
        assert_eq!(
            board["states"],
            json!([{"name": "Todo", "category": "todo"}, {"name": "In Review", "category": "review"}])
        );
        let first = &board["items"][0];
        assert_eq!(
            (first["ref"].as_str(), first["category"].as_str(), first["sprint"].as_str()),
            (Some("ALA-1"), Some("review"), Some("Cycle 41"))
        );
        assert_eq!((first["owner"].as_str(), first["priority"].as_str()), (Some("james"), Some("high")));
        assert_eq!((&first["labels"], &first["estimate"]), (&json!(["Bug"]), &json!(3)));
        assert_eq!(
            (first["created_at"].as_str(), first["updated_at"].as_str()),
            (Some("2026-10-01T00:00:00.000Z"), Some("2026-10-08T01:00:00.000Z"))
        );
        assert!(first.get("body").is_none(), "a card's detail comes once it's opened");
        assert_eq!(board["items"][1]["category"], "done");
        assert_eq!((&board["items"][1]["body"], &board["items"][1]["children"]), (&json!("Why"), &json!(["ALA-9"])));
        // the people who can be given a card, and the labels a card can have
        assert_eq!(
            board["groups"]["people"],
            json!([{"handle": "ada", "name": "Ada Lovelace", "avatar": null},
                   {"handle": "james", "name": "James Briggs", "avatar": "https://public.linear.app/j.png"}])
        );
        assert_eq!(
            board["groups"]["labels"],
            json!([{"name": "Bug", "color": "#eb5757"}, {"name": "Feature", "color": "#bb87fc"}])
        );
        assert_eq!(
            board["groups"]["sprints"],
            json!([{"id": "c1", "name": "Cycle 41", "start": "2026-10-05", "end": "2026-10-19"}])
        );
        assert_eq!(board["groups"]["milestones"], json!([{"id": "m1", "name": "Beta", "target": "2026-11-01"}]));
        // cut to fit: the closed issue goes first
        let none = HashMap::new();
        let open_only = super::board(&map, &scope, &by_id(vec![issues["1"].clone()]), &none, CONTENT_MAX);
        let max = serde_json::to_vec(&open_only).unwrap().len() + 200 + 5;
        let small = super::board(&map, &scope, &issues, &none, max);
        assert_eq!(small["items"].as_array().unwrap().len(), 1);
        assert_eq!(small["items"][0]["ref"], "ALA-1");
        assert_eq!(small["project"]["note"], "1 older issue left out: Linear has them");
    }

    #[test]
    fn a_rank_goes_between_its_neighbours() {
        let col = vec![
            json!({"id": "a", "sortOrder": 1.0}),
            json!({"id": "b", "sortOrder": 3.0}),
            json!({"id": "c", "sortOrder": 7.0}),
        ];
        assert_eq!(sort_between(&col, "b", true), Some(2.0));
        assert_eq!(sort_between(&col, "b", false), Some(5.0));
        assert_eq!(sort_between(&col, "a", true), Some(0.0));
        assert_eq!(sort_between(&col, "c", false), Some(8.0));
        assert_eq!(sort_between(&col, "z", true), None);
    }

    #[test]
    fn a_cards_fields_are_checked_against_the_board() {
        let scope = scope();
        let team = &scope.teams[0];
        let issues = by_id(vec![issue("7", ("unstarted", "Todo"), 1.0, "2026-10-08T00:00:00.000Z")]);
        let input = |v: Value| issue_input(v.as_object().unwrap(), &scope, team, &issues);
        assert_eq!(
            input(json!({"title": " New ", "priority": "low", "target": "2026-10-20", "body": "Steps:\n1. …"}))
                .unwrap(),
            json!({"title": "New", "priority": 4, "dueDate": "2026-10-20", "description": "Steps:\n1. …"})
        );
        assert_eq!(
            input(json!({"owner": "ada", "labels": ["Bug", "Feature"], "sprint": "Cycle 41", "estimate": 2,
                         "parent": "ALA-7", "state": "In Review"}))
            .unwrap(),
            json!({"assigneeId": "u2", "labelIds": ["l-bug", "l-feat"], "cycleId": "c1", "estimate": 2,
                   "parentId": "7", "stateId": "s-In Review"})
        );
        assert_eq!(
            input(json!({"owner": null, "sprint": null, "parent": null, "target": null, "priority": "none"})).unwrap(),
            json!({"assigneeId": null, "cycleId": null, "parentId": null, "dueDate": null, "priority": 0})
        );
        assert_eq!(
            input(json!({"sprint": "c1", "priority": "medium"})).unwrap(),
            json!({"cycleId": "c1", "priority": 3})
        );
        let refused = |v: Value| input(v).unwrap_err();
        assert_eq!(refused(json!({"owner": "gone"})), "gone isn't one of ALA's people");
        assert_eq!(refused(json!({"labels": ["Area"]})), "Area isn't one of ALA's labels");
        assert_eq!(refused(json!({"sprint": "Cycle 2"})), "Cycle 2 isn't one of ALA's sprints");
        assert_eq!(refused(json!({"parent": "ALA-99"})), "ALA-99 isn't on this board");
        assert_eq!(refused(json!({"state": "Shipped"})), "Shipped isn't one of ALA's states");
        assert_eq!(refused(json!({"target": "soon"})), "soon isn't a date: YYYY-MM-DD");
        assert_eq!(refused(json!({"priority": "huge"})), "huge isn't a priority: urgent, high, normal, low or none");
        assert_eq!(refused(json!({"title": ""})), "a title can't be empty");
        assert_eq!(refused(json!({"color": "red"})), "color isn't something a Linear card has here");
        assert_eq!(refused(json!({})), "nothing to change");
    }

    #[test]
    fn a_search_filters_a_project_by_the_shape_of_what_it_is_given() {
        let by = |p| search_filter(None, None, None, None, Some(p), false)["project"].clone();
        assert_eq!(by("Spark Voice"), json!({"name": {"eqIgnoreCase": "Spark Voice"}}));
        let id = "2b6f1c0e-8d3a-4c1f-9a7e-5d4b3c2a1f00";
        assert_eq!(by(id), json!({"id": {"eq": id}}));
        let f = search_filter(Some("login"), Some("ala"), None, Some("none"), None, false);
        assert_eq!(f["assignee"], json!({"null": true}));
        assert_eq!(f["state"], json!({"type": {"nin": ["completed", "canceled"]}}));
        assert_eq!(f["team"], json!({"key": {"eqIgnoreCase": "ala"}}));
        assert!(search_filter(None, None, None, None, None, true).get("state").is_none(), "all includes closed ones");
    }

    #[test]
    fn a_cards_detail_is_its_body_comments_and_children() {
        let detail = detail_of(&json!({
            "id": "7", "description": null,
            "children": {"nodes": [{"identifier": "ALA-8"}]},
            "comments": {"nodes": [
                {"body": "second", "createdAt": "2026-10-09T02:00:00.000Z", "user": {"name": "Ada Lovelace", "displayName": "ada"}},
                {"body": "first", "createdAt": "2026-10-09T01:00:00.000Z", "user": null}]}}));
        assert_eq!(
            detail,
            json!({"body": "", "children": ["ALA-8"], "comments": [
                {"author": null, "body": "first", "at": "2026-10-09T01:00:00.000Z"},
                {"author": "ada", "body": "second", "at": "2026-10-09T02:00:00.000Z"}]})
        );
    }
}
