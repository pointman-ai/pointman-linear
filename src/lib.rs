//! A Linear team's board as Pointman's own board draws it (Pointman's docs/dev/panes.md, "Boards"; MB-343):
//! Linear's workflow states as columns in the projects syntax's seven categories, its issues as cards, and
//! its cycles as sprints, through core's map (tracker.toml, the one its Linear mirror reads). Linear is
//! asked with the vault's API key, over its GraphQL API.
//!
//! The board, as the pane sends it:
//!
//! ```text
//! {project: {key, title, leading_tool: "linear", note?},
//!  states: [{name, category}],                       // the team's, in Linear's order
//!  items: [{id, ref, level, kind, title, state, category, owner, parent, sprint, target, priority,
//!           rank, url}],                             // open issues, and those closed in the last 14 days
//!  groups: {sprints: [{id, name, start, end}], milestones: [{id, name, target}]}}
//! ```

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

pub const ISSUE_FIELDS: &str = "id identifier title priority dueDate url updatedAt sortOrder \
    state { id name type } assignee { name displayName } labels { nodes { name } } parent { identifier } \
    cycle { id number name startsAt endsAt } projectMilestone { id name targetDate }";

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

/// A team, as the board needs it: its states in Linear's order, and its recent cycles.
#[derive(Clone, Debug)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
    pub states: Vec<Value>,
    pub cycles: Vec<Value>,
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

    pub fn team(&self, key: &str) -> anyhow::Result<Team> {
        let data = self.gql(
            "query($key: String!) { teams(filter: {key: {eq: $key}}) { nodes { id key name \
               states { nodes { id name type position } } \
               cycles(first: 50, filter: {endsAt: {gt: \"-P60D\"}}) { nodes { id number name startsAt endsAt } } } } }",
            json!({"key": key}),
        )?;
        let t =
            data["teams"]["nodes"].get(0).cloned().ok_or_else(|| anyhow!("Linear has no team {key} for this key"))?;
        let mut states = t["states"]["nodes"].as_array().cloned().unwrap_or_default();
        states.sort_by(|a, b| a["position"].as_f64().unwrap_or(0.0).total_cmp(&b["position"].as_f64().unwrap_or(0.0)));
        let mut cycles = t["cycles"]["nodes"].as_array().cloned().unwrap_or_default();
        cycles
            .sort_by(|a, b| a["startsAt"].as_str().unwrap_or_default().cmp(b["startsAt"].as_str().unwrap_or_default()));
        Ok(Team {
            id: t["id"].as_str().unwrap_or_default().to_string(),
            key: t["key"].as_str().unwrap_or(key).to_string(),
            name: t["name"].as_str().unwrap_or(key).to_string(),
            states,
            cycles,
        })
    }

    /// The team's issues for its board: open ones, and those closed in the last 14 days. With `since`,
    /// only those updated after it, closed or not (what changed since the last look).
    pub fn issues(&self, team: &str, project: Option<&str>, since: Option<&str>) -> anyhow::Result<Vec<Value>> {
        let mut filter = json!({"team": {"key": {"eq": team}}});
        if let Some(project) = project.filter(|p| !p.is_empty()) {
            filter["project"] = json!({"name": {"eq": project}});
        }
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
}

// ------------------------------------------------------------------ the board

/// One issue as a board item.
pub fn item(map: &Tracker, issue: &Value) -> Value {
    let state = &issue["state"];
    let cycle = &issue["cycle"];
    let sprint = match cycle["name"].as_str().filter(|n| !n.is_empty()) {
        Some(name) => json!(name),
        None => cycle["number"].as_u64().map_or(Value::Null, |n| json!(format!("Cycle {n}"))),
    };
    let owner = issue["assignee"]["displayName"].as_str().or(issue["assignee"]["name"].as_str());
    json!({
        "id": issue["id"], "ref": issue["identifier"],
        "level": if map.is_epic(issue) { "epic" } else { "task" }, "kind": kind_of(issue),
        "title": issue["title"], "state": state["name"],
        "category": map.category(state["type"].as_str().unwrap_or_default(), state["name"].as_str().unwrap_or_default()),
        "owner": owner, "parent": issue["parent"]["identifier"], "sprint": sprint, "target": issue["dueDate"],
        "priority": priority_of(issue["priority"].as_i64().unwrap_or(0)),
        "rank": rank(issue["sortOrder"].as_f64().unwrap_or(0.0)), "url": issue["url"],
    })
}

