# bmux_agents_plugin_api

Typed contract of the bundled `bmux.agents` plugin, generated from
`bpdl/agents-plugin.bpdl`:

- `agent_states`: the retained state channel (`bmux.agents/agent-states`)
  carrying every pane that runs a coding agent and what the agent is doing.
- `agents_state`: the same snapshot on request (`bmux.agents.read`).

See `plugins/agents-plugin/README.md` for how agents are detected.
