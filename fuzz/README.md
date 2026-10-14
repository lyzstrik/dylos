# Parser fuzzing

Install the pinned nightly and cargo-fuzz as described in `docs/host.md`.
Run `just fuzz-check`, then `just fuzz labspec 120` (or `manifest` or
`agent_protocol`). The default duration is 120 seconds; weekly CI uses 300.

`seeds/` contains copies of lab YAML and core test fixtures plus minimal valid
messages. Recipes copy these into ignored `corpus/` directories before running;
new generated inputs stay local. Crashes are written under ignored `artifacts/`
and uploaded by CI on failure. Reproduce a finding with its artifact as an input
to the corresponding harness; report it separately without changing production
parsers in this work.
