"""Explicit, timezone-aware UTC dates for corpus selection."""
import argparse
from datetime import datetime, timezone


def utc_datetime(value):
    """Parse an ISO 8601 timestamp without consulting the host timezone."""
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            f"invalid timestamp {value!r}; use ISO 8601 with Z or an explicit UTC offset"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise argparse.ArgumentTypeError(
            f"timestamp {value!r} needs an explicit timezone (Z or a UTC offset)"
        )
    return parsed.astimezone(timezone.utc)
