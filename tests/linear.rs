//! The extension against a stand-in node (pointman-extension's testing::StandIn), started as a node starts
//! it, with a stand-in Linear: a GraphQL server on a free local port that answers as Linear's API would,
//! for two teams (ALA and OPS, each with its own states) and a project across both, and keeps the changes
//! it's asked to make.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pointman_extension::testing::{PaneOptions, StandIn};
use serde_json::{json, Value};

const KEY: &str = "lin_api_0123456789abcdefghijklmnopqrstuvwxyzABCD";
/// A second Linear workspace's key: it sees `second`'s team and project, and nothing of the first's.
const KEY2: &str = "lin_api_ZYXWVUTSRQPONMLKJIHGFEDCBAzyxwvutsrqpo";
const WAIT: Duration = Duration::from_secs(10);

/// Linear, as far as the board needs it: the teams (people, labels), issues, issueUpdate, issueCreate
/// and comments.
struct Fake {
    issues: Vec<Value>,
    /// (issue id, body, author's display name), oldest first
    comments: Vec<(String, String, String)>,
    /// (query's first word, variables) for each call
    calls: Vec<(String, Value)>,
    /// issueUpdate refuses these ids with this message
    refuse: Option<(String, String)>,
    clock: u32,
}

fn state(name: &str, kind: &str, position: f64) -> Value {
    json!({"id": format!("st-{name}"), "name": name, "type": kind, "position": position})
}

fn states() -> Vec<Value> {
    vec![
        state("Todo", "unstarted", 1.0),
        state("In Progress", "started", 2.0),
        state("In Review", "started", 3.0),
        state("Done", "completed", 4.0),
        state("Canceled", "canceled", 5.0),
    ]
}

/// OPS's own workflow: a Todo and a Done of its own, and Doing, which ALA hasn't.
fn ops_states() -> Vec<Value> {
    ["Todo", "Doing", "Done"]
        .iter()
        .zip(["unstarted", "started", "completed"])
        .enumerate()
        .map(|(i, (name, kind))| json!({"id": format!("ops-{name}"), "name": name, "type": kind, "position": i as f64}))
        .collect()
}

fn person(id: &str, name: &str, handle: &str) -> Value {
    json!({"id": id, "name": name, "displayName": handle, "avatarUrl": format!("https://public.linear.app/{handle}.png"),
           "active": true})
}

fn people() -> Vec<Value> {
    vec![person("u1", "James Briggs", "james"), person("u2", "Ada Lovelace", "ada")]
}

fn label(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "color": "#eb5757", "isGroup": false})
}

/// The workspace's own labels, beside each team's.
fn workspace_labels() -> Value {
    json!({"nodes": [label("l-feat", "Feature")]})
}

fn team(id: &str, key: &str, name: &str, states: Vec<Value>) -> Value {
    json!({"id": id, "key": key, "name": name, "states": {"nodes": states},
           "cycles": {"nodes": [{"id": "cy-41", "number": 41, "name": null, "startsAt": "2026-10-05T00:00:00.000Z",
                                 "endsAt": "2026-10-19T00:00:00.000Z"}]},
           "members": {"nodes": people()}, "labels": {"nodes": [label(&format!("l-bug-{key}"), "Bug")]}})
}

fn teams() -> Vec<Value> {
    vec![team("team-1", "ALA", "Spark", states()), team("team-2", "OPS", "Operations", ops_states())]
}

fn issue(n: u32, state_name: &str, order: f64) -> Value {
    let st = states().into_iter().find(|s| s["name"] == state_name).unwrap();
    json!({"id": format!("lin_{n}"), "identifier": format!("ALA-{n}"), "title": format!("Issue {n}"), "priority": 3,
           "dueDate": null, "url": format!("https://linear.app/alai/issue/ALA-{n}"),
           "updatedAt": format!("2026-10-08T10:00:{n:02}.000Z"), "sortOrder": order,
           "team": {"id": "team-1"}, "state": {"id": st["id"], "name": st["name"], "type": st["type"]},
           "assignee": {"id": "u1", "name": "James Briggs", "displayName": "james"}, "labels": {"nodes": []},
           "estimate": null, "createdAt": "2026-10-01T00:00:00.000Z", "description": format!("Steps for {n}"),
           "parent": null, "cycle": null, "projectMilestone": null, "project": null})
}

