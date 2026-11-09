import pytest

from shoal import Field, filters as f


def test_leaf_serialization():
    assert f.eq("lang", "en").to_dict() == {"field": "lang", "op": "eq", "value": "en"}
    assert f.not_eq("lang", "en").to_dict() == {"field": "lang", "op": "not_eq", "value": "en"}
    assert f.gt("stars", 5).to_dict() == {"field": "stars", "op": "gt", "value": 5}
    assert f.gte("stars", 5).to_dict() == {"field": "stars", "op": "gte", "value": 5}
    assert f.lt("stars", 5).to_dict() == {"field": "stars", "op": "lt", "value": 5}
    assert f.lte("stars", 5).to_dict() == {"field": "stars", "op": "lte", "value": 5}
    assert f.in_("lang", ("en", "fr")).to_dict() == {
        "field": "lang", "op": "in", "value": ["en", "fr"],
    }
    assert f.contains_any("tags", ["a", "b"]).to_dict() == {
        "field": "tags", "op": "contains_any", "value": ["a", "b"],
    }
    assert f.prefix("path", "src/").to_dict() == {"field": "path", "op": "prefix", "value": "src/"}


def test_and_or_combinators():
    combined = f.eq("a", 1) & f.eq("b", 2)
    assert combined.to_dict() == {
        "op": "and",
        "filters": [
            {"field": "a", "op": "eq", "value": 1},
            {"field": "b", "op": "eq", "value": 2},
        ],
    }
    either = f.eq("a", 1) | f.eq("b", 2)
    assert either.to_dict()["op"] == "or"


def test_and_flattening():
    combined = f.eq("a", 1) & f.eq("b", 2) & f.eq("c", 3)
    payload = combined.to_dict()
    assert payload["op"] == "and"
    assert len(payload["filters"]) == 3
    assert all("filters" not in part for part in payload["filters"])


def test_not_and_double_negation():
    negated = ~f.eq("a", 1)
    assert negated.to_dict() == {"op": "not", "filter": {"field": "a", "op": "eq", "value": 1}}
    assert (~negated).to_dict() == {"field": "a", "op": "eq", "value": 1}


def test_nary_helpers():
    payload = f.and_(f.eq("a", 1), f.eq("b", 2), f.eq("c", 3)).to_dict()
    assert payload["op"] == "and"
    assert len(payload["filters"]) == 3
    single = f.or_(f.eq("a", 1))
    assert single.to_dict() == {"field": "a", "op": "eq", "value": 1}
    with pytest.raises(ValueError):
        f.and_()


def test_field_operator_style():
    assert (Field("lang") == "en").to_dict() == {"field": "lang", "op": "eq", "value": "en"}
    assert (Field("lang") != "en").to_dict() == {"field": "lang", "op": "not_eq", "value": "en"}
    assert (Field("stars") > 5).to_dict()["op"] == "gt"
    assert (Field("stars") >= 5).to_dict()["op"] == "gte"
    assert (Field("stars") < 5).to_dict()["op"] == "lt"
    assert (Field("stars") <= 5).to_dict()["op"] == "lte"
    assert Field("lang").in_(["en"]).to_dict()["op"] == "in"
    assert Field("tags").contains_any(["x"]).to_dict()["op"] == "contains_any"
    assert Field("path").prefix("src/").to_dict()["op"] == "prefix"

    combined = (Field("lang") == "en") & (Field("stars") >= 100)
    assert combined.to_dict()["op"] == "and"


def test_to_dict_returns_copy():
    flt = f.eq("a", 1)
    first = flt.to_dict()
    first["value"] = 999
    assert flt.to_dict()["value"] == 1


def test_invalid_field():
    with pytest.raises(ValueError):
        f.eq("", 1)
    with pytest.raises(ValueError):
        Field("")
