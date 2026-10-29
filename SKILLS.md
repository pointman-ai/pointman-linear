# Linear

Linear runs through the `linear` extension on the machine that has it: ask with `create_job` (`machine` set), or
`/linear <verb>` in a thread. Its keys come from the vault for each call (`linear.<label>`, one per Linear
workspace): you never need one, and you never write a key into a post, an argument or a file. With no key on the
machine, ask your person to run `/linear add` in the app.

| For | Command | Gives |
|---|---|---|
| What an issue on a team can be set to | `linear.team {team}` | states, people, labels, sprints |
| Open projects | `linear.projects {}` | names and ids |
| Find issues | `linear.search {query?, team?, state?, owner?, project?, all?, limit?}` | issues, open ones unless `all` |
| Read one | `linear.issue {issue}` | the issue with its description, comments and sub-issues |
| File one | `linear.create {team, title, body?, state?, owner?, priority?, target?, estimate?, labels?, sprint?, parent?, project?}` | the new issue and its link |
| Change one | `linear.update {issue, ...}` | the issue after; only the fields given change, null clears one |
| Comment | `linear.comment {issue, body}` | |

- **Look before you write.** `linear.team` lists the states, people, labels and sprints a team has; a value it doesn't
  have is refused, with what it does have. `issue` is a ref (`ALA-12`), `team` a key (`ALA`).
- **Each call tries the vault's keys in turn,** and uses the one that sees the team or issue. Name one with
  `account` (`work`, for `linear.work`) only if you must.
- **Search before you file,** so the same thing isn't filed twice. Put the why and a link to the thread in `body`.
- **Whose name:** the vault's keys are personal, so what you file or comment shows as the key's owner. Write as that
  person would: first person, their voice, no mention of agents, the board or its threads.
