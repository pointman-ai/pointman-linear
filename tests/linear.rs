//! The extension against a stand-in node (pointman-extension's testing::StandIn), started as a node starts
//! it, with a stand-in Linear: a GraphQL server on a free local port that answers as Linear's API would,
//! for one team (ALA), and keeps the changes it's asked to make.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pointman_extension::testing::{PaneOptions, StandIn};
use serde_json::{json, Value};

const KEY: &str = "lin_api_0123456789abcdefghijklmnopqrstuvwxyzABCD";
const WAIT: Duration = Duration::from_secs(10);

/// Linear, as far as the board needs it: the team, its issues, and issueUpdate.
struct Fake {
    issues: Vec<Value>,
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

fn issue(n: u32, state_name: &str, order: f64) -> Value {
    let st = states().into_iter().find(|s| s["name"] == state_name).unwrap();
    json!({"id": format!("lin_{n}"), "identifier": format!("ALA-{n}"), "title": format!("Issue {n}"), "priority": 3,
           "dueDate": null, "url": format!("https://linear.app/alai/issue/ALA-{n}"),
           "updatedAt": format!("2026-10-08T10:00:{n:02}.000Z"), "sortOrder": order,
           "state": {"id": st["id"], "name": st["name"], "type": st["type"]},
           "assignee": {"name": "James Briggs", "displayName": "james"}, "labels": {"nodes": []},
           "parent": null, "cycle": null, "projectMilestone": null})
}

impl Fake {
    fn answer(&mut self, body: &Value) -> (u16, Value) {
        let query = body["query"].as_str().unwrap_or_default();
        let vars = &body["variables"];
        let word = query.split(|c: char| !c.is_alphanumeric()).find(|w| ["teams", "issues", "issueUpdate"].contains(w));
        self.calls.push((word.unwrap_or("?").to_string(), vars.clone()));
        match word {
            Some("teams") => {
                if vars["key"] != "ALA" {
                    return (200, json!({"data": {"teams": {"nodes": []}}}));
                }
                (
                    200,
                    json!({"data": {"teams": {"nodes": [{"id": "team-1", "key": "ALA", "name": "Spark",
                    "states": {"nodes": states()},
                    "cycles": {"nodes": [{"id": "cy-41", "number": 41, "name": null, "startsAt": "2026-10-05T00:00:00.000Z",
                                          "endsAt": "2026-10-19T00:00:00.000Z"}]}}]}}}),
                )
            }
            Some("issues") => {
                let since = vars["filter"]["updatedAt"]["gt"].as_str();
                let nodes: Vec<Value> = self
                    .issues
                    .iter()
                    .filter(|i| since.is_none_or(|s| i["updatedAt"].as_str().unwrap() > s))
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
                let Some(it) = self.issues.iter_mut().find(|i| i["id"] == id.as_str()) else {
                    return (200, json!({"errors": [{"message": "Entity not found"}]}));
                };
                let input = &vars["input"];
                if let Some(st) = input["stateId"].as_str() {
                    let s = states().into_iter().find(|s| s["id"] == st).unwrap();
                    it["state"] = json!({"id": s["id"], "name": s["name"], "type": s["type"]});
                }
                for (from, to) in
                    [("title", "title"), ("priority", "priority"), ("dueDate", "dueDate"), ("sortOrder", "sortOrder")]
                {
                    if let Some(v) = input.get(from) {
                        it[to] = v.clone();
                    }
                }
                it["updatedAt"] = json!(format!("2026-10-08T11:00:{clock:02}.000Z"));
                (200, json!({"data": {"issueUpdate": {"success": true, "issue": it.clone()}}}))
            }
            _ => (200, json!({"errors": [{"message": format!("unknown query {query}")}]})),
        }
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
        ],
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
    let (mut length, mut authorized) = (0usize, false);
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
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    let (code, answer) = if authorized {
        fake.lock().unwrap().answer(&serde_json::from_slice(&body).unwrap_or(Value::Null))
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
    assert_eq!(board.format().as_deref(), Some("board"));
    assert_eq!(board.title().as_deref(), Some("Spark · Linear"));
    let content = board.content();
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
    let node = node(&url, KEY, json!({"team": "ALA"}));
    let board = node.open_pane("board", Value::Null).unwrap(); // the settings' team: opened from the menu
    board.input(json!({"kind": "move", "item": "lin_1", "to": "In Review"})).unwrap();
    assert_eq!(board.item("lin_1").unwrap()["category"], "review");
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1, json!({"id": "lin_1", "input": {"stateId": "st-In Review"}}));
    // only what changed went out
    let (method, last) = board.frames().pop().unwrap();
    assert_eq!(method, "pane.patch");
    let paths: Vec<&str> = last["patch"].as_array().unwrap().iter().map(|o| o["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/i/lin_1/category", "/i/lin_1/state"]);

