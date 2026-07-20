# Bonsai Harbor adapter

This adapter targets
`harbor-framework/harbor@2d3f78d55a703df2f76c005d7df44a5ce2d8adf5`
and Terminal-Bench 2.0 at
`harbor-framework/terminal-bench-2@2fd12b88aafdd04a52c298e3940bcb189f9766d6`.

The Bonsai binary must already be present in the Harbor task image, or
`BONSAI_BIN` must name it. The adapter deliberately contains no installer and
never downloads Bonsai, Harbor, a dataset, or a container image.

Use Harbor's import-path agent support with
`eval.adapters.harbor.bonsai:Bonsai`, a `<provider>/<model>` model name, and
explicit values for:

- `autonomy`
- `reasoning_effort`
- `max_turns`
- `max_generation_seconds`
- `max_output_chars`
- `max_tool_seconds`
- `timeout_seconds`
- `network` (`deny` or `allow`)

Harbor owns environment setup, the verifier, reward calculation, timeout
cleanup, and result persistence. Bonsai runs with `--isolation off` because the
prepared Harbor environment is already the disposable workspace boundary; its
normal effect, permission, hook, sandbox, and credential policies still apply.
