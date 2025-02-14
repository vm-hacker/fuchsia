// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect::Node;
use fuchsia_inspect_derive::{AttachError, Inspect, WithInspect};
use std::collections::HashMap;

/// A map that wraps an inspect node and attaches all inserted values to the node.
///
/// This class can either be explicitly given an inspect node through [ManagedInspectMap::with_node]
/// or can create its own inspect node when included in a struct that derives Inspect or when
/// [ManagedInspectMap::with_inspect] is called.
#[derive(Default)]
pub struct ManagedInspectMap<V> {
    map: HashMap<String, V>,
    node: Node,
}

impl<V> ManagedInspectMap<V>
where
    for<'a> &'a mut V: Inspect,
{
    /// Creates a new [ManagedInspectMap] that attaches inserted values to the given node.
    pub fn with_node(node: Node) -> Self {
        Self { map: HashMap::new(), node }
    }

    /// Returns a mutable reference to the underlying map. Clients should not insert values into the
    /// map through this reference.
    pub fn map_mut(&mut self) -> &mut HashMap<String, V> {
        &mut self.map
    }

    /// Inserts the given value into the map and attach it to the inspect tree. Returns the previous
    /// value with the given key, if any.
    // TODO(fxbug.dev/103390): remove allow once used.
    #[allow(dead_code)]
    pub(crate) fn insert(&mut self, key: String, value: V) -> Option<V> {
        // `with_inspect` will only return an error on types with interior mutability.
        let value_with_inspect =
            value.with_inspect(&self.node, &key).expect("Failed to attach new map entry");
        self.map.insert(key, value_with_inspect)
    }

    /// Returns a mutable reference to the value at the given key, inserting a value if not present.
    pub fn get_or_insert_with(&mut self, key: String, value: impl FnOnce() -> V) -> &mut V {
        let node = &self.node;
        self.map.entry(key.clone()).or_insert_with(|| {
            // `with_inspect` will only return an error on types with interior mutability.
            value().with_inspect(node, &key).expect("Failed to attach new map entry")
        })
    }
}

impl<V> Inspect for &mut ManagedInspectMap<V>
where
    for<'a> &'a mut V: Inspect,
{
    fn iattach(self, parent: &Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.node = parent.create_child(name.as_ref());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::managed_inspect_map::ManagedInspectMap;
    use fuchsia_inspect::{assert_data_tree, Inspector, Node};
    use fuchsia_inspect_derive::{IValue, Inspect, WithInspect};

    #[derive(Default, Inspect)]
    struct TestInspectWrapper {
        inspect_node: Node,
        pub test_map: ManagedInspectMap<IValue<String>>,
    }

    // Tests that inserting items into the map automatically records them in inspect.
    #[test]
    fn test_map_insert() {
        let inspector = Inspector::new();

        let mut map = ManagedInspectMap::<IValue<String>>::with_node(
            inspector.root().create_child("managed_node"),
        );

        let _ = map.insert("key1".to_string(), "value1".to_string().into());
        let _ = map.insert("key2".to_string(), "value2".to_string().into());

        assert_data_tree!(inspector, root: {
            managed_node: {
                "key1": "value1",
                "key2": "value2"
            }
        });
    }

    // Tests that removing items from the map automatically removes them from inspect.
    #[test]
    fn test_map_remove() {
        let inspector = Inspector::new();

        let mut map = ManagedInspectMap::<IValue<String>>::with_node(
            inspector.root().create_child("managed_node"),
        );

        let _ = map.insert("key1".to_string(), "value1".to_string().into());
        let _ = map.insert("key2".to_string(), "value2".to_string().into());

        let _ = map.map_mut().remove(&"key1".to_string());

        assert_data_tree!(inspector, root: {
            managed_node: {
                "key2": "value2"
            }
        });
    }

    // Tests that the map automatically attaches itself to the inspect hierarchy when used as a
    // field in a struct that derives Inspect.
    #[test]
    fn test_map_derive_inspect() {
        let inspector = Inspector::new();

        let mut wrapper = TestInspectWrapper::default()
            .with_inspect(inspector.root(), "wrapper_node")
            .expect("Failed to attach wrapper_node");

        let _ = wrapper.test_map.insert("key1".to_string(), "value1".to_string().into());

        // The map's node is named test_map since that's the field name in TestInspectWrapper.
        assert_data_tree!(inspector, root: {
            wrapper_node: {
                test_map: {
                    "key1": "value1",
                }
            }
        });
    }
}
