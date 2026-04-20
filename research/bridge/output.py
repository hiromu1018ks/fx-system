"""Write validation results to JSON for Rust consumption."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def write_validation_result(result: dict[str, Any], path: str | Path) -> Path:
    """Write validation result dict to a JSON file.

    Args:
        result: Validation result with 'all_passed', 'n_passed', 'n_failed', 'checks'.
        path: Output file path.

    Returns:
        The path written to.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        json.dump(result, f, indent=2)
    return path
