use alloc::{collections::BTreeMap, vec::Vec};

use super::{EmptySubtreeRoots, InnerNodeInfo, MerkleError, MerklePath, NodeIndex};
use crate::{
    hash::rpo::{Rpo256, RpoDigest},
    Felt, Word, EMPTY_WORD,
};

mod full;
pub use full::{Smt, SmtLeaf, SmtLeafError, SmtProof, SmtProofError, SMT_DEPTH};

mod simple;
pub use simple::SimpleSmt;

// CONSTANTS
// ================================================================================================

/// Minimum supported depth.
pub const SMT_MIN_DEPTH: u8 = 1;

/// Maximum supported depth.
pub const SMT_MAX_DEPTH: u8 = 64;

// SPARSE MERKLE TREE
// ================================================================================================

/// An abstract description of a sparse Merkle tree.
///
/// A sparse Merkle tree is a key-value map which also supports proving that a given value is indeed
/// stored at a given key in the tree. It is viewed as always being fully populated. If a leaf's
/// value was not explicitly set, then its value is the default value. Typically, the vast majority
/// of leaves will store the default value (hence it is "sparse"), and therefore the internal
/// representation of the tree will only keep track of the leaves that have a different value from
/// the default.
///
/// All leaves sit at the same depth. The deeper the tree, the more leaves it has; but also the
/// longer its proofs are - of exactly `log(depth)` size. A tree cannot have depth 0, since such a
/// tree is just a single value, and is probably a programming mistake.
///
/// Every key maps to one leaf. If there are as many keys as there are leaves, then
/// [Self::Leaf] should be the same type as [Self::Value], as is the case with
/// [crate::merkle::SimpleSmt]. However, if there are more keys than leaves, then [`Self::Leaf`]
/// must accommodate all keys that map to the same leaf.
///
/// [SparseMerkleTree] currently doesn't support optimizations that compress Merkle proofs.
pub(crate) trait SparseMerkleTree<const DEPTH: u8> {
    /// The type for a key
    type Key: Clone + Ord;
    /// The type for a value
    type Value: Clone + PartialEq;
    /// The type for a leaf
    type Leaf: Clone;
    /// The type for an opening (i.e. a "proof") of a leaf
    type Opening;

    /// The default value used to compute the hash of empty leaves
    const EMPTY_VALUE: Self::Value;

    /// The root of the empty tree with provided DEPTH
    const EMPTY_ROOT: RpoDigest;

    // PROVIDED METHODS
    // ---------------------------------------------------------------------------------------------

    /// Returns an opening of the leaf associated with `key`. Conceptually, an opening is a Merkle
    /// path to the leaf, as well as the leaf itself.
    fn open(&self, key: &Self::Key) -> Self::Opening {
        let leaf = self.get_leaf(key);

        let mut index: NodeIndex = {
            let leaf_index: LeafIndex<DEPTH> = Self::key_to_leaf_index(key);
            leaf_index.into()
        };

        let merkle_path = {
            let mut path = Vec::with_capacity(index.depth() as usize);
            for _ in 0..index.depth() {
                let is_right = index.is_value_odd();
                index.move_up();
                let InnerNode { left, right } = self.get_inner_node(index);
                let value = if is_right { left } else { right };
                path.push(value);
            }

            MerklePath::new(path)
        };

        Self::path_and_leaf_to_opening(merkle_path, leaf)
    }

    /// Inserts a value at the specified key, returning the previous value associated with that key.
    /// Recall that by definition, any key that hasn't been updated is associated with
    /// [`Self::EMPTY_VALUE`].
    ///
    /// This also recomputes all hashes between the leaf (associated with the key) and the root,
    /// updating the root itself.
    fn insert(&mut self, key: Self::Key, value: Self::Value) -> Self::Value {
        let old_value = self.insert_value(key.clone(), value.clone()).unwrap_or(Self::EMPTY_VALUE);

        // if the old value and new value are the same, there is nothing to update
        if value == old_value {
            return value;
        }

        let leaf = self.get_leaf(&key);
        let node_index = {
            let leaf_index: LeafIndex<DEPTH> = Self::key_to_leaf_index(&key);
            leaf_index.into()
        };

        self.recompute_nodes_from_index_to_root(node_index, Self::hash_leaf(&leaf));

        old_value
    }

