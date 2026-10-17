# linear

A Pointman extension that opens a Linear team's or project's board in the apps. Pointman draws it with
its own project board: the same columns, cards, filters and drag. Linear stays the source of truth, and
the board has full control of it. A move, a rank, an edit of any field, a new card or a comment goes to
Linear, and the card settles where Linear puts it. If Linear says no, the card snaps back with Linear's
reason.

Linear is asked from the node, with the vault's API key, so the apps never hold it.

```
pointman add linear
```

## Getting started

1. **Add a key:** `/linear add` in any thread opens a form for a personal API key from linear.app
   (Settings › Security & access › New API key). It's sealed into the vault as `linear.<label>` for every
   node, and never posted. Pointman asks Linear as the key's owner.
2. **Connect a board:** in a project's pane, **Add a project board** lists the Linear teams and open
   projects the key sees (`/linear boards` lists them in a thread). Pick one, and the project opens its
   board.

## The board

A board opens by a team, `{team: "ALA"}`, or by a Linear project, `{project: "<id>"}`. A project's issues
can be in several teams: its columns are each team's states by name (the first team's order first), and a
move sets the issue's own team's state of that name.

- **Columns:** the team's workflow states in Linear's order. Each falls in one of Pointman's seven
  categories (triage, backlog, todo, active, review, done, dropped): In Review is `review`, say, and a
  started state named for a hold is `active`, held.
- **Cards:** the team's or project's open issues, and those closed in the last 14 days. Each card has its
  identifier, epic or task (the EPIC label or an "[EPIC] …" title), kind (by the Bug and Feature labels),
  state, owner (the assignee), parent, cycle, due date, priority, labels, estimate, Linear's order, and a
  link to the issue.
- **An opened card** also has its description, its comments (oldest first) and its sub-issues.
- **Sprints and milestones:** the teams' recent cycles are its sprints, and the issues' project milestones
  its milestones. Its people are who a card can be given to, and its labels the teams' and the
  workspace's.
- **Size:** a board too big to send at once leaves out the longest-closed issues first, then the least
  recently changed, and the board says how many.
- **Keeping current:** every 30 seconds (`refresh_seconds`), an open board asks Linear what changed since
  its last look, and everything (teams' people, labels and sprints too) every ten minutes. Only what
  changed goes to the apps.

### What the board can do

| Gesture | What goes to Linear |
|---|---|
| `move {item, to}` | the issue's state |
| `rank {item, before \| after}` | its order (`sortOrder`) between its neighbours in that column |
| `edit {item, fields}` | `title`, `body`, `state`, `owner` (a handle, or null), `priority` (urgent, high, normal or medium, low, none), `target` (YYYY-MM-DD, or null), `estimate`, `labels` (names), `sprint` (an id or name, or null) and `parent` (a ref, or null), each checked against the board first |
| `open {item}` | nothing: it adds the card's description, comments and sub-issues |
| `comment {item, body}` | a comment |
| `create {fields}` | a new issue with `title` and any of `edit`'s fields, on `team` (a key) or the board's first team, and in the Linear project when the board is one |

## Commands

| Kind | Slash | What |
|---|---|---|
| `linear.add` | `/linear add` | Add a Linear API key for every node |
| `linear.boards` | `/linear boards` | The Linear teams and open projects whose boards can be connected |

`linear.boards` answers with each one's target, name, suggested project key and kind:

```
{targets: [{target: {team: "ALA"}, name: "Spark", key: "ALA", kind: "team"},
           {target: {project: "<id>"}, name: "Spark Voice", key: "SV", kind: "project"}],
 text: "1 Linear team: Spark (ALA). 1 Linear project: Spark Voice."}
```

The key is a team's own, or a project name's initials (a one-word name's first three letters); none when
the name gives no key.

## Settings

| Setting | Default | |
|---|---|---|
| `refresh_seconds` | 30 | How often an open board asks Linear what changed |
| `api_url` | `https://api.linear.app/graphql` | Linear's GraphQL endpoint |

## Building and testing

```
cargo test
```

The tests run it against a stand-in node and a stand-in Linear: a GraphQL server on a free local port that
answers as Linear's API would. To release, bump `version` in extension.toml and Cargo.toml and push a tag
for it (`git tag v0.4.3 && git push origin v0.4.3`). The release workflow builds each platform (macOS arm64
and x86_64, Linux x86_64 and arm64) and attaches it, and the marketplace lists it within 30 minutes.

## Licence

MIT. See `LICENSE`.