/// An OPS issue, in the Spark Voice project.
fn ops_issue(n: u32, state_name: &str) -> Value {
    let st = ops_states().into_iter().find(|s| s["name"] == state_name).unwrap();
    let mut i = issue(n, "Todo", 1.0);
    i["identifier"] = json!(format!("OPS-{n}"));
    i["team"] = json!({"id": "team-2"});
    i["state"] = json!({"id": st["id"], "name": st["name"], "type": st["type"]});
    i["project"] = json!({"id": "proj-1"});
    i
}

impl Fake {
    fn answer(&mut self, body: &Value) -> (u16, Value) {
        let query = body["query"].as_str().unwrap_or_default();
        let vars = &body["variables"];
        let word = query.split(|c: char| !c.is_alphanumeric()).find(|w| {
            ["teams", "projects", "project", "issues", "issue", "issueUpdate", "issueCreate", "commentCreate"]
                .contains(w)
        });
        self.calls.push((word.unwrap_or("?").to_string(), vars.clone()));
        match word {
            Some("teams") if vars["key"].is_null() => (
                200,
                json!({"data": {"teams": {"nodes": [{"id": "team-1", "key": "ALA", "name": "Spark"},
                                                    {"id": "team-2", "key": "OPS", "name": "Operations"}]}}}),
            ),
            Some("teams") => {
                let found: Vec<Value> = teams().into_iter().filter(|t| t["key"] == vars["key"]).collect();
                (200, json!({"data": {"teams": {"nodes": found}, "issueLabels": workspace_labels()}}))
            }
            Some("projects") => {
                // the status filter is in the query's text: completed and canceled ones aren't listed
                let listed = |status: &str| !query.contains(&format!("\"{status}\""));
                let nodes: Vec<Value> = [("proj-1", "Spark Voice", "started"), ("proj-2", "Old Launch", "completed")]
                    .iter()
                    .filter(|(_, _, status)| listed(status))
                    .map(|(id, name, _)| json!({"id": id, "name": name}))
                    .collect();
                (
                    200,
                    json!({"data": {"projects": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}),
                )
            }
            Some("project") if vars["id"] == "proj-1" => (
                200,
                json!({"data": {"project": {"id": "proj-1", "name": "Spark Voice", "teams": {"nodes": teams()}},
                                "issueLabels": workspace_labels()}}),
            ),
            Some("project") => (
                200,
                json!({"errors": [{"message": "Entity not found: Project",
                                   "extensions": {"userPresentableMessage": "Could not find referenced Project."}}]}),
            ),
            Some("issues") => {
                let filter = &vars["filter"];
                let since = filter["updatedAt"]["gt"].as_str();
                let team = filter["team"]["key"]["eq"].as_str();
                let project = &filter["project"]["id"]["eq"];
                let team_of = |i: &Value| if i["team"]["id"] == "team-2" { "OPS" } else { "ALA" };
                let nodes: Vec<Value> = self
                    .issues
                    .iter()
                    .filter(|i| since.is_none_or(|s| i["updatedAt"].as_str().unwrap() > s))
                    .filter(|i| team.is_none_or(|t| team_of(i) == t))
                    .filter(|i| project.is_null() || i["project"]["id"] == *project)
                    .cloned()
                    .collect();
                (
                    200,
                    json!({"data": {"issues": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}),
                )
            }
            Some("issueUpdate") => {
                let id = vars["id"].as_str().unwrap_or_default().to_string();
                if let Some((refused, why)) = &self.refuse {
                    if *refused == id {
                        return (
                            400,
                            json!({"errors": [{"message": "Argument Validation Error",
                                                        "extensions": {"userPresentableMessage": why}}]}),
                        );
                    }
                }
                self.clock += 1;
                let clock = self.clock;
                if !self.issues.iter().any(|i| i["id"] == id.as_str()) {
                    return (200, json!({"errors": [{"message": "Entity not found"}]}));
                }
                let input = &vars["input"];
                let parent = input["parentId"].as_str().map(|p| {
                    let found = self.issues.iter().find(|i| i["id"] == p).unwrap();
                    json!({"id": found["id"], "identifier": found["identifier"]})
                });
                let it = self.issues.iter_mut().find(|i| i["id"] == id.as_str()).unwrap();
                apply(it, input);
                if let Some(parent) = parent {
                    it["parent"] = parent;
                }
                it["updatedAt"] = json!(format!("2026-10-08T11:00:{clock:02}.000Z"));
                (200, json!({"data": {"issueUpdate": {"success": true, "issue": it.clone()}}}))
            }
            Some("issueCreate") => {
                self.clock += 1;
                let input = &vars["input"];
                let n = 100 + self.clock;
                let mut it = issue(n, "Todo", 0.0);
                if input["teamId"] == "team-2" {
                    it["team"] = json!({"id": "team-2"});
                    it["identifier"] = json!(format!("OPS-{n}"));
                    it["state"] = json!({"id": "ops-Todo", "name": "Todo", "type": "unstarted"});
                }
                if let Some(project) = input["projectId"].as_str() {
                    it["project"] = json!({"id": project});
                }
                it["assignee"] = Value::Null;
                apply(&mut it, input);
                it["updatedAt"] = json!(format!("2026-10-08T11:00:{:02}.000Z", self.clock));
                self.issues.push(it.clone());
                (200, json!({"data": {"issueCreate": {"success": true, "issue": it}}}))
            }
            Some("commentCreate") => {
                let input = &vars["input"];
                let (on, body) = (input["issueId"].as_str().unwrap(), input["body"].as_str().unwrap());
                self.comments.push((on.into(), body.into(), "james".into()));
                (200, json!({"data": {"commentCreate": {"success": true}}}))
            }
            Some("issue") => {
                let id = vars["id"].as_str().unwrap_or_default();
                let Some(it) = self.issues.iter().find(|i| i["id"] == id) else {
                    return (200, json!({"errors": [{"message": "Entity not found: Issue"}]}));
                };
                let children: Vec<Value> = self
                    .issues
                    .iter()
                    .filter(|i| i["parent"]["id"] == id)
                    .map(|i| json!({"identifier": i["identifier"]}))
                    .collect();
                let comments: Vec<Value> = self
                    .comments
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.0 == id)
                    .map(|(n, c)| {
                        json!({"body": c.1, "createdAt": format!("2026-10-09T01:00:{n:02}.000Z"),
                               "user": {"name": "James Briggs", "displayName": c.2}})
                    })
                    .collect();
                (
                    200,
                    json!({"data": {"issue": {"id": id, "description": it["description"],
                                              "children": {"nodes": children}, "comments": {"nodes": comments}}}}),
                )
            }
            _ => (200, json!({"errors": [{"message": format!("unknown query {query}")}]})),
        }
    }
}

/// An issue input's fields onto the stand-in's issue, as Linear would keep them.
fn apply(it: &mut Value, input: &Value) {
    if let Some(st) = input["stateId"].as_str() {
        let s = states().into_iter().chain(ops_states()).find(|s| s["id"] == st).unwrap();
        it["state"] = json!({"id": s["id"], "name": s["name"], "type": s["type"]});
    }
    for (from, to) in [
        ("title", "title"),
        ("priority", "priority"),
        ("dueDate", "dueDate"),
        ("sortOrder", "sortOrder"),
        ("estimate", "estimate"),
        ("description", "description"),
    ] {
        if let Some(v) = input.get(from) {
            it[to] = v.clone();
        }
    }
    if let Some(who) = input.get("assigneeId") {
        it["assignee"] = people().into_iter().find(|p| p["id"] == *who).unwrap_or(Value::Null);
    }
    if let Some(ids) = input["labelIds"].as_array() {
        let all = [label("l-bug-ALA", "Bug"), label("l-bug-OPS", "Bug"), label("l-feat", "Feature")];
        let nodes: Vec<Value> = ids
            .iter()
            .filter_map(|id| all.iter().find(|l| l["id"] == *id))
            .map(|l| json!({"id": l["id"], "name": l["name"]}))
            .collect();
        it["labels"] = json!({"nodes": nodes});
    }
    if let Some(cycle) = input.get("cycleId") {
        it["cycle"] = if cycle.is_null() {
            Value::Null
        } else {
            json!({"id": cycle, "number": 41, "name": null, "startsAt": "2026-10-05T00:00:00.000Z",
                   "endsAt": "2026-10-19T00:00:00.000Z"})
        };
    }
    if input.get("parentId").is_some_and(Value::is_null) {
        it["parent"] = Value::Null;
    }
}

/// Starts the stand-in Linear; its url and what it holds.
fn linear() -> (String, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake {
        issues: vec![
            issue(1, "In Progress", 1.0),
            issue(2, "In Progress", 2.0),
            issue(3, "In Review", 1.0),
            issue(4, "Done", 1.0),
            ops_issue(6, "Todo"),
        ],
        comments: vec![],
        calls: vec![],
        refuse: None,
        clock: 0,
    }));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/graphql", listener.local_addr().unwrap());
    let f = fake.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let f = f.clone();
            std::thread::spawn(move || serve(stream, &f));
        }
    });
    (url, fake)
}

