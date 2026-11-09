# Documentation

[Project README](../README.md)

The root README is the quick start. Detailed API guidance, implementation
contracts and maintenance procedures live here.

| Working on | Read |
|------------|------|
| Library integration, `no_std`, LinkAudio, encoding, clocks or scheduling | [API guide](api.md) |
| Runnable demos, audio playback, Docker or ESP32 | [Examples](examples.md) |
| API migration, state publication, RT capture/commit, session identity or ordering | [Migration and managed-state contracts](migration.md) |
| Discovery routing, interface leases, topology changes or platform evidence | [Discovery interfaces](discovery-ingress.md) |
| Owned IO runtime, enable/disable, callbacks or destruction | [Execution and shutdown](execution-lifecycle.md) |
| Source layout, dependencies, supported builds or validation | [Development](development.md) |
| Upstream watermark, backlog, workflow compilation, reviews or releases | [Upstream automation](upstream-automation.md) |

## For contributors and agents

Start with [repository instructions](../.github/copilot-instructions.md), then
read the relevant contracts above before changing behavior. Migration notes
retain source-breaking changes and edge cases that were previously in the
README; consult the current code and tests when applying them.

Update the affected topic document alongside code. Keep the root README focused
on installation, a small example, feature selection and navigation; use a brief
summary or link there when its maintenance rule requires an update. Do not append
per-fix implementation narratives to it.

Keep evidence limits explicit: a skipped prerequisite is not a tested guarantee,
and green CI does not establish missing Windows duplicate-network or interface-
incarnation evidence.
