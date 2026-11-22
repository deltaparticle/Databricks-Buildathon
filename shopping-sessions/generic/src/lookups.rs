//! manifest.json, products.csv and campaigns.csv.

use std::path::Path;

use crate::csv;
use crate::hash::FastMap;
use crate::key;
use crate::ts;

pub struct Window {
    pub start: i64,
    pub end: i64,
}

pub fn read_manifest(dir: &Path) -> Window {
    let path = dir.join("manifest.json");
    let text = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_slice(&text).expect("manifest.json is not JSON");
    let field = |name: &str| {
        let value = json[name].as_str().unwrap_or_else(|| panic!("manifest.json has no {name}"));
        ts::parse(value.as_bytes()).unwrap_or_else(|| panic!("manifest.json {name} is not a timestamp: {value}"))
    };
    Window {
        start: field("window_start"),
        end: field("window_end"),
    }
}

/// The well-formed data rows of a small CSV file.
fn small_rows<const N: usize>(path: &Path, mut f: impl FnMut([&[u8]; N])) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let Some(first) = csv::data_start(&data) else {
        return;
    };
    csv::for_each_line(&data, first, first, data.len(), |line| {
        let mut out = [&b""[..]; N];
        if csv::fields::<N>(line, &mut out) {
            f(out);
        }
    });
}

/// An exact map from ID bytes to a small index.
pub struct IdMap {
    packed: FastMap<u64, u32>,
    other: std::collections::HashMap<Vec<u8>, u32>,
}

impl IdMap {
    fn new() -> Self {
        Self {
            packed: FastMap::default(),
            other: std::collections::HashMap::new(),
        }
    }

    fn insert(&mut self, id: &[u8], value: u32) {
        match key::pack(id) {
            Some(k) => {
                self.packed.insert(k, value);
            }
            None => {
                self.other.insert(id.to_vec(), value);
            }
        }
    }

    #[inline]
    pub fn get(&self, id: &[u8]) -> Option<u32> {
        match key::pack(id) {
            Some(k) => self.packed.get(&k).copied(),
            None => self.other.get(id).copied(),
        }
    }
}

pub struct Products {
    map: IdMap,
    /// Category names in byte-wise order; an id is an index here.
    pub categories: Vec<Vec<u8>>,
    pub unknown: u16,
}

impl Products {
    pub fn read(dir: &Path) -> Self {
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        small_rows::<2>(&dir.join("products.csv"), |[product, category]| {
            if !product.is_empty() && !category.is_empty() {
                rows.push((product.to_vec(), category.to_vec()));
            }
        });
        let mut categories: Vec<Vec<u8>> = rows.iter().map(|(_, c)| c.clone()).collect();
        categories.push(b"unknown".to_vec());
        categories.sort_unstable();
        categories.dedup();
        assert!(categories.len() <= u16::MAX as usize, "too many categories");
        let id_of = |name: &[u8]| categories.binary_search_by(|c| c.as_slice().cmp(name)).unwrap() as u16;
        let unknown = id_of(b"unknown");
        let mut map = IdMap::new();
        for (product, category) in &rows {
            map.insert(product, id_of(category) as u32);
        }
        Self { map, categories, unknown }
    }

    #[inline]
    pub fn category(&self, product: &[u8]) -> u16 {
        match self.map.get(product) {
            Some(c) => c as u16,
            None => self.unknown,
        }
    }
}

pub struct Campaigns {
    map: IdMap,
    pub ids: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
}

impl Campaigns {
    pub fn read(dir: &Path) -> Self {
        let mut map = IdMap::new();
        let mut ids = Vec::new();
        let mut channels = Vec::new();
        small_rows::<2>(&dir.join("campaigns.csv"), |[campaign, channel]| {
            if !campaign.is_empty() && map.get(campaign).is_none() {
                map.insert(campaign, ids.len() as u32);
                ids.push(campaign.to_vec());
                channels.push(channel.to_vec());
            }
        });
        Self { map, ids, channels }
    }

    #[inline]
    pub fn get(&self, campaign: &[u8]) -> Option<u32> {
        self.map.get(campaign)
    }
}
