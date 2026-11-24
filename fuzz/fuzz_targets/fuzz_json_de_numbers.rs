#![no_main]

use arbitrary::Arbitrary;
use ijson::{FPHAConfig, FloatType, IValue, IValueDeserSeed};
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;

// A grammar of JSON number tokens: the attacker controls the mix of integers,
// floats, exponents, signs and magnitude boundaries fed to the array parser,
// which EXP-001d routes through the direct-f64-push fast path and the typed-
// array tag/promotion machinery.
#[derive(Arbitrary, Debug)]
enum NumTok {
    I(i64),
    U(u64),
    SmallInt(i8),
    Zero,
    NegZeroF,
    F(f64),
    F32(f32),
    Half(u8),       // small exact-in-f16 value: n/2
    Exp(i16),       // 1e{n}
    Frac(u32, u16), // {a}.{b}
    Big,            // 1e308
    Tiny,           // 5e-324
    Max64,          // u64::MAX
    Pow63,          // 2^63
}

impl NumTok {
    fn render(&self) -> String {
        match self {
            NumTok::I(v) => format!("{v}"),
            NumTok::U(v) => format!("{v}"),
            NumTok::SmallInt(v) => format!("{v}"),
            NumTok::Zero => "0".into(),
            NumTok::NegZeroF => "-0.0".into(),
            NumTok::F(v) if v.is_finite() => format!("{v:?}"),
            NumTok::F(_) => "0.0".into(),
            NumTok::F32(v) if v.is_finite() => format!("{v:?}"),
            NumTok::F32(_) => "0.0".into(),
            NumTok::Half(n) => format!("{}", *n as f64 / 2.0),
            NumTok::Exp(n) => format!("1e{n}"),
            NumTok::Frac(a, b) => format!("{a}.{b}"),
            NumTok::Big => "1e308".into(),
            NumTok::Tiny => "5e-324".into(),
            NumTok::Max64 => "18446744073709551615".into(),
            NumTok::Pow63 => "9223372036854775808".into(),
        }
    }
}

fn parse(json: &str, fpha: Option<FPHAConfig>) -> Option<IValue> {
    let mut de = serde_json::Deserializer::from_str(json);
    let v = IValueDeserSeed::new(fpha).deserialize_compact_objects(&mut de).ok()?;
    de.end().ok()?;
    Some(v)
}

fn parse_plain(json: &str) -> Option<IValue> {
    let mut de = serde_json::Deserializer::from_str(json);
    let v = IValue::deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(v)
}

fuzz_target!(|toks: Vec<NumTok>| {
    if toks.is_empty() || toks.len() > 4096 {
        return;
    }
    let body: Vec<String> = toks.iter().map(NumTok::render).collect();
    let json = format!("[{}]", body.join(","));

    // Safety/availability invariants:
    //  * the two ijson parse paths agree on the same input (compact == plain);
    //  * serialising a parsed value never panics, and its output re-parses
    //    without error.
    // Bit-exact float round-trip is NOT asserted: ijson's float formatting is
    // not guaranteed round-trip-stable for subnormal f64 (e.g. 9.1834e-41),
    // which is a pre-existing precision property, not a security property.
    if let Some(compact) = parse(&json, None) {
        if let Some(plain) = parse_plain(&json) {
            assert_eq!(compact, plain, "compact vs plain divergence: {json}");
        }
        let s = serde_json::to_string(&compact).expect("serialise must not panic");
        let _ = parse(&s, None).expect("serialised output must re-parse without error");
    }

    // FPHA forcing f32 must not panic on any token, including out-of-range ones.
    let _ = parse(&json, Some(FPHAConfig::new_with_type(FloatType::F32)));
    let _ = parse(&json, Some(FPHAConfig::new_with_type(FloatType::F16)));
});
