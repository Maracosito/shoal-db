//! Filter-algebra validation suite.
//!
//! The central invariant of the filter engine is that **set-based evaluation
//! through the attribute index agrees exactly with row-wise evaluation** of
//! the same AST over the same documents. This suite checks that invariant on
//! hand-picked filters covering every operator, and on hundreds of randomly
//! generated nested filters, then verifies boolean algebra laws, the
//! `NotEq` vs `Not(Eq)` distinction, serialization round-trips, and the
//! planner-facing cardinality estimator.

use shoal_query::filter::{
    Attributes, AttributeIndex, AttributeIndexBuilder, DocSet, Filter, Value,
};

/// Deterministic xorshift64 so the suite is reproducible without an RNG crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

const CATS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
const TAGS: [&str; 5] = ["red", "green", "blue", "yellow", "purple"];

fn gen_docs(rng: &mut Rng, n: usize) -> Vec<Attributes> {
    (0..n)
        .map(|i| {
            let mut d = Attributes::new();
            if rng.below(10) != 0 {
                d.insert(
                    "category".to_string(),
                    Value::from(CATS[rng.below(4) as usize]),
                );
            }
            if rng.below(10) != 0 {
                d.insert(
                    "score".to_string(),
                    Value::Float(rng.below(10_000) as f64 / 100.0),
                );
            }
            if rng.below(10) != 0 {
                d.insert("count".to_string(), Value::Int(rng.below(101) as i64 - 50));
            }
            d.insert("active".to_string(), Value::Bool(rng.below(2) == 0));
            let ntags = rng.below(4) as usize;
            let mut tv = Vec::new();
            for _ in 0..ntags {
                tv.push(Value::from(TAGS[rng.below(5) as usize]));
            }
            d.insert("tags".to_string(), Value::Array(tv));
            d.insert("name".to_string(), Value::from(format!("item-{i:04}")));
            d
        })
        .collect()
}

fn build_index(docs: &[Attributes]) -> AttributeIndex {
    let mut b = AttributeIndexBuilder::new();
    for (i, d) in docs.iter().enumerate() {
        b.add_doc(i as u32, d);
    }
    b.build()
}

fn row_eval(docs: &[Attributes], f: &Filter) -> Vec<u32> {
    docs.iter()
        .enumerate()
        .filter(|(_, d)| f.matches(d))
        .map(|(i, _)| i as u32)
        .collect()
}

fn sample_filters() -> Vec<Filter> {
    vec![
        Filter::eq("category", "alpha"),
        Filter::eq("active", true),
        Filter::eq("count", 0i64),
        Filter::eq("count", 0.0), // cross-type numeric equality
        Filter::not_eq("category", "beta"),
        Filter::gt("score", 50.0),
        Filter::gte("score", 50.0),
        Filter::lt("count", -10i64),
        Filter::lte("count", -10i64),
        Filter::gt("category", "beta"), // lexicographic string range
        Filter::lte("category", "beta"),
        Filter::is_in("category", ["alpha", "delta"]),
        Filter::contains_any("tags", ["red", "blue"]),
        Filter::is_in("tags", ["green"]), // In over an array field
        Filter::starts_with("name", "item-00"),
        Filter::contains("name", "42"),
        Filter::and(vec![Filter::eq("active", true), Filter::gt("score", 25.0)]),
        Filter::or(vec![
            Filter::eq("category", "alpha"),
            Filter::contains_any("tags", ["purple"]),
        ]),
        Filter::and(vec![
            Filter::or(vec![
                Filter::eq("category", "alpha"),
                Filter::eq("category", "beta"),
            ]),
            Filter::eq("active", false).negate(),
            Filter::lt("score", 90.0),
        ]),
        Filter::not_eq("count", 7i64).negate(),
        Filter::and(vec![]), // matches everything
        Filter::or(vec![]),  // matches nothing
        Filter::eq("missing_field", 1i64),
        Filter::not_eq("missing_field", 1i64),
        Filter::eq("missing_field", 1i64).negate(),
        Filter::gt("active", false), // bools are unordered -> empty
    ]
}

