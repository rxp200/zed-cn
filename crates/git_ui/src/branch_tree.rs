//! IDEA 风格的分支文件夹树：把分支名按 `/` 前缀分组为可折叠的文件夹节点，
//! 分支选择器与 Git 历史图侧栏共用。只处理纯数据结构，不依赖 UI 组件。

use collections::HashSet;
use git::repository::Branch;
use gpui::SharedString;

/// Folders precede leaves; leaves retain the repository's branch priority order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BranchTreeNode {
    pub folders: Vec<BranchTreeFolder>,
    /// 指向传入分支切片的索引。
    pub leaves: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BranchTreeFolder {
    /// Display name, possibly combining a compact directory chain such as "dev/shared".
    pub name: SharedString,
    /// 完整路径前缀（不含末尾 `/`），用作折叠状态的稳定键。
    pub path: SharedString,
    pub children: BranchTreeNode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BranchTreeRowKind {
    Folder {
        name: SharedString,
        path: SharedString,
        expanded: bool,
    },
    Leaf {
        branch_index: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BranchTreeRow {
    pub kind: BranchTreeRowKind,
    pub depth: usize,
}

impl BranchTreeRow {
    pub fn is_folder(&self) -> bool {
        matches!(self.kind, BranchTreeRowKind::Folder { .. })
    }
}

/// 分支名的最后一段，用于树内展示。
pub fn leaf_name(full_name: &str) -> &str {
    full_name.rsplit('/').next().unwrap_or(full_name)
}

/// Groups slash-separated names, retaining leaf priority and compacting directory chains.
pub fn build_branch_tree(branches: &[Branch]) -> BranchTreeNode {
    let mut root = BranchTreeNode::default();
    for (index, branch) in branches.iter().enumerate() {
        let mut node = &mut root;
        let mut segments = branch.name().split('/').peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                node.leaves.push(index);
                break;
            }
            let position = node
                .folders
                .iter()
                .position(|folder| folder.name.as_ref() == segment);
            let folder_index = match position {
                Some(position) => position,
                None => {
                    node.folders.push(BranchTreeFolder {
                        name: segment.into(),
                        path: String::new().into(),
                        children: BranchTreeNode::default(),
                    });
                    node.folders.len() - 1
                }
            };
            node = &mut node.folders[folder_index].children;
        }
    }
    fill_paths(&mut root, "");
    let remote_roots: HashSet<SharedString> = branches
        .iter()
        .filter_map(|branch| branch.remote_name().map(SharedString::from))
        .collect();
    compress_single_child_chains(&mut root, &remote_roots);
    sort_node(&mut root);
    root
}

fn fill_paths(node: &mut BranchTreeNode, prefix: &str) {
    for folder in &mut node.folders {
        let path = if prefix.is_empty() {
            folder.name.to_string()
        } else {
            format!("{prefix}/{}", folder.name)
        };
        folder.path = path.into();
        let child_prefix = folder.path.to_string();
        fill_paths(&mut folder.children, &child_prefix);
    }
}

/// 把「没有叶子、只有一个子文件夹」的文件夹与子文件夹合并，
/// 例如 dev → shared → workflow 压缩为 dev/shared → workflow。
/// 根节点不压缩，远程名（origin 等）因此始终是独立的一层。
fn compress_single_child_chains(node: &mut BranchTreeNode, remote_roots: &HashSet<SharedString>) {
    for folder in &mut node.folders {
        compress_single_child_chains(&mut folder.children, remote_roots);
    }
    for folder in &mut node.folders {
        if remote_roots.contains(&folder.path) {
            continue;
        }
        while folder.children.leaves.is_empty() && folder.children.folders.len() == 1 {
            let child = folder.children.folders.remove(0);
            folder.name = format!("{}/{}", folder.name, child.name).into();
            folder.path = child.path;
            folder.children = child.children;
        }
    }
}

fn sort_node(node: &mut BranchTreeNode) {
    node.folders
        .sort_by_key(|folder| folder.name.to_lowercase());
    node.leaves.sort();
    for folder in &mut node.folders {
        sort_node(&mut folder.children);
    }
}

/// 把树按折叠集合展开为扁平行序列，深度从 0 开始。
pub fn flatten_branch_tree(
    root: &BranchTreeNode,
    collapsed: &HashSet<SharedString>,
) -> Vec<BranchTreeRow> {
    let mut rows = Vec::new();
    flatten_node(root, collapsed, 0, &mut rows);
    rows
}

fn flatten_node(
    node: &BranchTreeNode,
    collapsed: &HashSet<SharedString>,
    depth: usize,
    rows: &mut Vec<BranchTreeRow>,
) {
    for folder in &node.folders {
        let expanded = !collapsed.contains(&folder.path);
        rows.push(BranchTreeRow {
            kind: BranchTreeRowKind::Folder {
                name: folder.name.clone(),
                path: folder.path.clone(),
                expanded,
            },
            depth,
        });
        if expanded {
            flatten_node(&folder.children, collapsed, depth + 1, rows);
        }
    }
    for &branch_index in &node.leaves {
        rows.push(BranchTreeRow {
            kind: BranchTreeRowKind::Leaf { branch_index },
            depth,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str) -> Branch {
        let ref_name = if name.contains('/') && name.starts_with("origin/") {
            format!("refs/remotes/{name}")
        } else {
            format!("refs/heads/{name}")
        };
        Branch {
            is_head: false,
            ref_name: ref_name.into(),
            upstream: None,
            most_recent_commit: None,
        }
    }

    fn branches(names: &[&str]) -> Vec<Branch> {
        names.iter().map(|name| branch(name)).collect()
    }

    fn folder<'a>(node: &'a BranchTreeNode, name: &str) -> &'a BranchTreeFolder {
        node.folders
            .iter()
            .find(|folder| folder.name.as_ref() == name)
            .unwrap_or_else(|| panic!("folder {name} not found"))
    }

    #[test]
    fn groups_branches_by_slash_prefix() {
        let branches = branches(&["dev/demo", "dev/shared/workflow", "feat/login", "main"]);
        let tree = build_branch_tree(&branches);

        assert_eq!(tree.leaves.len(), 1);
        assert_eq!(tree.folders.len(), 2);
        let dev = folder(&tree, "dev");
        assert_eq!(dev.children.leaves.len(), 1);
        let shared = folder(&dev.children, "shared");
        assert_eq!(shared.children.leaves.len(), 1);
        assert!(folder(&tree, "feat").children.leaves.len() == 1);
    }

    #[test]
    fn compresses_single_child_folder_chains() {
        let branches = branches(&["dev/shared/workflow"]);
        let tree = build_branch_tree(&branches);

        assert_eq!(tree.folders.len(), 1);
        let compressed = folder(&tree, "dev/shared");
        assert_eq!(compressed.path.as_ref(), "dev/shared");
        assert_eq!(compressed.children.leaves.len(), 1);
    }

    #[test]
    fn does_not_compress_folder_with_leaves() {
        let branches = branches(&["dev/demo", "dev/shared/workflow"]);
        let tree = build_branch_tree(&branches);

        let dev = folder(&tree, "dev");
        assert_eq!(dev.name.as_ref(), "dev");
        assert!(folder(&dev.children, "shared").children.leaves.len() == 1);
    }

    #[test]
    fn remote_names_form_top_level_folders() {
        let branches = branches(&["origin/master", "origin/dev/demo", "upstream/main"]);
        let tree = build_branch_tree(&branches);

        assert_eq!(tree.folders.len(), 2);
        assert!(folder(&tree, "origin").children.leaves.len() == 1);
        assert!(folder(&tree, "upstream").children.leaves.len() == 1);
    }

    #[test]
    fn remote_root_is_not_compacted_into_branch_prefix() {
        let branches = branches(&["origin/dev/demo"]);
        let tree = build_branch_tree(&branches);
        let origin = folder(&tree, "origin");
        assert_eq!(origin.path.as_ref(), "origin");
        assert_eq!(folder(&origin.children, "dev").children.leaves, vec![0]);
    }

    #[test]
    fn flatten_respects_collapsed_paths() {
        let branches = branches(&["dev/demo", "dev/shared/workflow", "main"]);
        let tree = build_branch_tree(&branches);
        let expanded = flatten_branch_tree(&tree, &HashSet::default());
        assert_eq!(expanded.len(), 5);

        let mut collapsed = HashSet::default();
        collapsed.insert(SharedString::from("dev"));
        let flattened = flatten_branch_tree(&tree, &collapsed);
        assert_eq!(flattened.len(), 2);
        assert!(flattened[0].is_folder());
        assert_eq!(flattened[0].depth, 0);
        assert!(matches!(flattened[1].kind, BranchTreeRowKind::Leaf { .. }));
    }

    #[test]
    fn leaf_name_returns_last_segment() {
        assert_eq!(leaf_name("dev/shared/workflow"), "workflow");
        assert_eq!(leaf_name("main"), "main");
        assert_eq!(leaf_name("origin/dev/demo"), "demo");
    }

    #[test]
    fn folders_sort_before_leaves() {
        let branches = branches(&["zeta", "alpha/beta"]);
        let tree = build_branch_tree(&branches);
        let flattened = flatten_branch_tree(&tree, &HashSet::default());
        assert!(flattened[0].is_folder());
        assert!(matches!(
            flattened[2].kind,
            BranchTreeRowKind::Leaf { branch_index } if branch_index == 0
        ));
    }
}
