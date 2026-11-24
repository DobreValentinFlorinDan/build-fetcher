//! A stored `TimelineCapture` read straight from its JSON text into the struct, without building a `serde_json::Value`
//! tree first (which took about a sixth of an evaluation: every game's timeline is read when the games are prepared).
//! As lenient as `TimelineCapture::from_json`: a missing or null number is 0, a row that is not an array is zeros, a
//! field of another type is empty. Anything it can't read this way (text that isn't an object, a key twice) goes back
//! to the `Value` path, so the result is always the same; `tests/capture.rs` compares the two.

use crate::capture::{Kill, TimelineCapture};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

/// What `visit_*` gives for a value that is not what the reader wants: consumed, and the default kept.
macro_rules! lenient {
    () => {
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
}

/// A number; anything else (null, a string, an array, an object) is None.
struct Num(Option<f64>);
impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<f64>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            lenient!();
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(Some(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(Some(v as f64))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(Some(v as f64))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        d.deserialize_any(V).map(Num)
    }
}
impl Num {
    fn or0(self) -> f64 {
        self.0.unwrap_or(0.0)
    }
}

/// A string; anything else is "".
#[derive(Default)]
struct Text(String);
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = String;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            lenient!();
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(v.to_string())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(String::new())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(String::new())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(String::new())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(String::new())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(String::new())
            }
        }
        d.deserialize_any(V).map(Text)
    }
}

/// An array read by `seq`; anything else gives `T::default()`. `seq` must leave the array fully read.
struct ArrayOr<T, F>(F, std::marker::PhantomData<T>);
impl<'de, T: Default, F: FnOnce(&mut dyn ErasedSeq<'de>) -> Result<T, String>> Visitor<'de> for ArrayOr<T, F> {
    type Value = T;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("anything")
    }
    lenient!();
    fn visit_f64<E>(self, _: f64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_i64<E>(self, _: i64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_u64<E>(self, _: u64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_str<E>(self, _: &str) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<T, A::Error> {
        while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(T::default())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<T, A::Error> {
        let mut s = Seq(a);
        let out = (self.0)(&mut s).map_err(serde::de::Error::custom)?;
        s.rest().map_err(serde::de::Error::custom)?;
        Ok(out)
    }
}

/// The elements of an array, one at a time, by kind (object-safe, so the readers below are plain closures).
trait ErasedSeq<'de> {
    /// The next element as a number: None at the end of the array, Some(None) for an element that isn't a number.
    fn num(&mut self) -> Result<Option<Option<f64>>, String>;
    fn text(&mut self) -> Result<String, String>;
    fn nums(&mut self) -> Result<Vec<f64>, String>;
    fn rest(&mut self) -> Result<(), String>;
}
struct Seq<A>(A);
impl<'de, A: SeqAccess<'de>> ErasedSeq<'de> for Seq<A> {
    fn num(&mut self) -> Result<Option<Option<f64>>, String> {
        self.0.next_element::<Num>().map(|n| n.map(|n| n.0)).map_err(|e| e.to_string())
    }
    fn text(&mut self) -> Result<String, String> {
        self.0.next_element::<Text>().map(|t| t.unwrap_or_default().0).map_err(|e| e.to_string())
    }
    fn nums(&mut self) -> Result<Vec<f64>, String> {
        self.0.next_element::<Nums>().map(|n| n.map(|n| n.0).unwrap_or_default()).map_err(|e| e.to_string())
    }
    fn rest(&mut self) -> Result<(), String> {
        while self.0.next_element::<IgnoredAny>().map_err(|e| e.to_string())?.is_some() {}
        Ok(())
    }
}

/// The next element as a number, 0 when it is missing or not a number.
fn n0<'de>(s: &mut dyn ErasedSeq<'de>) -> Result<f64, String> {
    Ok(s.num()?.flatten().unwrap_or(0.0))
}

fn array_or<'de, D: Deserializer<'de>, T: Default>(d: D, f: impl FnOnce(&mut dyn ErasedSeq<'de>) -> Result<T, String>) -> Result<T, D::Error> {
    d.deserialize_any(ArrayOr(f, std::marker::PhantomData))
}

/// [n, n, ...]: every element a number or 0.
#[derive(Default)]
struct Nums(Vec<f64>);
impl<'de> Deserialize<'de> for Nums {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        array_or(d, |s| {
            let mut v = Vec::new();
            while let Some(x) = s.num()? {
                v.push(x.unwrap_or(0.0));
            }
            Ok(Nums(v))
        })
    }
}

/// A row of K numbers (missing ones 0, more ignored); a row that isn't an array is zeros.
struct Row<const K: usize>([f64; K]);
impl<const K: usize> Default for Row<K> {
    fn default() -> Self {
        Row([0.0; K])
    }
}
impl<'de, const K: usize> Deserialize<'de> for Row<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        array_or(d, |s| {
            let mut r = [0.0; K];
            for x in r.iter_mut() {
                *x = n0(s)?;
            }
            Ok(Row(r))
        })
    }
}