#[test]
fn index_eval_matches_row_eval_on_sample_filters() {
    let mut rng = Rng::new(0x5EED_0001);
    let docs = gen_docs(&mut rng, 500);
    let idx = build_index(&docs);
    for f in sample_filters() {
        let expected = row_eval(&docs, &f);
        let actual = idx.eval(&f);
        assert_eq!(
            actual.as_slice(),
            expected.as_slice(),
            "index vs row mismatch for {f:?}"
        );
    }
}

fn rand_leaf(rng: &mut Rng) -> Filter {
    match rng.below(9) {
        0 => Filter::eq("category", CATS[rng.below(4) as usize]),
        1 => Filter::eq("active", rng.below(2) == 0),
        2 => Filter::gt("score", rng.below(100) as f64),
        3 => Filter::lte("score", rng.below(100) as f64),
        4 => Filter::gte("count", rng.below(101) as i64 - 50),
        5 => Filter::lt("count", rng.below(101) as i64 - 50),
        6 => {
            let pairs = [["red", "green"], ["blue", "yellow"], ["purple", "red"]];
            Filter::contains_any("tags", pairs[rng.below(3) as usize])
        }
        7 => Filter::starts_with("name", format!("item-{}", rng.below(10))),
        _ => Filter::not_eq("category", "beta"),
    }
}

fn rand_filter(rng: &mut Rng, depth: u32) -> Filter {
    if depth == 0 {
        return rand_leaf(rng);
    }
    match rng.below(4) {
        0 => Filter::and(
            (0..2 + rng.below(2))
                .map(|_| rand_filter(rng, depth - 1))
                .collect(),
        ),
        1 => Filter::or(
            (0..2 + rng.below(2))
                .map(|_| rand_filter(rng, depth - 1))
                .collect(),
        ),
        2 => rand_filter(rng, depth - 1).negate(),
        _ => rand_leaf(rng),
    }
}

#[test]
fn index_eval_matches_row_eval_on_random_nested_filters() {
    let mut rng = Rng::new(0x5EED_0002);
    let docs = gen_docs(&mut rng, 300);
    let idx = build_index(&docs);
    for case in 0..200 {
        let f = rand_filter(&mut rng, 3);
        let expected = row_eval(&docs, &f);
        let actual = idx.eval(&f);
        assert_eq!(
            actual.as_slice(),
            expected.as_slice(),
            "mismatch in random case {case}: {f:?}"
        );
    }
}

#[test]
fn boolean_algebra_laws_hold_under_index_eval() {
    let mut rng = Rng::new(0x5EED_0003);
    let docs = gen_docs(&mut rng, 250);
    let idx = build_index(&docs);
    let universe = idx.universe().clone();

    let a = Filter::eq("category", "alpha");
    let b = Filter::gt("score", 50.0);

    // De Morgan: ¬(A ∧ B) = ¬A ∨ ¬B
    let lhs = idx.eval(&Filter::and(vec![a.clone(), b.clone()]).negate());
    let rhs = idx.eval(&Filter::or(vec![a.clone().negate(), b.clone().negate()]));
    assert_eq!(lhs, rhs);

    // De Morgan: ¬(A ∨ B) = ¬A ∧ ¬B
    let lhs = idx.eval(&Filter::or(vec![a.clone(), b.clone()]).negate());
    let rhs = idx.eval(&Filter::and(vec![a.clone().negate(), b.clone().negate()]));
    assert_eq!(lhs, rhs);

    // Double negation: ¬¬A = A
    assert_eq!(idx.eval(&a.clone().negate().negate()), idx.eval(&a));

    // Annihilation / excluded middle (Not = complement over the universe).
    assert_eq!(
        idx.eval(&Filter::and(vec![a.clone(), a.clone().negate()])),
        DocSet::empty()
    );
    assert_eq!(
        idx.eval(&Filter::or(vec![a.clone(), a.clone().negate()])),
        universe
    );

    // Idempotence.
    assert_eq!(
        idx.eval(&Filter::and(vec![a.clone(), a.clone()])),
        idx.eval(&a)
    );
    assert_eq!(
        idx.eval(&Filter::or(vec![b.clone(), b.clone()])),
        idx.eval(&b)
    );
}

