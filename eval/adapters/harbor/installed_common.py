"""Shared no-install helpers for disposable Harbor task environments."""

import shlex
import uuid
from collections.abc import Mapping
from urllib.parse import urlsplit


def required_environment(overlay: Mapping[str, str], names: tuple[str, ...]) -> dict[str, str]:
    """Select explicit trial values only; never consult Harbor's host fallback."""
    selected = {}
    for name in names:
        value = overlay.get(name)
        if not isinstance(value, str) or not value.strip() or "\0" in value:
            raise ValueError(f"explicit trial environment required: {name}")
        selected[name] = value
    return selected


def require_https_endpoint(endpoint: str) -> None:
    """Reject credential-bearing or ambiguous endpoints before any trial effect."""
    parsed = urlsplit(endpoint)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
            or parsed.password is not None or parsed.query or parsed.fragment
            or any(character.isspace() or ord(character) < 32 for character in endpoint)):
        raise ValueError("explicit credential-free HTTPS endpoint required")
    # Accessing port also rejects malformed/out-of-range ports.
    parsed.port


def private_home(product: str) -> str:
    return f"/tmp/tb4-{product}-{uuid.uuid4().hex}"


def isolated_command(command: str, env: dict[str, str], home: str) -> tuple[str, dict[str, str]]:
    """Clear ambient process env without putting secret values in shell text.

    Harbor exec receives only explicitly named values. env -i also removes
    credentials baked into the task image; approved task env must be added here.
    """
    selected = {"HOME": home, "XDG_CONFIG_HOME": home + "/config",
                "XDG_CACHE_HOME": home + "/cache", "XDG_DATA_HOME": home + "/data",
                "PATH": "/opt/benchmark/bin:/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", **env}
    bindings = " ".join(f'{shlex.quote(key)}="${{{key}}}"' for key in selected)
    return f"env -i {bindings} /bin/bash --noprofile --norc -c {shlex.quote(command)}", selected


def redact(text: str, secrets: tuple[str, ...]) -> str:
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[REDACTED]")
    return text
