# linear

Linear on Pointman's machines (MB-343, Pointman's docs/dev/panes.md): a Linear team's or project's board as a pane,
drawn by the apps' own board, with the same columns, cards, filters and drag as Pointman's projects. Linear's
workflow states become columns in the projects syntax's seven categories, through the same map core's Linear
mirror reads (tracker.toml, a copy built in). A move, a rank or an edit goes to Linear, and the card settles
where Linear puts it. If Linear says no, the card snaps back with Linear's reason.

Linear is asked with the vault's API key (`/linear add`), from the node, so the apps never hold it.

```
pointman add linear
```

## The board pane

`[[panes]] id = "board"`, `format = "board"`. Opened with a team, `{team: "ALA"}`, or a Linear project,
`{project: "<id>"}`: a project connected in Pointman's Project pane keeps that target, and opens the board
by it. A project's issues can be in several teams: its columns are each team's states by name (the first
team's order first), and a move sets the issue's own team's state of that name.

- **Columns:** the team's workflow states in Linear's order, each with its category. For example, In
  Review is `review`, and a started state named for a hold is `active`.
- **Cards:** the team's or project's open issues, and those closed in the last 14 days. Each card has its identifier,
  epic or task (the EPIC label or an "[EPIC] …" title), kind (by the Bug and Feature labels), state,
  assignee, parent, cycle, due date, priority, Linear's order as `rank`, and url.
- **Groups:** the team's recent cycles as sprints, and the issues' project milestones.
- **Size:** a board over a frame's 1 MiB leaves out the longest-closed issues first, then the least
  recently changed, and `project.note` says how many.
- **Gestures:**
  - `move {item, to}` sets the state.
  - `rank {item, before | after}` sets `sortOrder` between the neighbours in that column.
  - `edit {item, fields}` takes `title`, `priority` and `target`. Other fields are changed in Linear.
  - `open {item}` is the app's own.
- **Keeping current:** every minute (`refresh_seconds`), an open board asks Linear what changed since its
  last look, and all of it every ten minutes. Only the change goes to the apps, through the SDK's
  `show_board`.

## What can be connected

`linear.boards` (`/linear boards`) lists the teams the key sees, then its Linear projects that aren't
completed or canceled, each by name. It's the board's `targets`, so the apps' **Add a project board** runs
it to offer what can be connected:

```
{targets: [{target: {team: "ALA"}, name: "Spark", key: "ALA", kind: "team"},
           {target: {project: "<id>"}, name: "Spark Voice", key: "SV", kind: "project"}],
 text: "1 Linear team: Spark (ALA). 1 Linear project: Spark Voice."}
```

`key` is the project key it suggests: a team's own, or a project name's initials (a one-word name's first
three letters), as the apps suggest one; none when the name gives no key.

Settings: `refresh_seconds` (60), and `api_url` (Linear's GraphQL endpoint).

## Building

`cargo test` runs it against a stand-in node and a stand-in Linear: a GraphQL server on a free local port
that answers as Linear's API would. Releases are built per platform by `.github/workflows/release.yml`,
as for every compiled extension.
