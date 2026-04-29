# Runner

The bounded context that owns *agentry as action-runner*. The engine
executes message graphs; the Commandant authors them. Roles, workflow
topologies, and the workflow vocabulary are the three classes of catalog
the runner consumes at dispatch time. This concept doc records the surface
introduced by EPIC #182 brief A (#183): an operator-facing CLI for the
team catalog, an atomic register primitive, and a dispatch-time
validation hook.

The catalog has three classes. *Roles* are engine-seeded — the inline
literals in `crates/orchestrator-runtime/src/seed.rs` register every
`AgentRole` the runtime knows how to spawn. *Workflows* (`TeamTopology`
records) cohabit two seed sources: engine-seeded literals also from
`seed.rs`, and Commandant-authored topologies registered through the
`orchestrator team register` CLI. Both write to the same Redis key
namespace (`agentry:team:<name>:v<version>`); a future migration is
per-topology and lazy. *Vocabulary* — the field set of `TeamTopology`,
`MessageEdge`, and `AgentRole` — is engine-defined and evolves only
additively: new fields land as optional with serde defaults, and strict
deserialization (`#[serde(deny_unknown_fields)]`) rejects typos or
unknown keys at parse time.

Operators interact with the team catalog through `orchestrator team
{list,show,register,validate}` (see `crates/orchestrator-runtime/src/cli_teams.rs`).
`list` prints one `{"name":"…","version":N}` JSON object per registered
team (NDJSON). `show <name> <version>` pretty-prints the full
`TeamTopology`. `register <file>` parses a `TeamTopology` JSON file under
strict serde, fetches the role registry, runs `team_validator::validate`,
and on a clean pass calls `register_team_strict` to write the body
atomically — the SET is `NX`, so a second register at the same
`(name, version)` reports `already_exists` without overwriting the first
writer's body. `validate <file>` runs the same parse-and-validate
pipeline but never persists; it prints `{"valid":true}` or
`{"valid":false,"violations":[…]}`.

The validator runs the six checks listed in
`specs/concepts/validation.md` — vocabulary integrity (parse-time, by
serde's `deny_unknown_fields`), type integrity, reference integrity,
topological integrity, acyclicity, and single-terminal — at two trigger
points. The CLI runs them at register time so the operator gets
immediate feedback before the body lands in Redis. The daemon runs them
at dispatch time, after `redis_io::fetch_team` and before any role
spawn, so a topology that was malformed at write time (or whose role
registry has since changed under it) is rejected with a structured
`team_validation_failed` trace event and an `Error::Config` that
short-circuits the brief without spawning anything.

Cohabitation between Rust-source seed and CLI-register is intentional.
`save_team` overwrites — it is idempotent and used by `seed.rs` to
re-publish the engine-seeded topologies on every `orchestrator seed`.
`register_team_strict` is first-writer-wins — operators cannot
accidentally clobber an existing topology by re-running their register
command, and the engine's seeded topologies cannot accidentally clobber
an operator's freshly-authored one. The same key namespace means the
daemon's `fetch_team` does not need to know which path wrote the entry.

One open question is deliberately deferred. Inline workflow definitions
inside a brief payload (the brief carries its own ad-hoc topology rather
than naming a registered one) are not supported in this surface; today
every brief names a `(team_name, version)` already in the catalog.
Per-target-repo workflow variability is tracked on its own brief
(#189). Both will land as additive vocabulary changes once the runner
primitive proves out.

## RegisterOutcome

The result of a strict register. `Registered` means the SET succeeded and
this call's body is now the canonical body at
`agentry:team:<name>:v<version>`. `AlreadyExists` means a body was
already present at that key and was left untouched — the caller did not
win the race. Returned by `redis_io::register_team_strict`; the CLI maps
`AlreadyExists` to a non-zero exit so duplicate-register attempts are
visible in shell pipelines.
