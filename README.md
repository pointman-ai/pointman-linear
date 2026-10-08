# linear

Linear on Pointman's machines (MB-343, Pointman's docs/dev/panes.md): a Linear team's board as a pane,
drawn by the apps' own board, with the same columns, cards, filters and drag as Pointman's projects. Linear's
workflow states become columns in the projects syntax's seven categories, through the same map core's Linear
mirror reads (tracker.toml, a copy built in). A move, a rank or an edit goes to Linear, and the card settles
where Linear puts it. If Linear says no, the card snaps back with Linear's reason.

Linear is asked with the vault's API key (`/linear add`), from the node, so the apps never hold it.

```
pointman add linear
```

## The board pane

`[[panes]] id = "board"`, `format = "board"`. Opened with `{team: "ALA"}` and optionally
`project: "Spark"`, or from the apps' menu with no target, which uses the settings' `team` and `project`.

- **Columns:** the team's workflow states in Linear's order, each with its category. For example, In
  Review is `review`, and a started state named for a hold is `active`.
- **Cards:** the team's open issues, and those closed in the last 14 days. Each card has its identifier,
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

Settings: `team`, `project`, `refresh_seconds` (60), and `api_url` (Linear's GraphQL endpoint).

## Building

`cargo test` runs it against a stand-in node and a stand-in Linear: a GraphQL server on a free local port
that answers as Linear's API would. Releases are built per platform by `.github/workflows/release.yml`,
as for every compiled extension.
