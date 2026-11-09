"""tidegauge: parse, summarise, and store tide gauge readings."""

from .parser import Reading, parse_csv, parse_file
from .stats import find_high_tides, mean_level, moving_average, tidal_range
from .store import load_readings, save_readings

__version__ = "0.1.0"

__all__ = [
    "Reading",
    "parse_csv",
    "parse_file",
    "mean_level",
    "tidal_range",
    "moving_average",
    "find_high_tides",
    "save_readings",
    "load_readings",
]
