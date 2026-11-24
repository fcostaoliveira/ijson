//! EXP-019: which front end rounds decimal floats correctly? `str::parse::<f64>` (correctly rounded)
//! is the arbiter. Numbers are checked at the root, inside an array and inside an object, because
//! simd-json copies a root-level number into a padded buffer and takes a different code path from
//! numbers embedded in a document.
fn tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == 'E' || c == '+'))
        .filter(|t| (t.contains('.') || t.contains('e') || t.contains('E')) && serde_json::from_str::<f64>(t).is_ok())
        .collect()
}
fn simd_root(s: &str) -> f64 { let mut b = s.as_bytes().to_vec(); simd_json::from_slice(&mut b).unwrap() }
fn simd_arr(s: &str) -> f64 { let mut b = format!("[{s}]").into_bytes(); let v: Vec<f64> = simd_json::from_slice(&mut b).unwrap(); v[0] }
fn simd_obj(s: &str) -> f64 { let mut b = format!("{{\"a\":{s}}}").into_bytes(); let v: std::collections::HashMap<String, f64> = simd_json::from_slice(&mut b).unwrap(); v["a"] }
fn serde_arr(s: &str) -> f64 { let v: Vec<f64> = serde_json::from_str(&format!("[{s}]")).unwrap(); v[0] }
fn main() {
    let fixtures = [
        ("yelp", include_str!("../benches/fixtures/pass-jsonsl-yelp.json")),
        ("yahoo2", include_str!("../benches/fixtures/pass-jsonsl-yahoo2.json")),
        ("gmaps-place", include_str!("../benches/fixtures/api_replies_q5_gmaps_place.json")),
        ("parser-0000", include_str!("../benches/fixtures/parser-0000.json")),
        ("pass-100", include_str!("../benches/fixtures/pass-100.json")),
    ];
    println!("| Fixture | decimal tokens | serde_json ≠ std | simd root ≠ std | simd in-array ≠ std | simd in-object ≠ std | examples (token: std bits → simd in-array bits) |");
    println!("|---|---|---|---|---|---|---|");
    for (name, text) in fixtures {
        let toks = tokens(text);
        let (mut sj, mut r, mut a, mut o) = (0, 0, 0, 0);
        let mut ex = Vec::new();
        for t in &toks {
            let std: f64 = t.parse().unwrap();
            if serde_arr(t).to_bits() != std.to_bits() { sj += 1; }
            if simd_root(t).to_bits() != std.to_bits() { r += 1; }
            let ia = simd_arr(t);
            if ia.to_bits() != std.to_bits() { a += 1; if ex.len() < 3 { ex.push(format!("`{t}`: {:#x} → {:#x} ({})", std.to_bits(), ia.to_bits(), serde_json::to_string(&ia).unwrap())); } }
            if simd_obj(t).to_bits() != std.to_bits() { o += 1; }
        }
        println!("| {name} | {} | {sj} | {r} | {a} | {o} | {} |", toks.len(), ex.join("; "));
    }
    // Pattern probe: does the digit count decide it?
    println!("\n| Input (in array) | std bits | simd bits | match |\n|---|---|---|---|");
    for s in ["3.862266540527344", "3.86226654052734", "3.8622665405273", "41.1770806", "41.177080", "0.1", "1.5", "-122.4026", "7.038531e-26", "1e23"] {
        let std: f64 = s.parse().unwrap(); let si = simd_arr(s);
        println!("| {s} | {:#018x} | {:#018x} | {} |", std.to_bits(), si.to_bits(), std.to_bits() == si.to_bits());
    }
    full_document_check();
}

/// Full-document context: every f64 leaf of the document parsed by serde_json (`serde_json::Value`,
/// the same number parser the ijson path uses), by simd-json (`OwnedValue`), and by std from the
/// text's decimal tokens, compared as multisets of bit patterns.
fn leaves_serde(v: &serde_json::Value, out: &mut Vec<u64>) {
    match v {
        serde_json::Value::Number(n) if n.is_f64() => out.push(n.as_f64().unwrap().to_bits()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| leaves_serde(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| leaves_serde(x, out)),
        _ => {}
    }
}
fn leaves_simd(v: &simd_json::OwnedValue, out: &mut Vec<u64>) {
    use simd_json::prelude::*;
    if let Some(f) = v.as_f64() { if !v.is_i64() && !v.is_u64() { out.push(f.to_bits()); } }
    if let Some(a) = v.as_array() { a.iter().for_each(|x| leaves_simd(x, out)); }
    if let Some(o) = v.as_object() { o.values().for_each(|x| leaves_simd(x, out)); }
}
fn diff(name_a: &str, a: &[u64], name_b: &str, b: &[u64]) {
    let mut a = a.to_vec(); let mut b = b.to_vec(); a.sort(); b.sort();
    let mut only_a = Vec::new(); let mut only_b = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if j == b.len() || (i < a.len() && a[i] < b[j]) { only_a.push(a[i]); i += 1; }
        else if i == a.len() || b[j] < a[i] { only_b.push(b[j]); j += 1; }
        else { i += 1; j += 1; }
    }
    let show = |v: &[u64]| v.iter().take(6).map(|b| format!("{:#x}={}", b, f64::from_bits(*b))).collect::<Vec<_>>().join(", ");
    println!("| {name_a} vs {name_b} | {} / {} | {} | {} | {} | {} |", a.len(), b.len(), only_a.len(), show(&only_a), only_b.len(), show(&only_b));
}
#[allow(dead_code)]
fn full_document_check() {
    println!("\n## Full-document context\n");
    println!("| Comparison | leaves | only in first | values | only in second | values |\n|---|---|---|---|---|---|");
    let fixtures = [
        ("yelp", include_str!("../benches/fixtures/pass-jsonsl-yelp.json")),
        ("yahoo2", include_str!("../benches/fixtures/pass-jsonsl-yahoo2.json")),
        ("gmaps-place", include_str!("../benches/fixtures/api_replies_q5_gmaps_place.json")),
    ];
    for (name, text) in fixtures {
        let sv: serde_json::Value = serde_json::from_str(text).unwrap();
        let mut sl = Vec::new(); leaves_serde(&sv, &mut sl);
        let mut b = text.as_bytes().to_vec();
        let ov = simd_json::to_owned_value(&mut b).unwrap();
        let mut il = Vec::new(); leaves_simd(&ov, &mut il);
        let stdl: Vec<u64> = tokens(text).iter().map(|t| t.parse::<f64>().unwrap().to_bits()).collect();
        diff(&format!("{name}: serde_json"), &sl, "simd-json", &il);
        diff(&format!("{name}: serde_json"), &sl, "std tokens", &stdl);
        diff(&format!("{name}: simd-json"), &il, "std tokens", &stdl);
    }
}
