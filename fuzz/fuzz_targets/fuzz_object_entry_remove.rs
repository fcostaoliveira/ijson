#![no_main]

use arbitrary::Arbitrary;
use ijson::{IObject, IValue};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

// A key space that reliably crosses the small-object threshold (8) into the
// hash table and back, mixing inline (<= 7 byte) and heap keys so interning,
// byte comparison and Robin-Hood displacement all run. `id` selects the key.
#[derive(Arbitrary, Debug)]
enum Op {
    Insert(u8, u64),
    Remove(u8),
    Get(u8),
    // Goes through `entry(...).or_insert(...)` (the EXP-015 borrowed-probe path).
    EntryOrInsert(u8, u64),
    // Goes through `entry(...).and_modify(...)`.
    EntryAndModify(u8, u64),
    // Removes everything, exercising clear/drop of a populated table.
    Clear,
}

fn key_of(id: u8) -> String {
    match id % 4 {
        0 => format!("k{id}"),                                   // inline short
        1 => format!("field_{id}"),                              // inline/edge
        _ => format!("a_long_adversarial_object_key_number_{id}"), // heap
    }
}

fuzz_target!(|ops: Vec<Op>| {
    let mut obj = IObject::new();
    let mut model: BTreeMap<u8, u64> = BTreeMap::new();

    for op in ops {
        match op {
            Op::Insert(id, v) => {
                let prev = obj
                    .insert(key_of(id).as_str(), IValue::from(v))
                    .unwrap()
                    .and_then(|old| old.to_u64());
                assert_eq!(prev, model.insert(id, v));
            }
            Op::Remove(id) => {
                let removed = obj.remove(key_of(id).as_str()).and_then(|v| v.to_u64());
                assert_eq!(removed, model.remove(&id));
            }
            Op::Get(id) => {
                let got = obj.get(key_of(id).as_str()).and_then(|v| v.to_u64());
                assert_eq!(got, model.get(&id).copied());
            }
            Op::EntryOrInsert(id, v) => {
                let k = key_of(id);
                let got = obj
                    .entry(k.as_str())
                    .unwrap()
                    .or_insert(IValue::from(v))
                    .to_u64();
                let expected = *model.entry(id).or_insert(v);
                assert_eq!(got, Some(expected));
            }
            Op::EntryAndModify(id, v) => {
                let k = key_of(id);
                obj.entry(k.as_str())
                    .unwrap()
                    .and_modify(|slot| *slot = IValue::from(v));
                if let Some(cur) = model.get_mut(&id) {
                    *cur = v;
                }
            }
            Op::Clear => {
                obj.clear();
                model.clear();
            }
        }
        assert_eq!(obj.len() as usize, model.len());
    }

    // Final agreement: every model key present with the right value, and no
    // extra keys (via len equality above) survive in the object.
    for (id, v) in &model {
        assert_eq!(
            obj.get(key_of(*id).as_str()).and_then(|x| x.to_u64()),
            Some(*v)
        );
    }
    // Insertion/iteration order has as many entries as the model.
    assert_eq!(obj.keys().count(), model.len());
});