    // just before ALA-3 in In Review (sortOrder 1): between nothing and 1
    board.input(json!({"kind": "rank", "item": "lin_1", "before": "lin_3"})).unwrap();
    let update = fake.lock().unwrap().calls.iter().rfind(|c| c.0 == "issueUpdate").cloned().unwrap();
    assert_eq!(update.1["input"], json!({"sortOrder": 0.0}));
    assert!(board.item("lin_1").unwrap()["rank"].as_str() < board.item("lin_3").unwrap()["rank"].as_str());

    board
        .input(json!({"kind": "edit", "item": "lin_2", "fields": {"title": "Renamed", "priority": "urgent"}}))
        .unwrap();
    assert_eq!(
        (board.item("lin_2").unwrap()["title"].as_str(), board.item("lin_2").unwrap()["priority"].as_str()),
        (Some("Renamed"), Some("urgent"))
    );
    board.input(json!({"kind": "open", "item": "lin_2"})).unwrap();
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn what_linear_refuses_snaps_back_with_its_reason() {
    let (url, fake) = linear();
    fake.lock().unwrap().refuse = Some(("lin_2".into(), "Parent issues close when their sub-issues do".into()));
    let node = node(&url, KEY, json!({}));
    let board = node.open_pane("board", json!({"team": "ALA"})).unwrap();
    let refused = board.input(json!({"kind": "move", "item": "lin_2", "to": "Done"})).unwrap_err();
    assert_eq!(refused.message, "Linear: Parent issues close when their sub-issues do");
    assert_eq!(board.item("lin_2").unwrap()["state"], "In Progress");
    let no_state = board.input(json!({"kind": "move", "item": "lin_1", "to": "Shipped"})).unwrap_err();
    assert_eq!(no_state.message, "Shipped isn't one of ALA's states");
    let owner = board.input(json!({"kind": "edit", "item": "lin_1", "fields": {"owner": "loupe"}})).unwrap_err();
    assert!(owner.message.starts_with("owner is changed in Linear itself"), "{owner}");
    let gone = board.input(json!({"kind": "move", "item": "lin_99", "to": "Done"})).unwrap_err();
    assert_eq!(gone.message, "lin_99 isn't on this board");
    assert!(board.frames().is_empty(), "{:?}", board.frames());
}

#[test]
fn linears_changes_reach_an_open_board() {
    let (url, fake) = linear();
    let node = node(&url, KEY, json!({}));
    let options = PaneOptions { context: json!({}), ..Default::default() };
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
    let patches: Vec<Value> = board.frames().into_iter().filter(|(m, _)| m == "pane.patch").map(|(_, p)| p).collect();
    let ops: Vec<Value> = patches.iter().flat_map(|p| p["patch"].as_array().unwrap().clone()).collect();
    assert!(ops.iter().any(|o| o["path"] == "/i/lin_2/title"), "{ops:?}");
    assert!(ops.iter().any(|o| o["path"] == "/board/items/-" && o["value"]["ref"] == "ALA-5"), "{ops:?}");
    assert!(board.frames().iter().all(|(m, _)| m == "pane.patch"));
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
    assert!(e.message.starts_with("which Linear team?"), "{e}");
    let e = node.open_pane("board", json!({"team": "NOPE"})).err().unwrap();
    assert_eq!(e.message, "Linear has no team NOPE for this key");
    let none = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_linear"))
        .settings(json!({"api_url": url}))
        .start()
        .unwrap();
    let e = none.open_pane("board", json!({"team": "ALA"})).err().unwrap();
    assert_eq!(e.message, "no Linear API key in the vault: add one with /linear add");
}

#[test]
fn linear_add_posted_says_where_keys_go() {
    let (url, _fake) = linear();
    let node = node(&url, KEY, json!({}));
    let done = node.run("linear.add", json!({"name": "work"})).unwrap();
    assert!(done.result["text"].as_str().unwrap().contains("`/linear add`"));
}