    /// Recomputes the branch nodes (including the root) from `index` all the way to the root.
    /// `node_hash_at_index` is the hash of the node stored at index.
    fn recompute_nodes_from_index_to_root(
        &mut self,
        mut index: NodeIndex,
        node_hash_at_index: RpoDigest,
    ) {
        let mut node_hash = node_hash_at_index;
        for node_depth in (0..index.depth()).rev() {
            let is_right = index.is_value_odd();
            index.move_up();
            let InnerNode { left, right } = self.get_inner_node(index);
            let (left, right) = if is_right {
                (left, node_hash)
            } else {
                (node_hash, right)
            };
            node_hash = Rpo256::merge(&[left, right]);

            if node_hash == *EmptySubtreeRoots::entry(DEPTH, node_depth) {
                // If a subtree is empty, then can remove the inner node, since it's equal to the
                // default value
                self.remove_inner_node(index)
            } else {
                self.insert_inner_node(index, InnerNode { left, right });
            }
        }
        self.set_root(node_hash);
    }

    /// Computes what changes are necessary to insert the specified key-value pairs into this Merkle
    /// tree, allowing for validation before applying those changes.
    ///
    /// This method returns a [`MutationSet`], which contains all the information for inserting
    /// `kv_pairs` into this Merkle tree already calculated, including the new root hash, which can
    /// be queried with [`MutationSet::root()`]. Once a mutation set is returned,
    /// [`SparseMerkleTree::apply_mutations()`] can be called in order to commit these changes to
    /// the Merkle tree, or [`drop()`] to discard them.
    fn compute_mutations(
        &self,
        kv_pairs: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> MutationSet<DEPTH, Self::Key, Self::Value> {
        use NodeMutation::*;

        let mut new_root = self.root();
        let mut new_pairs: BTreeMap<Self::Key, Self::Value> = Default::default();
        let mut node_mutations: BTreeMap<NodeIndex, NodeMutation> = Default::default();

        for (key, value) in kv_pairs {
            // If the old value and the new value are the same, there is nothing to update.
            // For the unusual case that kv_pairs has multiple values at the same key, we'll have
            // to check the key-value pairs we've already seen to get the "effective" old value.
            let old_value = new_pairs.get(&key).cloned().unwrap_or_else(|| self.get_value(&key));
            if value == old_value {
                continue;
            }

            let leaf_index = Self::key_to_leaf_index(&key);
            let mut node_index = NodeIndex::from(leaf_index);

            // We need the current leaf's hash to calculate the new leaf, but in the rare case that
            // `kv_pairs` has multiple pairs that go into the same leaf, then those pairs are also
            // part of the "current leaf".
            let old_leaf = {
                let pairs_at_index = new_pairs
                    .iter()
                    .filter(|&(new_key, _)| Self::key_to_leaf_index(new_key) == leaf_index);

                pairs_at_index.fold(self.get_leaf(&key), |acc, (k, v)| {
                    // Most of the time `pairs_at_index` should only contain a single entry (or
                    // none at all), as multi-leaves should be really rare.
                    let existing_leaf = acc.clone();
                    self.construct_prospective_leaf(existing_leaf, k, v)
                })
            };

            let new_leaf = self.construct_prospective_leaf(old_leaf, &key, &value);

            let mut new_child_hash = Self::hash_leaf(&new_leaf);

            for node_depth in (0..node_index.depth()).rev() {
                // Whether the node we're replacing is the right child or the left child.
                let is_right = node_index.is_value_odd();
                node_index.move_up();

                let old_node = node_mutations
                    .get(&node_index)
                    .map(|mutation| match mutation {
                        Addition(node) => node.clone(),
                        Removal => EmptySubtreeRoots::get_inner_node(DEPTH, node_depth),
                    })
                    .unwrap_or_else(|| self.get_inner_node(node_index));

                let new_node = if is_right {
                    InnerNode {
                        left: old_node.left,
                        right: new_child_hash,
                    }
                } else {
                    InnerNode {
                        left: new_child_hash,
                        right: old_node.right,
                    }
                };

                // The next iteration will operate on this new node's hash.
                new_child_hash = new_node.hash();

                let &equivalent_empty_hash = EmptySubtreeRoots::entry(DEPTH, node_depth);
                let is_removal = new_child_hash == equivalent_empty_hash;
                let new_entry = if is_removal { Removal } else { Addition(new_node) };
                node_mutations.insert(node_index, new_entry);
            }

            // Once we're at depth 0, the last node we made is the new root.
            new_root = new_child_hash;
            // And then we're done with this pair; on to the next one.
            new_pairs.insert(key, value);
        }

        MutationSet {
            old_root: self.root(),
            new_root,
            node_mutations,
            new_pairs,
        }
    }

    /// Apply the prospective mutations computed with [`SparseMerkleTree::compute_mutations()`] to
    /// this tree.
    ///
    /// # Errors
    /// If `mutations` was computed on a tree with a different root than this one, returns
    /// [`MerkleError::ConflictingRoots`] with a two-item [`Vec`]. The first item is the root hash
    /// the `mutations` were computed against, and the second item is the actual current root of
    /// this tree.
    fn apply_mutations(
        &mut self,
        mutations: MutationSet<DEPTH, Self::Key, Self::Value>,
    ) -> Result<(), MerkleError>
    where
        Self: Sized,
    {
        use NodeMutation::*;
        let MutationSet {
            old_root,
            node_mutations,
            new_pairs,
            new_root,
        } = mutations;

        // Guard against accidentally trying to apply mutations that were computed against a
        // different tree, including a stale version of this tree.
        if old_root != self.root() {
            return Err(MerkleError::ConflictingRoots {
                expected_root: self.root(),
                actual_root: old_root,
            });
        }

        for (index, mutation) in node_mutations {
            match mutation {
                Removal => self.remove_inner_node(index),
                Addition(node) => self.insert_inner_node(index, node),
            }
        }

        for (key, value) in new_pairs {
            self.insert_value(key, value);
        }

        self.set_root(new_root);

        Ok(())
    }

    // REQUIRED METHODS
    // ---------------------------------------------------------------------------------------------

    /// The root of the tree
    fn root(&self) -> RpoDigest;

    /// Sets the root of the tree
    fn set_root(&mut self, root: RpoDigest);

    /// Retrieves an inner node at the given index
    fn get_inner_node(&self, index: NodeIndex) -> InnerNode;

    /// Inserts an inner node at the given index
    fn insert_inner_node(&mut self, index: NodeIndex, inner_node: InnerNode);

    /// Removes an inner node at the given index
    fn remove_inner_node(&mut self, index: NodeIndex);

    /// Inserts a leaf node, and returns the value at the key if already exists
    fn insert_value(&mut self, key: Self::Key, value: Self::Value) -> Option<Self::Value>;

    /// Returns the value at the specified key. Recall that by definition, any key that hasn't been
    /// updated is associated with [`Self::EMPTY_VALUE`].
    fn get_value(&self, key: &Self::Key) -> Self::Value;

    /// Returns the leaf at the specified index.
    fn get_leaf(&self, key: &Self::Key) -> Self::Leaf;

    /// Returns the hash of a leaf
    fn hash_leaf(leaf: &Self::Leaf) -> RpoDigest;

    /// Returns what a leaf would look like if a key-value pair were inserted into the tree, without
    /// mutating the tree itself. The existing leaf can be empty.
    ///
    /// To get a prospective leaf based on the current state of the tree, use `self.get_leaf(key)`
    /// as the argument for `existing_leaf`. The return value from this function can be chained back
    /// into this function as the first argument to continue making prospective changes.
    ///
    /// # Invariants
    /// Because this method is for a prospective key-value insertion into a specific leaf,
    /// `existing_leaf` must have the same leaf index as `key` (as determined by
    /// [`SparseMerkleTree::key_to_leaf_index()`]), or the result will be meaningless.
    fn construct_prospective_leaf(
        &self,
        existing_leaf: Self::Leaf,
        key: &Self::Key,
        value: &Self::Value,
    ) -> Self::Leaf;

    /// Maps a key to a leaf index
    fn key_to_leaf_index(key: &Self::Key) -> LeafIndex<DEPTH>;

    /// Maps a (MerklePath, Self::Leaf) to an opening.
    ///
    /// The length `path` is guaranteed to be equal to `DEPTH`
    fn path_and_leaf_to_opening(path: MerklePath, leaf: Self::Leaf) -> Self::Opening;
}

// INNER NODE
// ================================================================================================

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub(crate) struct InnerNode {
    pub left: RpoDigest,
    pub right: RpoDigest,
}

impl InnerNode {
    pub fn hash(&self) -> RpoDigest {
        Rpo256::merge(&[self.left, self.right])
    }
}

// LEAF INDEX
// ================================================================================================

/// The index of a leaf, at a depth known at compile-time.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct LeafIndex<const DEPTH: u8> {
    index: NodeIndex,
}

