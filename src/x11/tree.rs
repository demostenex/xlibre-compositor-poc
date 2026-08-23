use std::collections::HashSet;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, Window};

use super::capture::{
    is_bad_window_error, print_metadata, WindowHierarchy, WindowMetadata, WindowRole,
};
use super::connection::X11Connection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BindingStatus {
    NoClient,
    SingleClient(Window),
    Ambiguous(Vec<Window>),
}

pub(crate) fn classify_binding_status(candidates: &[Window]) -> BindingStatus {
    match candidates {
        [] => BindingStatus::NoClient,
        [client] => BindingStatus::SingleClient(*client),
        candidates => BindingStatus::Ambiguous(candidates.to_vec()),
    }
}

#[derive(Debug)]
pub(crate) struct HierarchyBinding {
    pub(crate) root_child_xid: Window,
    pub(crate) semantic_client_xids: Vec<Window>,
    pub(crate) semantic_client: BindingStatus,
    pub(crate) lifecycle_candidate_xid: Window,
    pub(crate) surface_candidate: Option<WindowMetadata>,
    pub(crate) descendants: Vec<WindowMetadata>,
    pub(crate) stale: bool,
}

#[derive(Debug)]
pub(crate) struct HierarchySnapshot {
    pub(crate) root: Window,
    pub(crate) children: Vec<HierarchyBinding>,
}

impl X11Connection {
    pub(crate) fn snapshot_hierarchy(&self) -> Result<HierarchySnapshot, Box<dyn Error>> {
        let root = self.inner.setup().roots[self.screen_num()].root;
        let root_tree = self.inner.query_tree(root)?.reply()?;
        let mut children = Vec::with_capacity(root_tree.children.len());

        for root_child in root_tree.children {
            match self.inspect_root_child(root, root_child) {
                Ok(binding) => children.push(binding),
                Err(error) if is_bad_window_error(error.as_ref()) => {
                    children.push(HierarchyBinding::stale(root_child));
                }
                Err(error) => return Err(error),
            }
        }

        Ok(HierarchySnapshot { root, children })
    }

    fn inspect_root_child(
        &self,
        root: Window,
        root_child: Window,
    ) -> Result<HierarchyBinding, Box<dyn Error>> {
        let hierarchy = WindowHierarchy {
            source: root_child,
            parent: Some(root),
            top_level: root_child,
            root,
        };
        let mut semantic_client_xids = Vec::new();
        let mut descendants = Vec::new();
        let mut visited = HashSet::from([root_child]);
        let mut pending = vec![(root_child, root)];
        let mut surface_candidate = None;
        let mut stale = false;

        while let Some((window, expected_parent)) = pending.pop() {
            let tree = match self.inner.query_tree(window)?.reply() {
                Ok(tree) if tree.root == root && tree.parent == expected_parent => tree,
                Ok(_) => {
                    stale = true;
                    continue;
                }
                Err(error) if is_bad_window_error(&error) => {
                    stale = true;
                    continue;
                }
                Err(error) => return Err(Box::new(error)),
            };

            let metadata = match self.window_metadata(window, hierarchy) {
                Ok(metadata) => metadata,
                Err(error) if is_bad_window_error(error.as_ref()) => {
                    stale = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if metadata.role == WindowRole::Client {
                semantic_client_xids.push(window);
            }
            if window == root_child {
                surface_candidate = Some(metadata);
            } else {
                descendants.push(metadata);
            }
            for child in tree.children {
                if visited.insert(child) {
                    pending.push((child, window));
                }
            }
        }

        Ok(HierarchyBinding {
            root_child_xid: root_child,
            semantic_client: classify_binding_status(&semantic_client_xids),
            semantic_client_xids,
            lifecycle_candidate_xid: root_child,
            surface_candidate,
            descendants,
            stale,
        })
    }
}

impl HierarchyBinding {
    fn stale(root_child_xid: Window) -> Self {
        Self {
            root_child_xid,
            semantic_client_xids: Vec::new(),
            semantic_client: BindingStatus::NoClient,
            lifecycle_candidate_xid: root_child_xid,
            surface_candidate: None,
            descendants: Vec::new(),
            stale: true,
        }
    }

    fn metadata_for(&self, window: Window) -> Option<&WindowMetadata> {
        self.surface_candidate
            .as_ref()
            .filter(|metadata| metadata.window == window)
            .or_else(|| self.descendants.iter().find(|metadata| metadata.window == window))
    }
}

pub(crate) fn print_snapshot(snapshot: &HierarchySnapshot) {
    println!("X11 global hierarchy snapshot");
    println!("root: 0x{:08x}", snapshot.root);
    println!("root children: {}", snapshot.children.len());
    println!("stacking: bottom -> top");

    for (index, binding) in snapshot.children.iter().enumerate() {
        let position = if index == 0 {
            "bottom"
        } else if index + 1 == snapshot.children.len() {
            "top"
        } else {
            ""
        };
        println!("\n[{index}] {position}");
        println!(
            "surface candidate: 0x{:08x}",
            binding.root_child_xid
        );
        if binding.stale {
            println!("snapshot status: STALE (window changed or disappeared)");
        }
        if let Some(metadata) = binding.surface_candidate.as_ref() {
            print_metadata("surface candidate metadata", metadata);
        }

        println!("semantic client:");
        match &binding.semantic_client {
            BindingStatus::NoClient => println!("status: none"),
            BindingStatus::SingleClient(client) => {
                println!("status: single");
                println!("xid: 0x{client:08x}");
                if let Some(metadata) = binding.metadata_for(*client) {
                    print_metadata("metadata", metadata);
                }
            }
            BindingStatus::Ambiguous(clients) => {
                println!("status: ambiguous");
                println!("candidates:");
                for client in clients {
                    println!("  0x{client:08x}");
                    if let Some(metadata) = binding.metadata_for(*client) {
                        print_metadata("  metadata", metadata);
                    }
                }
            }
        }
        println!(
            "semantic client candidates: {}",
            binding.semantic_client_xids.len()
        );
        println!(
            "lifecycle candidate: 0x{:08x}",
            binding.lifecycle_candidate_xid
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_binding_status, BindingStatus};

    #[test]
    fn no_semantic_client_is_explicit() {
        assert_eq!(
            classify_binding_status(&[]),
            BindingStatus::NoClient
        );
    }

    #[test]
    fn one_semantic_client_is_selected() {
        assert_eq!(
            classify_binding_status(&[10]),
            BindingStatus::SingleClient(10)
        );
    }

    #[test]
    fn multiple_semantic_clients_are_ambiguous() {
        assert_eq!(
            classify_binding_status(&[10, 20]),
            BindingStatus::Ambiguous(vec![10, 20])
        );
    }
}
