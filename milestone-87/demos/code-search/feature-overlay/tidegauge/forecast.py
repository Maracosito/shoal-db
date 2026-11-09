"""Tide level forecasting via harmonic analysis (feature branch only).

Fits the principal M2 constituent to observed readings and extrapolates
future water levels. Deliberately simple — one constituent, no nodal
corrections — but genuinely predictive for short horizons.
"""

from __future__ import annotations

import math
from datetime import datetime, timedelta
from typing import List, Sequence, Tuple

from .parser import Reading
from .stats import harmonic_fit

M2_PERIOD_HOURS = 12.42


def predict_levels(
    readings: Sequence[Reading],
    horizon_hours: float,
    step_minutes: int = 30,
    period_hours: float = M2_PERIOD_HOURS,
) -> List[Tuple[datetime, float]]:
    """Predict future tide levels from past readings.

    Fits level(t) = a + b*cos(wt) + c*sin(wt) over the observation window,
    then evaluates the fitted curve at step_minutes intervals for
    horizon_hours past the last observation. Returns (timestamp, level_m)
    pairs.
    """
    if len(readings) < 3:
        raise ValueError("predict_levels needs at least 3 readings")
    t0 = readings[0].timestamp
    times = [(r.timestamp - t0).total_seconds() / 3600.0 for r in readings]
    levels = [r.level_m for r in readings]
    a, b, c = harmonic_fit(times, levels, period_hours=period_hours)

    omega = 2.0 * math.pi / period_hours
    last = readings[-1].timestamp
    out: List[Tuple[datetime, float]] = []
    steps = int(horizon_hours * 60 / step_minutes)
    for k in range(1, steps + 1):
        when = last + timedelta(minutes=step_minutes * k)
        t = (when - t0).total_seconds() / 3600.0
        out.append((when, a + b * math.cos(omega * t) + c * math.sin(omega * t)))
    return out


def amplitude_and_phase(b: float, c: float) -> Tuple[float, float]:
    """Convert cos/sin coefficients to (amplitude_m, phase_radians)."""
    return math.hypot(b, c), math.atan2(c, b)
