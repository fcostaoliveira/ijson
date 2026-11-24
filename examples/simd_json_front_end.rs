//! EXP-019 (measurement only): is the `IValue` built through simd-json's serde deserializer identical
//! to the one built through serde_json's, and how many bytes does ijson's `IValue` retain beside
//! simd-json's own `OwnedValue` DOM?
//!
//! For every referee fixture and a set of number/string edge cases the example parses the input both
//! ways with the same `IValueDeserSeed` (compact objects, no FPHA) and compares: `IValue` equality,
//! the compact serialisation bytes, `mem_allocated()`, and for top-level arrays the typed-array tag
//! and capacity. Retained bytes come from `mockalloc` wrapping the system allocator.
//!
//! Usage: `cargo run --release --no-default-features --example simd_json_front_end`

use std::alloc::System;

use ijson::{IValue, IValueDeserSeed};
use mockalloc::{record_allocs, AllocInfo, Mockalloc};

#[global_allocator]
static ALLOCATOR: Mockalloc<System> = Mockalloc(System);

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}
fn float_array_json(n: usize) -> String {
    let mut g = Lcg(0x5eed);
    let items: Vec<String> = (0..n)
        .map(|_| format!("{:.6}", (g.next() % 2_000_000) as f64 / 1000.0 - 1000.0))
        .collect();
    format!("[{}]", items.join(","))
}
fn int_array_json(n: usize) -> String {
    let mut g = Lcg(0x1d);
    let items: Vec<String> = (0..n)
        .map(|_| ((g.next() % 200_000) as i64 - 100_000).to_string())
        .collect();
    format!("[{}]", items.join(","))
}

fn fixtures() -> Vec<(&'static str, String)> {
    vec![
        (
            "yelp",
            include_str!("../benches/fixtures/pass-jsonsl-yelp.json").to_string(),
        ),
        (
            "yahoo2",
            include_str!("../benches/fixtures/pass-jsonsl-yahoo2.json").to_string(),
        ),
        (
            "gmaps-place",
            include_str!("../benches/fixtures/api_replies_q5_gmaps_place.json").to_string(),
        ),
        (
            "int-arrays",
            include_str!("../benches/fixtures/homogeneous_integer_arrays.json").to_string(),
        ),
        (
            "parser-0000",
            include_str!("../benches/fixtures/parser-0000.json").to_string(),
        ),
        (
            "pass-100",
            include_str!("../benches/fixtures/pass-100.json").to_string(),
        ),
        ("floats-1024", float_array_json(1024)),
        ("ints-1024", int_array_json(1024)),
    ]
}

