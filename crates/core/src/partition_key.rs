//! 3.x partition key 的固定逻辑桶；只影响带 key 的 schema。

use crate::schema::CollectionSchema;
use crate::storage::manifest::Manifest;

pub(crate) const BUCKETS: usize = 16;

pub(crate) fn bucket(text: &str) -> usize {
    // 大端无符号 MD5 mod 16 只取摘要最后四位。
    usize::from(md5::compute(text.as_bytes()).0[15] & 15)
}

pub(crate) fn name(bucket: usize) -> String {
    format!("_pk_{bucket}")
}

pub(crate) fn register(schema: &CollectionSchema, manifest: &mut Manifest) {
    if schema.partition_key().is_none() {
        return;
    }
    for bucket in 0..BUCKETS {
        let name = name(bucket);
        if !manifest.partitions.contains_key(&name) {
            manifest.partition_order.push(name.clone());
            manifest.partitions.insert(name, Default::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_hashes() {
        for (text, expected) in [("0", 10), ("1", 11), ("", 14), ("None", 4)] {
            assert_eq!(bucket(text), expected);
        }
    }
}
