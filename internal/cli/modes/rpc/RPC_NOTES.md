# RPC Mode Notes

Implemented command subset:

- `prompt`
- `steer`
- `follow_up`
- `abort`
- `set_model`
- `set_thinking` / `set_thinking_level`
- `get_state`
- `get_messages`
- `get_last_assistant_text`
- `quit`

Deferred TS RPC commands:

- session rebinding and tree operations: `new_session`, `switch_session`, `fork`, `clone`, `session_fork`, `session_move`
- compaction controls
- retry controls
- bash side-channel execution
- model cycling and model registry enumeration
- extension UI request/response plumbing
- slash command enumeration