#[test]
fn not_eq_differs_from_not_eq_composition_on_missing_fields() {
    // One doc has `category`, the other doesn't.
    let mut with_cat = Attributes::new();
    with_cat.insert("category".to_string(), Value::from("beta"));
    let without_cat = Attributes::new();
    let docs = vec![with_cat, without_cat];
    let idx = build_index(&docs);

    let not_eq = Filter::not_eq("category", "alpha");
    let not_of_eq = Filter::eq("category", "alpha").negate();

    // Row semantics.
    assert!(not_eq.matches(&docs[0]));
    assert!(!not_eq.matches(&docs[1])); // missing field -> leaf is false
    assert!(not_of_eq.matches(&docs[1])); // complement includes missing

    // Index semantics agree.
    assert_eq!(idx.eval(&not_eq).as_slice(), &[0]);
    assert_eq!(idx.eval(&not_of_eq).as_slice(), &[0, 1]);
}

#[test]
fn serialization_roundtrip_preserves_all_eval_results() {
    let mut rng = Rng::new(0x5EED_0004);
    let docs = gen_docs(&mut rng, 400);
    let idx = build_index(&docs);

    let bytes = idx.encode();
    let decoded = AttributeIndex::decode(&bytes).expect("decode");
    assert_eq!(decoded.doc_count(), idx.doc_count());

    for f in sample_filters() {
        assert_eq!(decoded.eval(&f), idx.eval(&f), "post-decode mismatch: {f:?}");
    }
    for _ in 0..50 {
        let f = rand_filter(&mut rng, 2);
        assert_eq!(decoded.eval(&f), idx.eval(&f), "post-decode mismatch: {f:?}");
    }

    // Corruption is detected, not silently accepted.
    assert!(AttributeIndex::decode(&bytes[..bytes.len() - 5]).is_err());
    assert!(AttributeIndex::decode(b"WRNG\x01\x00\x00\x00").is_err());
}

#[test]
fn filter_json_wire_format_roundtrip() {
    let f = Filter::and(vec![
        Filter::eq("category", "alpha"),
        Filter::contains_any("tags", ["red"]).negate(),
        Filter::gte("score", 12.5),
        Filter::is_in("count", [1i64, 2, 3]),
        Filter::starts_with("name", "item-"),
    ]);
    let json = serde_json::to_string(&f).expect("serialize");
    let back: Filter = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(f, back);

    // Spot-check the wire shape: internally tagged on "op".
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["op"], "and");
    assert_eq!(v["filters"][0]["op"], "eq");
    assert_eq!(v["filters"][0]["field"], "category");
    assert_eq!(v["filters"][0]["value"], "alpha");
    assert_eq!(v["filters"][1]["op"], "not");
    assert_eq!(v["filters"][1]["filter"]["op"], "contains_any");
    assert_eq!(v["filters"][3]["op"], "in");
}

#[test]
fn estimate_is_sane_and_bounds_positive_filters() {
    let mut rng = Rng::new(0x5EED_0005);
    let docs = gen_docs(&mut rng, 350);
    let idx = build_index(&docs);
    let n = idx.doc_count();

    // Estimates never exceed the universe.
    for f in sample_filters() {
        let est = idx.estimate(&f);
        assert!(est <= n, "estimate {est} > universe {n} for {f:?}");
    }

    // For Not-free filters the estimate is an upper bound on true cardinality.
    let monotone = vec![
        Filter::eq("category", "alpha"),
        Filter::gt("score", 50.0),
        Filter::is_in("category", ["alpha", "delta"]),
        Filter::contains_any("tags", ["red", "blue"]),
        Filter::starts_with("name", "item-01"),
        Filter::and(vec![Filter::eq("active", true), Filter::gt("score", 25.0)]),
        Filter::or(vec![
            Filter::eq("category", "alpha"),
            Filter::eq("category", "beta"),
        ]),
        Filter::and(vec![
            Filter::or(vec![
                Filter::contains_any("tags", ["purple"]),
                Filter::lt("count", 0i64),
            ]),
            Filter::lte("score", 75.0),
        ]),
    ];
    for f in monotone {
        let est = idx.estimate(&f);
        let actual = idx.eval(&f).len();
        assert!(
            est >= actual,
            "estimate {est} < actual {actual} for monotone filter {f:?}"
        );
    }
}