impl<const DEPTH: u8> LeafIndex<DEPTH> {
    pub fn new(value: u64) -> Result<Self, MerkleError> {
        if DEPTH < SMT_MIN_DEPTH {
            return Err(MerkleError::DepthTooSmall(DEPTH));
        }

        Ok(LeafIndex { index: NodeIndex::new(DEPTH, value)? })
    }

    pub fn value(&self) -> u64 {
        self.index.value()
    }
}

impl LeafIndex<SMT_MAX_DEPTH> {
    pub const fn new_max_depth(value: u64) -> Self {
        LeafIndex {
            index: NodeIndex::new_unchecked(SMT_MAX_DEPTH, value),
        }
    }
}

impl<const DEPTH: u8> From<LeafIndex<DEPTH>> for NodeIndex {
    fn from(value: LeafIndex<DEPTH>) -> Self {
        value.index
    }
}

impl<const DEPTH: u8> TryFrom<NodeIndex> for LeafIndex<DEPTH> {
    type Error = MerkleError;

    fn try_from(node_index: NodeIndex) -> Result<Self, Self::Error> {
        if node_index.depth() != DEPTH {
            return Err(MerkleError::InvalidNodeIndexDepth {
                expected: DEPTH,
                provided: node_index.depth(),
            });
        }

        Self::new(node_index.value())
    }
}

