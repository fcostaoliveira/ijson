//! Memory census: bytes retained by `ijson::IValue` (parsed the way RedisJSON parses `JSON.SET`)
//! versus `serde_json::Value`, per input file, with a breakdown of where the `IValue` bytes go.
//!
//! `cargo run --release --no-default-features --example memory_census -- [--json OUT] <file|synthetic:NAME>...`
//!
//! Prints one CSV row per file to stdout (header first); `--json OUT` also writes the rows as JSON.
//! Progress and skips go to stderr.
//!
//! Method
//! - Counting allocator: `mockalloc` over `System` (same as the memory referee). Bytes are the sizes
//!   *requested* from the allocator (`Layout::size()`), not the allocator's size classes. A second
//!   counter beside it models what jemalloc's default size classes (8, 16, 32, 48, ..., 4 per
//!   doubling above 128) would hold for the same requests; that column is a model, not a measurement.
//! - IValue path: `serde_json::Deserializer::from_str` + `IValueDeserSeed::new(None)
//!   .deserialize_compact_objects` + `end()`, i.e. RedisJSON `ivalue_manager.rs::from_str` with
//!   `Format::JSON` and no FPHA. String cache initialised with `init_shared_string_cache(false)`
//!   (OSS RedisJSON). serde_json's default recursion limit (128) stays on, as in `JSON.SET`
//!   (`limit_depth = true`), so a deeper document is rejected and listed as skipped.
//! - serde_json path: `serde_json::from_str::<serde_json::Value>`, with the features this crate
//!   builds serde_json with (no `preserve_order`, no `arbitrary_precision`: objects are `BTreeMap`s).
//! - String cache: before each file the previous documents are dropped and
//!   `ijson::reinit_shared_string_cache()` replaces the intern table with an empty one, so each file
//!   starts from an empty table and its retained bytes include the table growth it causes. After the
//!   census walk, the table is reinitialised again while the documents are still alive; the bytes
//!   freed by that call are the table's size, reported as `cache_table_bytes`.
//! - Line-delimited inputs (`.ndjson`/`.jsonl`, or a file that is not one JSON value but whose first
//!   line is) are parsed one document per line (one RedisJSON key per line), all kept alive, capped
//!   at `MAX_DOCS` documents. `input_bytes` is what was parsed (the measured lines, each with its
//!   newline); `file_bytes` is the whole file. A single document larger than `MAX_DOC_BYTES` is skipped.
//! - Live allocations = allocations - frees inside the measured window (a realloc counts as one of
//!   each). Peak = highest live byte count inside the window, relative to its start.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicI64, Ordering};

use ijson::array::ArraySliceRef;
use ijson::{DestructuredRef, IValue, IValueDeserSeed};
use mockalloc::{is_recording, record_allocs, Mockalloc};

const MAX_DOCS: usize = 20_000;
const MAX_DOC_BYTES: u64 = 200 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Allocator: mockalloc (measured, requested bytes) + a jemalloc size-class model beside it.

struct Census(Mockalloc<System>);

static JE_LIVE: AtomicI64 = AtomicI64::new(0);

/// jemalloc default size classes (LG_QUANTUM = 4, tiny class 8): 8, 16..128 step 16, then four
/// classes per power of two. Applied as a model only.
fn je_class(size: usize) -> i64 {
    let s = size.max(1);
    let c = if s <= 8 {
        8
    } else if s <= 128 {
        (s + 15) & !15
    } else {
        let lg = usize::BITS - 1 - (s - 1).leading_zeros(); // floor(log2(s - 1))
        let delta = 1usize << (lg - 2);
        (s + delta - 1) & !(delta - 1)
    };
    c as i64
}

