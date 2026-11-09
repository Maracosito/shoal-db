"""Composable metadata filter expressions.

Filters serialize to the Shoal wire format:

Leaf nodes::

    {"field": "price", "op": "lte", "value": 100}

Combinators::

    {"op": "and", "filters": [...]}
    {"op": "or",  "filters": [...]}
    {"op": "not", "filter": {...}}

Two ergonomic styles are supported and may be mixed freely:

Function style::

    from shoal import filters as f
    flt = f.and_(f.eq("lang", "en"), f.gte("stars", 100))

Field style (operator overloading)::

    from shoal import Field
    flt = (Field("lang") == "en") & (Field("stars") >= 100)
"""

from __future__ import annotations

import copy
from typing import Any, Dict, Iterable, List, Sequence


_LEAF_OPS = frozenset(
    {"eq", "not_eq", "gt", "gte", "lt", "lte", "in", "contains_any", "prefix"}
)


class Filter:
    """An immutable filter expression node."""

    __slots__ = ("_payload",)

    def __init__(self, payload: Dict[str, Any]) -> None:
        self._payload = payload

    def to_dict(self) -> Dict[str, Any]:
        """Serialize this expression to the wire format (deep copy)."""
        return copy.deepcopy(self._payload)

    def __and__(self, other: "Filter") -> "Filter":
        return _combine("and", self, other)

    def __or__(self, other: "Filter") -> "Filter":
        return _combine("or", self, other)

    def __invert__(self) -> "Filter":
        # Double negation collapses.
        if self._payload.get("op") == "not" and "filter" in self._payload:
            return Filter(copy.deepcopy(self._payload["filter"]))
        return Filter({"op": "not", "filter": copy.deepcopy(self._payload)})

    def __repr__(self) -> str:
        return f"Filter({self._payload!r})"


def _combine(op: str, a: Filter, b: Filter) -> Filter:
    if not isinstance(b, Filter):  # pragma: no cover - defensive
        raise TypeError(f"cannot combine Filter with {type(b).__name__}")
    parts: List[Dict[str, Any]] = []
    for node in (a, b):
        payload = node.to_dict()
        # Flatten nested combinators of the same kind: (a & b) & c -> and[a, b, c]
        if payload.get("op") == op and isinstance(payload.get("filters"), list):
            parts.extend(payload["filters"])
        else:
            parts.append(payload)
    return Filter({"op": op, "filters": parts})


def _leaf(field: str, op: str, value: Any) -> Filter:
    if not isinstance(field, str) or not field:
        raise ValueError("filter field must be a non-empty string")
    if op not in _LEAF_OPS:  # pragma: no cover - internal invariant
        raise ValueError(f"unknown filter op: {op}")
    return Filter({"field": field, "op": op, "value": value})


# --------------------------------------------------------------------------
# Function-style constructors
# --------------------------------------------------------------------------

def eq(field: str, value: Any) -> Filter:
    """field == value"""
    return _leaf(field, "eq", value)


def not_eq(field: str, value: Any) -> Filter:
    """field != value"""
    return _leaf(field, "not_eq", value)


def gt(field: str, value: Any) -> Filter:
    """field > value"""
    return _leaf(field, "gt", value)


def gte(field: str, value: Any) -> Filter:
    """field >= value"""
    return _leaf(field, "gte", value)


def lt(field: str, value: Any) -> Filter:
    """field < value"""
    return _leaf(field, "lt", value)


def lte(field: str, value: Any) -> Filter:
    """field <= value"""
    return _leaf(field, "lte", value)


def in_(field: str, values: Iterable[Any]) -> Filter:
    """field value is one of `values`."""
    return _leaf(field, "in", list(values))


def contains_any(field: str, values: Iterable[Any]) -> Filter:
    """Array-valued field contains at least one of `values`."""
    return _leaf(field, "contains_any", list(values))


def prefix(field: str, value: str) -> Filter:
    """String field starts with `value`."""
    return _leaf(field, "prefix", value)


def and_(*filters: Filter) -> Filter:
    """Logical AND over one or more filters."""
    return _nary("and", filters)


def or_(*filters: Filter) -> Filter:
    """Logical OR over one or more filters."""
    return _nary("or", filters)


def not_(filter: Filter) -> Filter:
    """Logical NOT of a filter."""
    return ~filter


def _nary(op: str, filters: Sequence[Filter]) -> Filter:
    if not filters:
        raise ValueError(f"{op}_() requires at least one filter")
    if len(filters) == 1:
        return Filter(filters[0].to_dict())
    result = filters[0]
    for f in filters[1:]:
        result = _combine(op, result, f)
    return result


# --------------------------------------------------------------------------
# Field-style construction via operator overloading
# --------------------------------------------------------------------------

class Field:
    """A named attribute that builds filters via operator overloading.

    Example::

        flt = (Field("lang") == "en") & (Field("stars") >= 100)
    """

    __slots__ = ("_name",)

    def __init__(self, name: str) -> None:
        if not isinstance(name, str) or not name:
            raise ValueError("Field name must be a non-empty string")
        self._name = name

    @property
    def name(self) -> str:
        return self._name

    def __eq__(self, value: Any) -> Filter:  # type: ignore[override]
        return eq(self._name, value)

    def __ne__(self, value: Any) -> Filter:  # type: ignore[override]
        return not_eq(self._name, value)

    def __gt__(self, value: Any) -> Filter:
        return gt(self._name, value)

    def __ge__(self, value: Any) -> Filter:
        return gte(self._name, value)

    def __lt__(self, value: Any) -> Filter:
        return lt(self._name, value)

    def __le__(self, value: Any) -> Filter:
        return lte(self._name, value)

    def in_(self, values: Iterable[Any]) -> Filter:
        return in_(self._name, values)

    def contains_any(self, values: Iterable[Any]) -> Filter:
        return contains_any(self._name, values)

    def prefix(self, value: str) -> Filter:
        return prefix(self._name, value)

    # __eq__ is overloaded for filter construction, so restore hashability.
    __hash__ = object.__hash__

    def __repr__(self) -> str:
        return f"Field({self._name!r})"
