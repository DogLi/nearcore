use borsh::{BorshDeserialize, BorshSerialize};

use near_primitives_core::hash::{hash, CryptoHash};
use near_schema_checker_lib::ProtocolSchema;

/// State value reference. Used to charge fees for value length before retrieving the value itself.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, ProtocolSchema)]
pub struct ValueRef {
    /// Value length in bytes.
    pub length: u32,
    /// Unique value hash.
    pub hash: CryptoHash,
}

impl ValueRef {
    /// Create serialized value reference by the value.
    /// Resulting array stores 4 bytes of length and then 32 bytes of hash.
    /// TODO (#7327): consider passing hash here to avoid double computation
    pub fn new(value: &[u8]) -> Self {
        Self { length: value.len() as u32, hash: hash(value) }
    }

    /// Decode value reference from the raw byte array.
    pub fn decode(bytes: &[u8; 36]) -> Self {
        let (length, hash) = stdx::split_array(bytes);
        let length = u32::from_le_bytes(*length);
        ValueRef { length, hash: CryptoHash(*hash) }
    }

    /// Returns length of the referenced value.
    pub fn len(&self) -> usize {
        usize::try_from(self.length).unwrap()
    }
}

impl std::cmp::PartialEq<[u8]> for ValueRef {
    fn eq(&self, rhs: &[u8]) -> bool {
        self.len() == rhs.len() && self.hash == CryptoHash::hash_bytes(rhs)
    }
}

impl std::fmt::Debug for ValueRef {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "({}, {})", self.length, self.hash)
    }
}

#[cfg(test)]
mod tests {
    use crate::state::ValueRef;
    use near_primitives_core::hash::hash;

    #[test]
    fn test_encode_decode() {
        let value = vec![1, 2, 3];
        let old_value_ref = ValueRef::new(&value);
        let mut value_ref_ser = [0u8; 36];
        value_ref_ser[0..4].copy_from_slice(&old_value_ref.length.to_le_bytes());
        value_ref_ser[4..36].copy_from_slice(&old_value_ref.hash.0);
        let value_ref = ValueRef::decode(&value_ref_ser);
        assert_eq!(value_ref.length, value.len() as u32);
        assert_eq!(value_ref.hash, hash(&value));
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, ProtocolSchema)]
pub enum FlatStateValue {
    Ref(ValueRef),
    Inlined(Vec<u8>),
}

impl FlatStateValue {
    pub const INLINE_DISK_VALUE_THRESHOLD: usize =
        near_primitives_core::config::INLINE_DISK_VALUE_THRESHOLD;

    pub fn on_disk(value: &[u8]) -> Self {
        if value.len() <= Self::INLINE_DISK_VALUE_THRESHOLD {
            Self::inlined(value)
        } else {
            Self::value_ref(value)
        }
    }

    pub fn value_ref(value: &[u8]) -> Self {
        Self::Ref(ValueRef::new(value))
    }

    pub fn inlined(value: &[u8]) -> Self {
        Self::Inlined(value.to_vec())
    }

    pub fn to_value_ref(&self) -> ValueRef {
        match self {
            Self::Ref(value_ref) => *value_ref,
            Self::Inlined(value) => ValueRef::new(value),
        }
    }

    pub fn value_len(&self) -> usize {
        match self {
            Self::Ref(value_ref) => value_ref.len(),
            Self::Inlined(value) => value.len(),
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Self::Ref(_) => size_of::<Self>(),
            Self::Inlined(value) => size_of::<Self>() + value.capacity(),
        }
    }
}

/// Value to insert to trie or update existing value in the trie.
#[derive(Debug, Clone)]
pub enum GenericTrieValue {
    /// Value to update both memtrie and trie storage. Full value is required
    /// for that.
    MemtrieAndDisk(Vec<u8>),
    /// Value to update only memtrie. In such case it is enough to have a
    /// `FlatStateValue`.
    MemtrieOnly(FlatStateValue),
}