unsafe impl GlobalAlloc for Census {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = self.0.alloc(l);
        if !p.is_null() && is_recording() {
            JE_LIVE.fetch_add(je_class(l.size()), Ordering::Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = self.0.alloc_zeroed(l);
        if !p.is_null() && is_recording() {
            JE_LIVE.fetch_add(je_class(l.size()), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if is_recording() {
            JE_LIVE.fetch_sub(je_class(l.size()), Ordering::Relaxed);
        }
        self.0.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = self.0.realloc(p, l, new_size);
        if !q.is_null() && is_recording() {
            JE_LIVE.fetch_add(je_class(new_size) - je_class(l.size()), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Census = Census(Mockalloc(System));

struct Window {
    retained: i64,
    live_allocs: i64,
    peak: u64,
    freed: u64,
    je_retained: i64,
}

fn window(f: impl FnOnce()) -> Window {
    let je0 = JE_LIVE.load(Ordering::Relaxed);
    let info = record_allocs(f);
    Window {
        retained: info.mem_allocated() as i64 - info.mem_freed() as i64,
        live_allocs: info.num_allocs() as i64 - info.num_frees() as i64,
        peak: info.peak_mem(),
        freed: info.mem_freed(),
        je_retained: JE_LIVE.load(Ordering::Relaxed) - je0,
    }
}

// ---------------------------------------------------------------------------------------------
// Structural walk of the parsed IValues. Byte figures use ijson's own layouts (src/object.rs,
// src/array.rs, src/unsafe_string.rs, src/number.rs at this commit); the sum is checked against
// the allocator's retained bytes (`unattributed_bytes` should be 0).

fn pad8(n: usize) -> usize {
    (n + 7) & !7
}
fn obj_layout(cap: usize) -> usize {
    let table = if cap > 8 { 4 * (cap + cap / 4) } else { 0 };
    pad8(8 + 16 * cap + table)
}
fn arr_layout(cap: usize, es: usize) -> usize {
    pad8(8 + cap * es)
}
fn str_layout(len: usize) -> usize {
    pad8(8 + len)
}

#[derive(Default, serde::Serialize, Clone)]
struct Shape {
    objects: u64,
    empty_objects: u64,
    object_entries: u64,
    object_slack_entries: u64,
    objects_with_table: u64,
    object_entry_bytes: u64, // header + len * 16
    object_slack_bytes: u64, // (cap - len) * 16
    object_table_bytes: u64, // u32 hash slots (cap > 8) + padding
    het_arrays: u64,
    empty_arrays: u64,
    het_elements: u64,
    het_used_bytes: u64,  // header + len * 8
    het_slack_bytes: u64, // (cap - len) * 8
    typed_arrays: u64,
    typed_elements: u64,
    typed_used_bytes: u64,
    typed_slack_bytes: u64,
    tiny_typed_arrays: u64, // typed, len <= 4
    tiny_typed_bytes: u64,
    string_refs: u64,
    inline_string_refs: u64,
    heap_string_refs: u64,
    distinct_heap_strings: u64,
    heap_string_bytes: u64,     // distinct heap strings, header + len, padded
    heap_string_payload: u64,   // distinct heap strings, len only
    heap_key_string_bytes: u64, // the part of heap_string_bytes first seen as an object key
    numbers: u64,
    heap_numbers: u64,
    heap_number_bytes: u64,
    scalars_other: u64, // null / bool
}

struct Walker {
    s: Shape,
    seen: HashSet<usize>,
}

impl Walker {
    fn string(&mut self, st: &ijson::IString, is_key: bool) {
        self.s.string_refs += 1;
        let v: &IValue = st.as_ref();
        if v.mem_allocated() > 0.0 {
            self.s.heap_string_refs += 1;
            if self.seen.insert(st.as_str().as_ptr() as usize) {
                let b = str_layout(st.len()) as u64;
                self.s.distinct_heap_strings += 1;
                self.s.heap_string_bytes += b;
                self.s.heap_string_payload += st.len() as u64;
                if is_key {
                    self.s.heap_key_string_bytes += b;
                }
            }
        } else {
            self.s.inline_string_refs += 1;
        }
    }

    fn walk(&mut self, v: &IValue) {
        match v.destructure_ref() {
            DestructuredRef::Null | DestructuredRef::Bool(_) => self.s.scalars_other += 1,
            DestructuredRef::Number(_) => {
                self.s.numbers += 1;
                let m = v.mem_allocated();
                if m > 0.0 {
                    self.s.heap_numbers += 1;
                    self.s.heap_number_bytes += m as u64;
                }
            }
            DestructuredRef::String(st) => self.string(st, false),
            DestructuredRef::Array(a) => {
                let len = a.len() as usize;
                let cap = a.capacity() as usize;
                let slice = a.as_slice();
                let es = match &slice {
                    ArraySliceRef::I8(_) | ArraySliceRef::U8(_) => 1,
                    ArraySliceRef::I16(_)
                    | ArraySliceRef::U16(_)
                    | ArraySliceRef::F16(_)
                    | ArraySliceRef::BF16(_) => 2,
                    ArraySliceRef::I32(_) | ArraySliceRef::U32(_) | ArraySliceRef::F32(_) => 4,
                    _ => 8,
                };
                let (used, slack) = if cap == 0 {
                    (0, 0)
                } else {
                    let total = arr_layout(cap, es);
                    let used = arr_layout(len, es);
                    (used as u64, (total - used) as u64)
                };
                if slice.is_typed() {
                    self.s.typed_arrays += 1;
                    self.s.typed_elements += len as u64;
                    self.s.typed_used_bytes += used;
                    self.s.typed_slack_bytes += slack;
                    if len <= 4 {
                        self.s.tiny_typed_arrays += 1;
                        self.s.tiny_typed_bytes += used + slack;
                    }
                } else {
                    if len == 0 {
                        self.s.empty_arrays += 1;
                    }
                    self.s.het_arrays += 1;
                    self.s.het_elements += len as u64;
                    self.s.het_used_bytes += used;
                    self.s.het_slack_bytes += slack;
                    for item in a.iter() {
                        self.walk(item.as_ref());
                    }
                }
            }
            DestructuredRef::Object(o) => {
                let len = o.len() as usize;
                let cap = o.capacity() as usize;
                self.s.objects += 1;
                self.s.object_entries += len as u64;
                if len == 0 {
                    self.s.empty_objects += 1;
                }
                if cap > 0 {
                    let entries = 8 + 16 * len;
                    let slack = 16 * (cap - len);
                    let total = obj_layout(cap);
                    self.s.object_slack_entries += (cap - len) as u64;
                    self.s.object_entry_bytes += entries as u64;
                    self.s.object_slack_bytes += slack as u64;
                    self.s.object_table_bytes += (total - entries - slack) as u64;
                    if cap > 8 {
                        self.s.objects_with_table += 1;
                    }
                }
                for (k, val) in o.iter() {
                    self.string(k, true);
                    self.walk(val);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs

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

/// The two generated fixtures of `benches/memory.rs`, same generators and seeds.
fn synthetic(name: &str) -> Option<String> {
    match name {
        "floats-1024" => {
            let mut g = Lcg(0x5eed);
            let items: Vec<String> = (0..1024)
                .map(|_| format!("{:.6}", (g.next() % 2_000_000) as f64 / 1000.0 - 1000.0))
                .collect();
            Some(format!("[{}]", items.join(",")))
        }
        "ints-1024" => {
            let mut g = Lcg(0x1d);
            let items: Vec<String> = (0..1024)
                .map(|_| ((g.next() % 200_000) as i64 - 100_000).to_string())
                .collect();
            Some(format!("[{}]", items.join(",")))
        }
        _ => None,
    }
}

enum Input {
    Docs {
        docs: Vec<String>,
        input_bytes: u64,
        ndjson: bool,
        capped: bool,
    },
    Skip(String),
}

fn is_one_value(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

fn read_lines(path: &str) -> std::io::Result<(Vec<String>, u64, bool)> {
    let f = std::fs::File::open(path)?;
    let mut docs = Vec::new();
    let mut bytes = 0u64;
    let mut capped = false;
    for line in BufReader::new(f).lines() {
        let line = line?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if docs.len() == MAX_DOCS {
            capped = true;
            break;
        }
        bytes += line.len() as u64 + 1;
        docs.push(t.to_string());
    }
    Ok((docs, bytes, capped))
}

fn load(arg: &str) -> Input {
    if let Some(name) = arg.strip_prefix("synthetic:") {
        return match synthetic(name) {
            Some(s) => Input::Docs {
                input_bytes: s.len() as u64,
                docs: vec![s],
                ndjson: false,
                capped: false,
            },
            None => Input::Skip(format!("unknown synthetic fixture {name}")),
        };
    }
    let meta = match std::fs::metadata(arg) {
        Ok(m) => m,
        Err(e) => return Input::Skip(format!("unreadable: {e}")),
    };
    let lower = arg.to_ascii_lowercase();
    let lines = |why: &str| match read_lines(arg) {
        Ok((docs, input_bytes, capped)) => {
            if docs.iter().all(|d| is_one_value(d)) {
                Input::Docs {
                    docs,
                    input_bytes,
                    ndjson: true,
                    capped,
                }
            } else {
                Input::Skip(format!("{why}, but a line is not one JSON value"))
            }
        }
        Err(e) => Input::Skip(format!("unreadable: {e}")),
    };
    if lower.ends_with(".ndjson") || lower.ends_with(".jsonl") {
        return lines("line-delimited by name");
    }
    if meta.len() > MAX_DOC_BYTES {
        // Only a line-delimited file can be measured; a single document this large is skipped.
        let first = std::fs::File::open(arg)
            .ok()
            .and_then(|f| BufReader::new(f).lines().next())
            .and_then(|l| l.ok())
            .unwrap_or_default();
        if !first.trim().is_empty() && is_one_value(first.trim()) {
            return lines("line-delimited by content");
        }
        return Input::Skip(format!(
            "single document of {} bytes > {} byte limit",
            meta.len(),
            MAX_DOC_BYTES
        ));
    }
    let text = match std::fs::read_to_string(arg) {
        Ok(t) => t,
        Err(e) => return Input::Skip(format!("unreadable: {e}")),
    };
    if is_one_value(&text) {
        return Input::Docs {
            input_bytes: text.len() as u64,
            docs: vec![text],
            ndjson: false,
            capped: false,
        };
    }
    let first = text.lines().map(str::trim).find(|l| !l.is_empty());
    if first.map_or(false, is_one_value) {
        return lines("line-delimited by content");
    }
    Input::Skip("not valid JSON (serde_json rejects it)".into())
}

// ---------------------------------------------------------------------------------------------

fn parse_redisjson(s: &str) -> Result<IValue, serde_json::Error> {
    // RedisJSON redis_json/src/ivalue_manager.rs `from_str`, Format::JSON, fpha = None.
    let mut de = serde_json::Deserializer::from_str(s);
    let v = IValueDeserSeed::new(None).deserialize_compact_objects(&mut de)?;
    de.end()?;
    Ok(v)
}

#[derive(serde::Serialize)]
struct Row {
    file: String,
    ndjson: bool,
    docs: u64,
    capped: bool,
    file_bytes: u64,
    input_bytes: u64,
    ivalue_retained: i64,
    ivalue_live_allocs: i64,
    ivalue_parse_peak: u64,
    ivalue_je_model: i64,
    ivalue_mem_allocated: f64,
    cache_table_bytes: u64,
    serde_retained: i64,
    serde_live_allocs: i64,
    serde_parse_peak: u64,
    serde_je_model: i64,
    unattributed_bytes: i64,
    #[serde(flatten)]
    shape: Shape,
}

const CSV_COLS: &[&str] = &[
    "file",
    "ndjson",
    "docs",
    "capped",
    "file_bytes",
    "input_bytes",
    "ivalue_retained",
    "ivalue_live_allocs",
    "ivalue_parse_peak",
    "ivalue_je_model",
    "ivalue_mem_allocated",
    "cache_table_bytes",
    "serde_retained",
    "serde_live_allocs",
    "serde_parse_peak",
    "serde_je_model",
    "unattributed_bytes",
    "objects",
    "empty_objects",
    "object_entries",
    "object_slack_entries",
    "objects_with_table",
    "object_entry_bytes",
    "object_slack_bytes",
    "object_table_bytes",
    "het_arrays",
    "empty_arrays",
    "het_elements",
    "het_used_bytes",
    "het_slack_bytes",
    "typed_arrays",
    "typed_elements",
    "typed_used_bytes",
    "typed_slack_bytes",
    "tiny_typed_arrays",
    "tiny_typed_bytes",
    "string_refs",
    "inline_string_refs",
    "heap_string_refs",
    "distinct_heap_strings",
    "heap_string_bytes",
    "heap_string_payload",
    "heap_key_string_bytes",
    "numbers",
    "heap_numbers",
    "heap_number_bytes",
    "scalars_other",
];

fn csv_line(row: &Row) -> String {
    let v = serde_json::to_value(row).unwrap();
    CSV_COLS
        .iter()
        .map(|c| match &v[*c] {
            serde_json::Value::String(s) => format!("\"{}\"", s.replace('"', "\"\"")),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn main() {
    let mut json_out: Option<String> = None;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--json" {
            json_out = args.next();
        } else {
            files.push(a);
        }
    }
    // OSS RedisJSON: init_shared_string_cache(is_bigredis = false).
    ijson::init_shared_string_cache(false).expect("string cache already initialised");

    println!("{}", CSV_COLS.join(","));
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for f in &files {
        let (docs, input_bytes, ndjson, capped) = match load(f) {
            Input::Docs {
                docs,
                input_bytes,
                ndjson,
                capped,
            } => (docs, input_bytes, ndjson, capped),
            Input::Skip(why) => {
                eprintln!("SKIP {f}: {why}");
                skipped.push((f.clone(), why));
                continue;
            }
        };

        // (a) IValue via RedisJSON's parse path, from an empty intern table.
        ijson::reinit_shared_string_cache();
        let mut ivals: Vec<IValue> = Vec::with_capacity(docs.len());
        let mut err = None;
        let iw = window(|| {
            for d in &docs {
                match parse_redisjson(d) {
                    Ok(v) => ivals.push(v),
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
        });
        if let Some(e) = err {
            drop(ivals);
            eprintln!("SKIP {f}: ijson parse error: {e}");
            skipped.push((f.clone(), format!("ijson parse error: {e}")));
            continue;
        }
        let mem_allocated: f64 = ivals.iter().map(IValue::mem_allocated).sum();
        let mut w = Walker {
            s: Shape::default(),
            seen: HashSet::new(),
        };
        for v in &ivals {
            w.walk(v);
        }
        drop(w.seen);
        let shape = w.s;
        // Intern-table size: reinitialising with the documents alive frees exactly the table.
        let tw = window(ijson::reinit_shared_string_cache);
        let cache_table_bytes = tw.freed;
        drop(ivals);

        // (b) serde_json::Value.
        let mut svals: Vec<serde_json::Value> = Vec::with_capacity(docs.len());
        let sw = window(|| {
            for d in &docs {
                svals.push(serde_json::from_str(d).unwrap());
            }
        });
        drop(svals);

        let s = &shape;
        let attributed = s.object_entry_bytes
            + s.object_slack_bytes
            + s.object_table_bytes
            + s.het_used_bytes
            + s.het_slack_bytes
            + s.typed_used_bytes
            + s.typed_slack_bytes
            + s.heap_string_bytes
            + s.heap_number_bytes
            + cache_table_bytes;
        let row = Row {
            file: f.clone(),
            ndjson,
            docs: docs.len() as u64,
            capped,
            file_bytes: std::fs::metadata(f).map_or(input_bytes, |m| m.len()),
            input_bytes,
            ivalue_retained: iw.retained,
            ivalue_live_allocs: iw.live_allocs,
            ivalue_parse_peak: iw.peak,
            ivalue_je_model: iw.je_retained,
            ivalue_mem_allocated: mem_allocated,
            cache_table_bytes,
            serde_retained: sw.retained,
            serde_live_allocs: sw.live_allocs,
            serde_parse_peak: sw.peak,
            serde_je_model: sw.je_retained,
            unattributed_bytes: iw.retained - attributed as i64,
            shape,
        };
        println!("{}", csv_line(&row));
        eprintln!(
            "{f}: docs {} input {} ivalue {} serde {} ratio {:.3}",
            row.docs,
            row.input_bytes,
            row.ivalue_retained,
            row.serde_retained,
            row.ivalue_retained as f64 / row.serde_retained as f64
        );
        rows.push(row);
    }
    if let Some(p) = json_out {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            max_docs: usize,
            max_doc_bytes: u64,
            rows: &'a [Row],
            skipped: &'a [(String, String)],
        }
        let out = Out {
            max_docs: MAX_DOCS,
            max_doc_bytes: MAX_DOC_BYTES,
            rows: &rows,
            skipped: &skipped,
        };
        std::fs::write(&p, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
}