/// The board for a team (and maybe one of its projects), from its issues by id, cut to `max` bytes: the
/// longest-closed issues go first, then the least recently changed, and the project says how many.
pub fn board(map: &Tracker, team: &Team, project: Option<&str>, issues: &HashMap<String, Value>, max: usize) -> Value {
    let title = match project.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p} · {}", team.name),
        None => team.name.clone(),
    };
    let states: Vec<Value> = team
        .states
        .iter()
        .map(|s| {
            let name = s["name"].as_str().unwrap_or_default();
            json!({"name": name, "category": map.category(s["type"].as_str().unwrap_or_default(), name)})
        })
        .collect();
    let sprints: Vec<Value> = team
        .cycles
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
    let mut items: Vec<Value> = ordered.into_iter().map(|i| item(map, i)).collect();
    let mut out = json!({
        "project": {"key": team.key, "title": title, "leading_tool": "linear"},
        "states": states, "items": [], "groups": {"sprints": sprints, "milestones": milestones},
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
    let left = issues.len() - kept.len();
    if left > 0 {
        let s = if left == 1 { "" } else { "s" };
        out["project"]["note"] = json!(format!("{left} older issue{s} left out: Linear has them"));
    }
    out["items"] = Value::Array(kept);
    out
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

/// A map of fields an edit may set: title, priority and target, as Linear's input. Anything else says why
/// it can't.
pub fn edit_input(fields: &Map<String, Value>) -> Result<Value, String> {
    let mut input = Map::new();
    for (field, value) in fields {
        match field.as_str() {
            "title" => {
                let title = value.as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or("a title can't be empty")?;
                input.insert("title".into(), json!(title));
            }
            "priority" => {
                let n = priority_number(value.as_str().unwrap_or_default())
                    .ok_or_else(|| format!("{value} isn't a priority: urgent, high, normal, low or none"))?;
                input.insert("priority".into(), json!(n));
            }
            "target" => {
                input.insert("dueDate".into(), value.clone());
            }
            other => {
                return Err(format!("{other} is changed in Linear itself: this board edits title, priority and target"))
            }
        }
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
               "assignee": {"name": "James Briggs", "displayName": "james"}, "labels": {"nodes": []},
               "parent": null, "cycle": null, "projectMilestone": null})
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
    fn the_board_has_the_teams_states_and_its_issues() {
        let map = Tracker::default();
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
        };
        let mut a = issue("1", ("started", "In Review"), 1.0, "2026-10-08T01:00:00.000Z");
        a["cycle"] = team.cycles[0].clone();
        a["projectMilestone"] = json!({"id": "m1", "name": "Beta", "targetDate": "2026-11-01"});
        let b = issue("2", ("completed", "Done"), 2.0, "2026-10-07T00:00:00.000Z");
        let issues = by_id(vec![a, b]);
        let board = board(&map, &team, Some("Voice"), &issues, CONTENT_MAX);
        assert_eq!(board["project"], json!({"key": "ALA", "title": "Voice · Spark", "leading_tool": "linear"}));
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
        assert_eq!(board["items"][1]["category"], "done");
        assert_eq!(
            board["groups"]["sprints"],
            json!([{"id": "c1", "name": "Cycle 41", "start": "2026-10-05", "end": "2026-10-19"}])
        );
        assert_eq!(board["groups"]["milestones"], json!([{"id": "m1", "name": "Beta", "target": "2026-11-01"}]));
        // cut to fit: the closed issue goes first
        let open_only = super::board(&map, &team, None, &by_id(vec![issues["1"].clone()]), CONTENT_MAX);
        let max = serde_json::to_vec(&open_only).unwrap().len() + 200 + 5;
        let small = super::board(&map, &team, None, &issues, max);
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
    fn edits_take_title_priority_and_target() {
        let fields = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            edit_input(&fields(json!({"title": " New ", "priority": "low", "target": "2026-10-20"}))).unwrap(),
            json!({"title": "New", "priority": 4, "dueDate": "2026-10-20"})
        );
        assert!(edit_input(&fields(json!({"owner": "x"}))).unwrap_err().contains("owner is changed in Linear itself"));
        assert!(edit_input(&fields(json!({"priority": "huge"}))).unwrap_err().contains("isn't a priority"));
        assert!(edit_input(&fields(json!({"title": ""}))).unwrap_err().contains("can't be empty"));
    }
}
