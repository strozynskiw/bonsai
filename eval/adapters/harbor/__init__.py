"""Harbor adapter for a preinstalled Bonsai binary."""

__all__ = ["Bonsai"]


def __getattr__(name: str):
    """Keep offline preparation and reporting independent of Harbor installs."""
    if name == "Bonsai":
        from .bonsai import Bonsai

        return Bonsai
    raise AttributeError(name)