/// Number and string edge cases RedisJSON's formatting and typed-array tagging depend on.
fn edge_cases() -> Vec<(&'static str, String)> {
    vec![
        ("ints-i64-u64-edges", "[0,-0,1,-1,9223372036854775807,9223372036854775808,18446744073709551615,-9223372036854775808,127,128,-128,-129,255,256,32767,32768,65535,65536,2147483647,2147483648,4294967295,4294967296]".into()),
        ("ints-beyond-u64", "[18446744073709551616]".into()),
        ("ints-below-i64", "[-9223372036854775809]".into()),
        ("floats-formatting", "[1.0,-0.0,0.1,0.2,0.30000000000000004,1e308,1E308,1e+308,1e-7,2.5e-5,123456789.123456789,1E2,1e+2,100.0,5e-324,1.7976931348623157e308,2.2250738585072014e-308,3.14159,-2.5,1e22,1e23,9007199254740993,0.000001,1e-6]".into()),
        ("floats-typed-f32-shape", "[1.5,2.5,-3.25,1024.125]".into()),
        ("mixed-int-float", "[1,2.0,3]".into()),
        ("int-array-small", "[1,2,3,4,5,6,7,8]".into()),
        ("int-array-i16", "[1000,-1000,32767,-32768]".into()),
        ("strings-escapes", r#"["é😀","\n\t\"\\\/","plain ascii","\u0000nul","long string long string long string long string long string long string"]"#.into()),
        ("objects-dup-keys", r#"{"a":1,"b":2,"a":3,"c":{"x":[],"y":{},"x":null}}"#.into()),
        ("object-17-keys", r#"{"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9,"k10":10,"k11":11,"k12":12,"k13":13,"k14":14,"k15":15,"k16":16,"k17":17}"#.into()),
        ("scalars", "true".into()),
        ("null", "null".into()),
        ("empty-containers", "[[],{},[[]],[{}]]".into()),
        ("deep-nesting-64", format!("{}1{}", "[".repeat(64), "]".repeat(64))),
        ("whitespace", " \n\t{ \"a\" : [ 1 , 2 ] } \n".into()),
    ]
}

fn parse_serde(s: &str) -> Result<IValue, String> {
    let mut de = serde_json::Deserializer::from_str(s);
    let v = IValueDeserSeed::new(None)
        .deserialize_compact_objects(&mut de)
        .map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    Ok(v)
}

fn parse_simd(s: &str) -> Result<IValue, String> {
    let mut bytes = s.as_bytes().to_vec();
    let mut de = simd_json::Deserializer::from_slice(&mut bytes).map_err(|e| e.to_string())?;
    IValueDeserSeed::new(None)
        .deserialize_compact_objects(&mut de)
        .map_err(|e| e.to_string())
}

fn describe(v: &IValue) -> String {
    match v.as_array() {
        Some(a) => format!(
            "array tag={:?} len={} cap={}",
            a.as_slice().type_tag(),
            a.len(),
            a.capacity()
        ),
        None => format!("{:?}", v.type_()),
    }
}

/// Run `f`, keep its product alive, and report (product, allocation record).
fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocInfo) {
    let mut slot: Option<T> = None;
    let info = record_allocs(|| slot = Some(f()));
    (slot.unwrap(), info)
}

fn main() {
    let _ = ijson::init_shared_string_cache(false);
    let mut differences = 0;

    println!("## Equivalence (serde_json front end vs simd-json front end, same `IValueDeserSeed`)\n");
    println!("| Input | serde_json | simd-json | IValue == | bytes == | mem_allocated | note |");
    println!("|---|---|---|---|---|---|---|");
    for (name, text) in fixtures().into_iter().chain(edge_cases()) {
        let a = parse_serde(&text);
        let b = parse_simd(&text);
        match (&a, &b) {
            (Ok(a), Ok(b)) => {
                let sa = serde_json::to_vec(a).unwrap();
                let sb = serde_json::to_vec(b).unwrap();
                let eq = a == b;
                let bytes_eq = sa == sb;
                let ma = a.mem_allocated();
                let mb = b.mem_allocated();
                let mut note = if describe(a) != describe(b) {
                    format!("shape: {} vs {}", describe(a), describe(b))
                } else {
                    describe(a)
                };
                if !bytes_eq {
                    // Show the first differing region of the compact output.
                    let i = sa.iter().zip(&sb).position(|(x, y)| x != y).unwrap_or(sa.len().min(sb.len()));
                    let lo = i.saturating_sub(20);
                    note.push_str(&format!(
                        "; first diff @{}: `{}` vs `{}`",
                        i,
                        String::from_utf8_lossy(&sa[lo..(i + 30).min(sa.len())]),
                        String::from_utf8_lossy(&sb[lo..(i + 30).min(sb.len())])
                    ));
                }
                if !eq || !bytes_eq || ma != mb {
                    differences += 1;
                }
                println!(
                    "| {name} | ok | ok | {eq} | {bytes_eq} | {ma:.0} vs {mb:.0} | {note} |"
                );
            }
            (Ok(a), Err(e)) => {
                differences += 1;
                println!("| {name} | ok ({}) | ERROR `{e}` | — | — | — | simd-json rejects what serde_json accepts |", describe(a));
            }
            (Err(e), Ok(b)) => {
                differences += 1;
                println!("| {name} | ERROR `{e}` | ok ({}) | — | — | — | simd-json accepts what serde_json rejects |", describe(b));
            }
            (Err(ea), Err(eb)) => {
                println!("| {name} | ERROR `{ea}` | ERROR `{eb}` | — | — | — | both reject |");
            }
        }
    }
    println!("\nDifferences: {differences}\n");

    println!("## Retained bytes: ijson `IValue` vs simd-json `OwnedValue` (mockalloc over the system allocator)\n");
    println!("| Fixture | JSON bytes | IValue retained (serde_json) | allocs | mem_allocated | IValue retained (simd-json front end) | allocs | OwnedValue retained | allocs | OwnedValue / IValue |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (name, text) in fixtures() {
        let (iv, pi) = measure(|| parse_serde(&text).unwrap());
        let mem = iv.mem_allocated();
        drop(iv);
        let (iv2, ps) = measure(|| parse_simd(&text).unwrap());
        drop(iv2);
        let (iv, pi) = measure(|| parse_serde(&text).unwrap());
        let (ov, po) = measure(|| {
            let mut bytes = text.as_bytes().to_vec();
            simd_json::to_owned_value(&mut bytes).unwrap()
        });
        println!(
            "| {} | {} | {} | {} | {:.0} | {} | {} | {} | {} | {:.2}× |",
            name,
            text.len(),
            pi.mem_leaked(),
            pi.num_allocs(),
            mem,
            ps.mem_leaked(),
            ps.num_allocs(),
            po.mem_leaked(),
            po.num_allocs(),
            po.mem_leaked() as f64 / pi.mem_leaked() as f64
        );
        drop(ov);
        drop(iv);
    }
    println!(
        "\nsize_of: IValue {} B, simd_json::OwnedValue {} B",
        std::mem::size_of::<IValue>(),
        std::mem::size_of::<simd_json::OwnedValue>()
    );
}
