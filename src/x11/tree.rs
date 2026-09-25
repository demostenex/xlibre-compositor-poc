use std::collections::HashSet;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, Window};

use super::capture::{
    is_bad_window_error, is_stale_hierarchy_metadata_error, print_metadata, WindowHierarchy,
    WindowMetadata, WindowRole,
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

fn stale_root_child_binding(
    root_child: Window,
    error: &(dyn Error + 'static),
) -> Option<HierarchyBinding> {
    is_bad_window_error(error).then(|| HierarchyBinding::stale(root_child))
}

fn initial_root_child_metadata<T>(
    result: Result<T, Box<dyn Error>>,
) -> Result<Option<T>, Box<dyn Error>> {
    match result {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if is_stale_hierarchy_metadata_error(error.as_ref()) => Ok(None),
        Err(error) => Err(error),
    }
}

fn descendant_metadata<T>(result: Result<T, Box<dyn Error>>) -> Result<Option<T>, Box<dyn Error>> {
    match result {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if is_stale_hierarchy_metadata_error(error.as_ref()) => Ok(None),
        Err(error) => Err(error),
    }
}

impl X11Connection {
    pub(crate) fn snapshot_hierarchy(&self) -> Result<HierarchySnapshot, Box<dyn Error>> {
        let root = self.inner.setup().roots[self.screen_num()].root;
        let root_tree = self.inner.query_tree(root)?.reply()?;
        let mut children = Vec::with_capacity(root_tree.children.len());

        for root_child in root_tree.children {
            match self.inspect_root_child(root, root_child) {
                Ok(binding) => children.push(binding),
                Err(error) => match stale_root_child_binding(root_child, error.as_ref()) {
                    Some(binding) => children.push(binding),
                    None => return Err(error),
                },
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

        let surface_candidate = match initial_root_child_metadata(
            self.window_metadata(root_child, hierarchy),
        )? {
            Some(metadata) => metadata,
            None => return Ok(HierarchyBinding::stale(root_child)),
        };

        let root_tree = match self.inner.query_tree(root_child)?.reply() {
            Ok(tree) if tree.root == root && tree.parent == root => tree,
            Ok(_) => return Ok(HierarchyBinding::stale(root_child)),
            Err(error) if is_bad_window_error(&error) => {
                return Ok(HierarchyBinding::stale(root_child));
            }
            Err(error) => return Err(Box::new(error)),
        };

        let mut semantic_client_xids = Vec::new();
        if surface_candidate.role == WindowRole::Client {
            semantic_client_xids.push(root_child);
        }
        let mut descendants = Vec::new();
        let mut visited = HashSet::from([root_child]);
        let mut pending = root_tree
            .children
            .into_iter()
            .map(|child| (child, root_child))
            .collect::<Vec<_>>();
        let surface_candidate = Some(surface_candidate);
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

            let metadata = match descendant_metadata(self.window_metadata(window, hierarchy))? {
                Some(metadata) => metadata,
                None => {
                    stale = true;
                    continue;
                }
            };
            if metadata.role == WindowRole::Client {
                semantic_client_xids.push(window);
            }
            descendants.push(metadata);
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
    use super::{
        classify_binding_status, descendant_metadata, initial_root_child_metadata,
        stale_root_child_binding, BindingStatus,
    };
    use std::error::Error;
    use x11rb::errors::ReplyError;
    use x11rb::protocol::ErrorKind;
    use x11rb::x11_utils::X11Error;

    fn reply_error(kind: ErrorKind) -> Box<dyn Error> {
        Box::new(ReplyError::X11Error(X11Error {
            error_kind: kind,
            error_code: 9,
            sequence: 1,
            bad_value: 42,
            minor_opcode: 0,
            major_opcode: 14,
            extension_name: None,
            request_name: Some("GetGeometry"),
        }))
    }

    #[test]
    fn initial_root_child_metadata_bad_drawable_and_bad_window_are_stale() {
        for kind in [ErrorKind::Drawable, ErrorKind::Window] {
            assert!(initial_root_child_metadata::<()>(Err(reply_error(kind)))
                .expect("stale metadata must not propagate")
                .is_none());
        }
    }

    #[test]
    fn initial_root_child_metadata_bad_match_propagates() {
        let error = initial_root_child_metadata::<()>(Err(reply_error(ErrorKind::Match)))
            .expect_err("unrelated X11 errors must propagate");
        assert!(matches!(
            error.downcast_ref::<ReplyError>(),
            Some(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Match
        ));
    }

    #[test]
    fn descendant_metadata_bad_drawable_and_bad_window_mark_stale() {
        for kind in [ErrorKind::Drawable, ErrorKind::Window] {
            assert!(descendant_metadata::<()>(Err(reply_error(kind)))
                .expect("stale descendant metadata must not propagate")
                .is_none());
        }
    }

    #[test]
    fn query_tree_drawable_error_is_not_reclassified_as_metadata_stale() {
        let drawable = reply_error(ErrorKind::Drawable);
        assert!(stale_root_child_binding(42, drawable.as_ref()).is_none());
    }

    #[test]
    fn successful_initial_root_child_metadata_is_preserved() {
        assert_eq!(
            initial_root_child_metadata(Ok("metadata")).unwrap(),
            Some("metadata")
        );
    }

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