/// An array of T (each T lenient by itself); anything else is empty.
struct List<T>(Vec<T>);
impl<T> Default for List<T> {
    fn default() -> Self {
        List(Vec::new())
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Vec<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            lenient!();
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Vec::new())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut v = Vec::with_capacity(a.size_hint().unwrap_or(0).min(4096));
                while let Some(x) = a.next_element::<T>()? {
                    v.push(x);
                }
                Ok(v)
            }
        }
        d.deserialize_any(V(std::marker::PhantomData)).map(List)
    }
}

/// {"1": [row, ...], ...} by participant id; keys that aren't ids are skipped, a later key replaces an earlier one.
struct ByPid<const K: usize>(BTreeMap<u32, Vec<[f64; K]>>);
impl<const K: usize> Default for ByPid<K> {
    fn default() -> Self {
        ByPid(BTreeMap::new())
    }
}
impl<'de, const K: usize> Deserialize<'de> for ByPid<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const K: usize>;
        impl<'de, const K: usize> Visitor<'de> for V<K> {
            type Value = BTreeMap<u32, Vec<[f64; K]>>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            lenient!();
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(BTreeMap::new())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(BTreeMap::new())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(BTreeMap::new())
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(BTreeMap::new())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BTreeMap::new())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut m = BTreeMap::new();
                while let Some(key) = a.next_key::<std::borrow::Cow<str>>()? {
                    match key.parse::<u32>() {
                        Ok(pid) => {
                            let rows: List<Row<K>> = a.next_value()?;
                            m.insert(pid, rows.0.into_iter().map(|r| r.0).collect());
                        }
                        Err(_) => {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(m)
            }
        }
        d.deserialize_any(V::<K>).map(ByPid)
    }
}

/// [t, pid, kind, item, after?]: `after` is Some when the row has a fifth element (0 when that isn't a number).
#[derive(Default)]
struct ItemRow((f64, f64, f64, f64, Option<f64>));
impl<'de> Deserialize<'de> for ItemRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        array_or(d, |s| {
            let (t, p, k, i) = (n0(s)?, n0(s)?, n0(s)?, n0(s)?);
            let after = s.num()?.map(|a| a.unwrap_or(0.0));
            Ok(ItemRow((t, p, k, i, after)))
        })
    }
}

/// [t, killer, victim, [assists], bounty, shutdown].
#[derive(Default)]
struct KillRow(Kill);
impl<'de> Deserialize<'de> for KillRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        array_or(d, |s| {
            let (t, killer, victim) = (n0(s)?, n0(s)?, n0(s)?);
            let assists = s.nums()?;
            Ok(KillRow(Kill { t, killer, victim, assists, bounty: n0(s)?, shutdown: n0(s)? }))
        })
    }
}

/// [t, team, kind, subtype].
#[derive(Default)]
struct ObjectiveRow((f64, f64, String, String));
impl<'de> Deserialize<'de> for ObjectiveRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        array_or(d, |s| Ok(ObjectiveRow((n0(s)?, n0(s)?, s.text()?, s.text()?))))
    }
}

struct Capture;
impl<'de> DeserializeSeed<'de> for Capture {
    type Value = TimelineCapture;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<TimelineCapture, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = TimelineCapture;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a timeline capture object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<TimelineCapture, A::Error> {
                let mut c = TimelineCapture::default();
                let mut seen = 0u8;
                while let Some(key) = a.next_key::<std::borrow::Cow<str>>()? {
                    let bit = match &*key {
                        "interval" => 1,
                        "t" => 2,
                        "frames" => 4,
                        "pos" => 8,
                        "items" => 16,
                        "kills" => 32,
                        "objectives" => 64,
                        _ => 0,
                    };
                    if seen & bit != 0 {
                        return Err(serde::de::Error::custom("a key twice")); // the Value path decides
                    }
                    seen |= bit;
                    match bit {
                        1 => c.interval = a.next_value::<Num>()?.or0(),
                        2 => c.t = a.next_value::<Nums>()?.0,
                        4 => c.frames = a.next_value::<ByPid<5>>()?.0,
                        8 => c.pos = a.next_value::<ByPid<2>>()?.0,
                        16 => c.items = a.next_value::<List<ItemRow>>()?.0.into_iter().map(|r| r.0).collect(),
                        32 => c.kills = a.next_value::<List<KillRow>>()?.0.into_iter().map(|r| r.0).collect(),
                        64 => c.objectives = a.next_value::<List<ObjectiveRow>>()?.0.into_iter().map(|r| r.0).collect(),
                        _ => {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(c)
            }
        }
        d.deserialize_map(V)
    }
}

/// The capture in `text`, or None when this reader can't take it (the caller then reads it through `Value`).
pub fn read(text: &str) -> Option<TimelineCapture> {
    let mut de = serde_json::Deserializer::from_str(text);
    let c = Capture.deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(c)
}