fn serve(stream: TcpStream, fake: &Mutex<Fake>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (mut length, mut authorized, mut second_key) = (0usize, false, false);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let lower = l.to_ascii_lowercase();
        if let Some(n) = lower.strip_prefix("content-length:") {
            length = n.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            authorized = l["authorization:".len()..].trim() == KEY;
            second_key = l["authorization:".len()..].trim() == KEY2;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    let (code, answer) = if authorized {
        fake.lock().unwrap().answer(&serde_json::from_slice(&body).unwrap_or(Value::Null))
    } else if second_key {
        second(&serde_json::from_slice(&body).unwrap_or(Value::Null))
    } else {
        (401, json!({"message": "Authentication required"}))
    };
    let text = answer.to_string();
    let mut out = stream;
    let _ = write!(
        out,
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// A second Linear workspace, as its own key (KEY2) sees it: one team (SPA, Spacesheep), one project (GTM
/// Launch) and one issue, and nothing of the first workspace's.
fn second(body: &Value) -> (u16, Value) {
    let query = body["query"].as_str().unwrap_or_default();
    let vars = &body["variables"];
    let word =
        query.split(|c: char| !c.is_alphanumeric()).find(|w| ["teams", "projects", "project", "issues"].contains(w));
    let spa = team("team-9", "SPA", "Spacesheep", states());
    match word {
        Some("teams") if vars["key"].is_null() => {
            (200, json!({"data": {"teams": {"nodes": [{"id": "team-9", "key": "SPA", "name": "Spacesheep"}]}}}))
        }
        Some("teams") => {
            let found: Vec<Value> = [spa].into_iter().filter(|t| t["key"] == vars["key"]).collect();
            (200, json!({"data": {"teams": {"nodes": found}, "issueLabels": workspace_labels()}}))
        }
        Some("projects") => (
            200,
            json!({"data": {"projects": {"nodes": [{"id": "proj-9", "name": "GTM Launch"}],
                                         "pageInfo": {"hasNextPage": false, "endCursor": null}}}}),
        ),
        Some("project") if vars["id"] == "proj-9" => (
            200,
            json!({"data": {"project": {"id": "proj-9", "name": "GTM Launch", "teams": {"nodes": [spa]}},
                            "issueLabels": workspace_labels()}}),
        ),
        Some("issues") => {
            let filter = &vars["filter"];
            let mut spa_1 = issue(1, "Todo", 1.0);
            spa_1["id"] = json!("spa_1");
            spa_1["identifier"] = json!("SPA-1");
            spa_1["team"] = json!({"id": "team-9"});
            let mine = filter["team"]["key"]["eq"] == "SPA" || filter["project"]["id"]["eq"] == "proj-9";
            let nodes = if mine { vec![spa_1] } else { vec![] };
            (200, json!({"data": {"issues": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}))
        }
        _ => (200, json!({"errors": [{"message": "Entity not found"}]})),
    }
}

/// The cards a pane.edit puts, as the extension sent them.
fn put_items(edit: &Value) -> Vec<Value> {
    let edits = edit["edits"].as_array().unwrap();
    edits
        .iter()
        .flat_map(|e| e["putItems"]["items"].as_array().cloned().unwrap_or_default())
        .map(|i| i["board"].clone())
        .collect()
}

fn node(url: &str, key: &str, settings: Value) -> StandIn {
    let mut s = json!({"api_url": url, "refresh_seconds": 1});
    s.as_object_mut().unwrap().extend(settings.as_object().unwrap().clone());
    StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(s)
        .secret("linear.work", format!("LINEAR_API_KEY={key}\n"))
        .start()
        .unwrap()
}

#[test]
fn a_teams_board_opens_with_its_states_and_issues() {
    let (url, _fake) = linear();
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    assert_eq!(board.format_word(), Some("board"));
    assert_eq!(board.title().as_deref(), Some("Spark · Linear"));
    let content = board.board();
    assert_eq!(content["project"], json!({"key": "ALA", "title": "Spark", "leading_tool": "linear"}));
    let states: Vec<(String, String)> = content["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap().into(), s["category"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        states,
        [
            ("Todo".into(), "todo".into()),
            ("In Progress".into(), "active".into()),
            ("In Review".into(), "review".into()),
            ("Done".into(), "done".into()),
            ("Canceled".into(), "dropped".into())
        ]
    );
    let one = board.item("lin_1").unwrap();
    assert_eq!(
        (one["ref"].as_str(), one["state"].as_str(), one["category"].as_str()),
        (Some("ALA-1"), Some("In Progress"), Some("active"))
    );
    assert_eq!(
        (one["owner"].as_str(), one["priority"].as_str(), one["level"].as_str()),
        (Some("james"), Some("normal"), Some("task"))
    );
    assert!(board.item("lin_1").unwrap()["rank"].as_str() < board.item("lin_2").unwrap()["rank"].as_str());
    assert_eq!(content["groups"]["sprints"][0]["name"], "Cycle 41");
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn moves_ranks_and_edits_go_to_linear() {
    let (url, fake) = linear();
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    board.input_json(json!({"kind": "move", "item": "lin_1", "to": "In Review"})).unwrap();
    assert_eq!(board.item("lin_1").unwrap()["category"], "review");
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1, json!({"id": "lin_1", "input": {"stateId": "st-In Review"}}));
    // only what changed went out
    let (method, last) = board.frames().pop().unwrap();
    assert_eq!(method, "pane.edit");
    let put = put_items(&last);
    assert_eq!(put.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["lin_1"]);
    assert_eq!((put[0]["state"].as_str(), put[0]["category"].as_str()), (Some("In Review"), Some("review")));

    // just before ALA-3 in In Review (sortOrder 1): between nothing and 1
    board.input_json(json!({"kind": "rank", "item": "lin_1", "before": "lin_3"})).unwrap();
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1["input"], json!({"sortOrder": 0.0}));
    assert!(board.item("lin_1").unwrap()["rank"].as_str() < board.item("lin_3").unwrap()["rank"].as_str());

    board
        .input_json(json!({"kind": "edit", "item": "lin_2", "fields": {"title": "Renamed", "priority": "urgent"}}))
        .unwrap();
    assert_eq!(
        (board.item("lin_2").unwrap()["title"].as_str(), board.item("lin_2").unwrap()["priority"].as_str()),
        (Some("Renamed"), Some("urgent"))
    );
    board.input_json(json!({"kind": "open", "item": "lin_2"})).unwrap();
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn what_linear_refuses_snaps_back_with_its_reason() {
    let (url, fake) = linear();
    fake.lock().unwrap().refuse = Some(("lin_2".into(), "Parent issues close when their sub-issues do".into()));
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    let refused = board.input_json(json!({"kind": "move", "item": "lin_2", "to": "Done"})).unwrap_err();
    assert_eq!(refused.message, "Linear: Parent issues close when their sub-issues do");
    assert_eq!(board.item("lin_2").unwrap()["state"], "In Progress");
    let no_state = board.input_json(json!({"kind": "move", "item": "lin_1", "to": "Shipped"})).unwrap_err();
    assert_eq!(no_state.message, "Shipped isn't one of ALA's states");
    let owner = board.input_json(json!({"kind": "edit", "item": "lin_1", "fields": {"owner": "loupe"}})).unwrap_err();
    assert_eq!(owner.message, "loupe isn't one of ALA's people");
    let gone = board.input_json(json!({"kind": "move", "item": "lin_99", "to": "Done"})).unwrap_err();
    assert_eq!(gone.message, "lin_99 isn't on this board");
    assert!(board.frames().is_empty(), "{:?}", board.frames());
}

#[test]
fn linears_changes_reach_an_open_board() {
    let (url, fake) = linear();
    let node = node(&url, KEY, json!({}));
    let options = PaneOptions::default();
    let board = node.open_pane_with("board", json!({"team": "ALA"}), options).unwrap();
    {
        let mut f = fake.lock().unwrap();
        f.issues[1]["title"] = json!("Changed in Linear");
        f.issues[1]["updatedAt"] = json!("2026-10-08T12:00:00.000Z");
        let mut new = issue(5, "Todo", 1.0);
        new["updatedAt"] = json!("2026-10-08T12:00:01.000Z");
        f.issues.push(new);
    }
    board
        .wait(WAIT, |b| b.item("lin_5").is_some() && b.item("lin_2").unwrap()["title"] == "Changed in Linear")
        .unwrap();
    // only what changed: ALA-2's title, and ALA-5 added
    let put: Vec<Value> =
        board.frames().iter().filter(|(m, _)| m == "pane.edit").flat_map(|(_, p)| put_items(p)).collect();
    assert!(put.iter().any(|i| i["id"] == "lin_2" && i["title"] == "Changed in Linear"), "{put:?}");
    assert!(put.iter().any(|i| i["ref"] == "ALA-5"), "{put:?}");
    assert!(put.iter().all(|i| i["id"] == "lin_2" || i["id"] == "lin_5"), "{put:?}");
    assert!(board.frames().iter().all(|(m, _)| m == "pane.edit"), "{:?}", board.frames());
    // the next look asks only for what changed since
    let looks: Vec<Value> =
        fake.lock().unwrap().calls.iter().filter(|c| c.0 == "issues").map(|c| c.1.clone()).collect();
    assert!(
        looks.len() >= 2 && looks[0]["filter"]["or"].is_array() && looks[1]["filter"]["updatedAt"]["gt"].is_string()
    );
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn a_board_that_cant_open_says_why() {
    let (url, _fake) = linear();
    let bad = node(&url, "lin_api_ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ", json!({}));
    let e = bad.open_pane("board", json!({"team": "ALA"})).err().unwrap();
    assert_eq!(e.message, "Linear refused the API key (401): add a working one with /linear add");
    let node = node(&url, KEY, json!({}));
    let e = node.open_pane("board", Value::Null).err().unwrap();
    assert!(e.message.starts_with("which Linear team or project?"), "{e}");
    let e = node.open_pane("board", json!({"team": "NOPE"})).err().unwrap();
    assert_eq!(e.message, "Linear has no team NOPE for this key");
    let e = node.open_pane("board", json!({"project": "proj-9"})).err().unwrap();
    assert_eq!(e.message, "Linear: Could not find referenced Project.");
    let none = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(json!({"api_url": url}))
        .start()
        .unwrap();
    let e = none.open_pane("board", json!({"team": "ALA"})).err().unwrap();
    assert_eq!(e.message, "no Linear API key in the vault: add one with /linear add");
}

#[test]
fn linear_boards_lists_what_a_project_can_connect() {
    let (url, _fake) = linear();
    let node = node(&url, KEY, json!({}));
    let done = node.run("linear.boards", json!({})).unwrap();
    // the teams, then the projects that aren't completed or canceled
    assert_eq!(
        done.result["targets"],
        json!([{"target": {"team": "OPS"}, "name": "Operations", "key": "OPS", "kind": "team"},
               {"target": {"team": "ALA"}, "name": "Spark", "key": "ALA", "kind": "team"},
               {"target": {"project": "proj-1"}, "name": "Spark Voice", "key": "SV", "kind": "project"}])
    );
    assert_eq!(done.result["text"], "2 Linear teams: Operations (OPS), Spark (ALA). 1 Linear project: Spark Voice.");
    // a target it lists is what the board opens by
    let team = done.result["targets"][1]["target"].clone();
    assert_eq!(node.open_pane("board", team).unwrap().title().as_deref(), Some("Spark · Linear"));
    let project = done.result["targets"][2]["target"].clone();
    assert_eq!(node.open_pane("board", project).unwrap().title().as_deref(), Some("Spark Voice · Linear"));
    let none = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(json!({"api_url": url}))
        .start()
        .unwrap();
    let e = none.run("linear.boards", json!({})).unwrap_err();
    assert_eq!(e.message, "no Linear API key in the vault: add one with /linear add");
}

/// A key per Linear workspace in the vault (linear.alai, linear.spacesheep): linear.boards lists every key's
/// teams and projects, and a board opens with whichever key sees it, its first by label or not.
#[test]
fn every_key_in_the_vault_has_its_boards() {
    let (url, _fake) = linear();
    let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(json!({"api_url": url, "refresh_seconds": 1}))
        .secret("linear.alai", format!("LINEAR_API_KEY={KEY}\n"))
        .secret("linear.spacesheep", format!("LINEAR_API_KEY={KEY2}\n"))
        .start()
        .unwrap();
    let done = node.run("linear.boards", json!({})).unwrap();
    let names: Vec<&str> =
        done.result["targets"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Operations", "Spark", "Spark Voice", "Spacesheep", "GTM Launch"]);
    assert_eq!(
        done.result["text"],
        "linear.alai: 2 Linear teams: Operations (OPS), Spark (ALA). 1 Linear project: Spark Voice.\n\
         linear.spacesheep: 1 Linear team: Spacesheep (SPA). 1 Linear project: GTM Launch."
    );
    // the second key's team and project open, with its issues
    let spa = node.open_pane("board", json!({"team": "SPA"})).unwrap();
    assert_eq!(spa.title().as_deref(), Some("Spacesheep · Linear"));
    assert!(spa.board().to_string().contains("SPA-1"), "{}", spa.board());
    let gtm = node.open_pane("board", json!({"project": "proj-9"})).unwrap();
    assert_eq!(gtm.title().as_deref(), Some("GTM Launch · Linear"));
    // and the first's still do
    assert_eq!(node.open_pane("board", json!({"team": "ALA"})).unwrap().title().as_deref(), Some("Spark · Linear"));
    // one neither key sees says what each said
    let e = node.open_pane("board", json!({"team": "NOPE"})).unwrap_err();
    assert!(e.message.contains("linear.alai: ") && e.message.contains("linear.spacesheep: "), "{e}");

    // a board opened straight away, with no linear.boards before it, finds its key too
    let fresh = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(json!({"api_url": url}))
        .secret("linear.alai", format!("LINEAR_API_KEY={KEY}\n"))
        .secret("linear.spacesheep", format!("LINEAR_API_KEY={KEY2}\n"))
        .start()
        .unwrap();
    assert_eq!(
        fresh.open_pane("board", json!({"team": "SPA"})).unwrap().title().as_deref(),
        Some("Spacesheep · Linear")
    );
}

#[test]
fn linear_add_posted_says_where_keys_go() {
    let (url, _fake) = linear();
    let node = node(&url, KEY, json!({}));
    let done = node.run("linear.add", json!({"name": "work"})).unwrap();
    assert!(done.result["text"].as_str().unwrap().contains("`/linear add`"));
}

#[test]
fn a_projects_board_has_its_issues_from_every_team() {
    let (url, fake) = linear();
    fake.lock().unwrap().issues[0]["project"] = json!({"id": "proj-1"}); // ALA-1 is in Spark Voice too
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"project": "proj-1"})).unwrap();
    let content = board.board();
    assert_eq!(content["project"], json!({"key": "SV", "title": "Spark Voice", "leading_tool": "linear"}));
    // ALA's states in its order, then OPS's own (Doing); a name both have is one column
    let states: Vec<&str> = content["states"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(states, ["Todo", "In Progress", "In Review", "Done", "Canceled", "Doing"]);
    let mut refs: Vec<&str> = content["items"].as_array().unwrap().iter().map(|i| i["ref"].as_str().unwrap()).collect();
    refs.sort();
    assert_eq!(refs, ["ALA-1", "OPS-6"]);
    // a move takes the issue's own team's state by that name
    board.input_json(json!({"kind": "move", "item": "lin_6", "to": "Doing"})).unwrap();
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1, json!({"id": "lin_6", "input": {"stateId": "ops-Doing"}}));
    assert_eq!(board.item("lin_6").unwrap()["category"], "active");
    board.input_json(json!({"kind": "move", "item": "lin_6", "to": "Done"})).unwrap();
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1["input"], json!({"stateId": "ops-Done"}));
    let refused = board.input_json(json!({"kind": "move", "item": "lin_1", "to": "Doing"})).unwrap_err();
    assert_eq!(refused.message, "Doing isn't one of ALA's states");
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn an_opened_card_gets_its_detail() {
    let (url, fake) = linear();
    fake.lock().unwrap().issues[1]["parent"] = json!({"id": "lin_1", "identifier": "ALA-1"});
    fake.lock().unwrap().comments.push(("lin_1".into(), "First!".into(), "ada".into()));
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    assert!(board.item("lin_1").unwrap().get("body").is_none());
    board.input_json(json!({"kind": "open", "item": "lin_1"})).unwrap();
    let card = board.item("lin_1").unwrap();
    assert_eq!(card["body"], "Steps for 1");
    assert_eq!(card["children"], json!(["ALA-2"]));
    assert_eq!(card["comments"], json!([{"author": "ada", "body": "First!", "at": "2026-10-09T01:00:00.000Z"}]));
    // only the detail went out
    let (method, last) = board.frames().pop().unwrap();
    assert_eq!(method, "pane.edit");
    let put = put_items(&last);
    assert_eq!(put.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["lin_1"]);
    assert_eq!(put[0]["body"], "Steps for 1");
    // the board's people and labels, for the card's pickers
    let groups = &board.board()["groups"];
    let handles: Vec<&str> =
        groups["people"].as_array().unwrap().iter().map(|p| p["handle"].as_str().unwrap()).collect();
    assert_eq!(handles, ["ada", "james"]);
    assert_eq!(groups["people"][1]["avatar"], "https://public.linear.app/james.png");
    let labels: Vec<&str> = groups["labels"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(labels, ["Bug", "Feature"]);
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn every_field_of_a_card_goes_to_linear() {
    let (url, fake) = linear();
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    board.input_json(json!({"kind": "open", "item": "lin_1"})).unwrap();
    board
        .input_json(json!({"kind": "edit", "item": "lin_1", "fields": {
            "owner": "ada", "labels": ["Bug", "Feature"], "estimate": 3, "sprint": "Cycle 41", "parent": "ALA-3",
            "body": "Now with steps", "target": "2026-10-20", "priority": "urgent"}}))
        .unwrap();
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(
        update.1["input"],
        json!({"assigneeId": "u2", "labelIds": ["l-bug-ALA", "l-feat"], "estimate": 3, "cycleId": "cy-41",
               "parentId": "lin_3", "description": "Now with steps", "dueDate": "2026-10-20", "priority": 1})
    );
    let card = board.item("lin_1").unwrap();
    assert_eq!((card["owner"].as_str(), &card["labels"]), (Some("ada"), &json!(["Bug", "Feature"])));
    assert_eq!(
        (&card["estimate"], card["sprint"].as_str(), card["parent"].as_str()),
        (&json!(3), Some("Cycle 41"), Some("ALA-3"))
    );
    assert_eq!((card["body"].as_str(), card["priority"].as_str()), (Some("Now with steps"), Some("urgent")));
    // taken off again
    board
        .input_json(json!({"kind": "edit", "item": "lin_1", "fields": {"owner": null, "sprint": null, "parent": null}}))
        .unwrap();
    let card = board.item("lin_1").unwrap();
    assert_eq!((&card["owner"], &card["sprint"], &card["parent"]), (&Value::Null, &Value::Null, &Value::Null));
    let nobody = board.input_json(json!({"kind": "edit", "item": "lin_1", "fields": {"owner": "nobody"}})).unwrap_err();
    assert_eq!(nobody.message, "nobody isn't one of ALA's people");
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn a_new_card_and_a_comment_go_to_linear() {
    let (url, fake) = linear();
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    board
        .input_json(json!({"kind": "create", "fields": {"title": "Fresh", "state": "In Review", "owner": "ada"}}))
        .unwrap();
    let made = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueCreate").cloned().unwrap();
    assert_eq!(
        made.1["input"],
        json!({"title": "Fresh", "stateId": "st-In Review", "assigneeId": "u2", "teamId": "team-1"})
    );
    let card = board.item("lin_101").unwrap();
    assert_eq!(
        (card["ref"].as_str(), card["state"].as_str(), card["owner"].as_str()),
        (Some("ALA-101"), Some("In Review"), Some("ada"))
    );
    let untitled = board.input_json(json!({"kind": "create", "fields": {"state": "Todo"}})).unwrap_err();
    assert_eq!(untitled.message, "a new card needs a title");

    board.input_json(json!({"kind": "comment", "item": "lin_2", "body": "Looks good"})).unwrap();
    let said = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "commentCreate").cloned().unwrap();
    assert_eq!(said.1["input"], json!({"issueId": "lin_2", "body": "Looks good"}));
    assert_eq!(board.item("lin_2").unwrap()["comments"][0]["body"], "Looks good");
    let empty = board.input_json(json!({"kind": "comment", "item": "lin_2", "body": " "})).unwrap_err();
    assert_eq!(empty.message, "a comment needs words");

    // on a project's board, a new card goes in the project, on the team it names
    let project = node.open_pane("board", json!({"project": "proj-1"})).unwrap();
    project.input_json(json!({"kind": "create", "fields": {"title": "Ops work", "team": "OPS"}})).unwrap();
    let made = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueCreate").cloned().unwrap();
    assert_eq!(made.1["input"], json!({"title": "Ops work", "teamId": "team-2", "projectId": "proj-1"}));
    assert_eq!(project.item("lin_102").unwrap()["ref"], "OPS-102");
    assert!(board.problems().is_empty() && project.problems().is_empty());
}