// MUTATIONS
// ================================================================================================

/// A change to an inner node of a [`SparseMerkleTree`] that hasn't yet been applied.
/// [`MutationSet`] stores this type in relation to a [`NodeIndex`] to keep track of what changes
/// need to occur at which node indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeMutation {
    /// Corresponds to [`SparseMerkleTree::remove_inner_node()`].
    Removal,
    /// Corresponds to [`SparseMerkleTree::insert_inner_node()`].
    Addition(InnerNode),
}

/// Represents a group of prospective mutations to a `SparseMerkleTree`, created by
/// `SparseMerkleTree::compute_mutations()`, and that can be applied with
/// `SparseMerkleTree::apply_mutations()`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MutationSet<const DEPTH: u8, K, V> {
    /// The root of the Merkle tree this MutationSet is for, recorded at the time
    /// [`SparseMerkleTree::compute_mutations()`] was called. Exists to guard against applying
    /// mutations to the wrong tree or applying stale mutations to a tree that has since changed.
    old_root: RpoDigest,
    /// The set of nodes that need to be removed or added. The "effective" node at an index is the
    /// Merkle tree's existing node at that index, with the [`NodeMutation`] in this map at that
    /// index overlayed, if any. Each [`NodeMutation::Addition`] corresponds to a
    /// [`SparseMerkleTree::insert_inner_node()`] call, and each [`NodeMutation::Removal`]
    /// corresponds to a [`SparseMerkleTree::remove_inner_node()`] call.
    node_mutations: BTreeMap<NodeIndex, NodeMutation>,
    /// The set of top-level key-value pairs we're prospectively adding to the tree, including
    /// adding empty values. The "effective" value for a key is the value in this BTreeMap, falling
    /// back to the existing value in the Merkle tree. Each entry corresponds to a
    /// [`SparseMerkleTree::insert_value()`] call.
    new_pairs: BTreeMap<K, V>,
    /// The calculated root for the Merkle tree, given these mutations. Publicly retrievable with
    /// [`MutationSet::root()`]. Corresponds to a [`SparseMerkleTree::set_root()`]. call.
    new_root: RpoDigest,
}

impl<const DEPTH: u8, K, V> MutationSet<DEPTH, K, V> {
    /// Queries the root that was calculated during `SparseMerkleTree::compute_mutations()`. See
    /// that method for more information.
    pub fn root(&self) -> RpoDigest {
        self.new_root
    }
}
